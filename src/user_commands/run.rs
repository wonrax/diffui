//! Running a declared command: ask its prompts and its confirm, read the
//! variables that come from the repository, then run it on the tab it was
//! started from.
//!
//! A program runs through [`diffui_core::process::stream`] with the launch
//! `PATH` (see [`crate::launch_env`]), its timeout, and a cancel the output
//! panel's Stop (or the description editor's) raises. Its lines land in an
//! activity row and the output panel the way a jj CLI run's do; what it
//! prints goes where `output` says once it exits, or into the description
//! editor as it arrives.

use std::sync::Arc;

use diffui_core::cli::{CliLine, CliStream};
use diffui_core::repo::CancelFlag;
use futures::StreamExt as _;

use super::template::{ARG_DIFF_LIMIT, Template, Values, Var};
use super::{Output, PromptKind, Step, UserCommand, context_values};
use crate::activity::ActivityId;
use crate::cli_output::Exit;
use crate::commands::CommandArg;
use crate::prose::Prose;
use crate::{Action, Cx, Diffui, TabId};

/// One run of a declared command, while it waits for an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RunId {
    pub(crate) tab: TabId,
    serial: u64,
}

/// The run waiting on the user (a prompt, the confirm) or on the repository
/// (its diff). One at a time: starting another drops it.
#[derive(Debug, Default)]
pub(crate) struct Runs {
    next: u64,
    waiting: Option<Waiting>,
}

#[derive(Debug)]
struct Waiting {
    id: RunId,
    command: Arc<UserCommand>,
    arg: CommandArg,
    values: Values,
    /// The prompts answered so far.
    answered: usize,
    confirmed: bool,
    fetching: bool,
}

/// A program writing into the description editor: what Stop raises, and the
/// text to put back if it fails. Kept once it has exited, so what it wrote
/// stays on the field's undo stack until the editor closes.
#[derive(Debug, Clone)]
pub(crate) struct Generation {
    pub(crate) activity: ActivityId,
    pub(crate) cancel: CancelFlag,
    pub(crate) before: String,
    /// Still writing: the field is read-only.
    pub(crate) running: bool,
}

/// What a running program reports.
enum Event {
    Line(CliLine),
    Exit(std::io::Result<std::process::ExitStatus>),
}

/// A program that has started: where what it reports goes.
struct Running {
    tab: TabId,
    job: diffui_core::JobId,
    activity: ActivityId,
    label: String,
    program: String,
    output: Output,
}

/// A program to run: everything rendered, nothing left to ask.
struct Launch {
    tab: TabId,
    label: String,
    argv: Vec<String>,
    stdin: Option<String>,
    output: Output,
    timeout: std::time::Duration,
    /// The revision `output = "description"` writes into.
    target: Option<diffui_core::RevisionSelection>,
}

impl Diffui {
    /// Start `command` on `arg`, on `tab` (the active tab when it was built).
    pub(crate) fn start_user_command(
        &mut self,
        command: Arc<UserCommand>,
        tab: TabId,
        arg: CommandArg,
        cx: &mut Cx,
    ) {
        if !self.is_active(tab) {
            return;
        }
        let values = context_values(self, &arg, &command.vars());
        self.user_runs.next += 1;
        let id = RunId {
            tab,
            serial: self.user_runs.next,
        };
        self.user_runs.waiting = Some(Waiting {
            id,
            command,
            arg,
            values,
            answered: 0,
            confirmed: false,
            fetching: false,
        });
        self.advance_user_run(cx);
    }

    /// Ask the next question, read the next value, or run.
    fn advance_user_run(&mut self, cx: &mut Cx) {
        let Some(run) = self.user_runs.waiting.as_ref() else {
            return;
        };
        let id = run.id;
        let command = run.command.clone();
        if let Some(prompt) = command.prompts.get(run.answered) {
            match &prompt.kind {
                PromptKind::Input => self.ask_input(id, &prompt.label, cx),
                PromptKind::Confirm => self.ask(crate::ConfirmDialog {
                    title: Prose::from(prompt.label.clone()),
                    body: Prose::new(),
                    confirm_label: "Continue".to_owned(),
                    look: crate::ConfirmLook {
                        routine: true,
                        ..Default::default()
                    },
                    pending: crate::Held::Command(id),
                }),
                PromptKind::Choose(choices) => {
                    self.open_choices(id, &prompt.label, choices.clone(), cx)
                }
            }
            return;
        }
        if let Some(confirm) = &command.confirm
            && !run.confirmed
        {
            let Ok(text) = confirm.render(&run.values) else {
                return self.drop_user_run(id, "a value it names went missing");
            };
            self.ask(crate::ConfirmDialog {
                title: Prose::from_backticks(&text),
                body: Prose::new(),
                confirm_label: command.label.clone(),
                look: crate::ConfirmLook {
                    routine: true,
                    ..Default::default()
                },
                pending: crate::Held::Command(id),
            });
            return;
        }
        let missing: Vec<Var> = command
            .vars()
            .into_iter()
            .filter(|var| var.is_fetched() && !run.values.contains_key(var))
            .cloned()
            .collect();
        if !missing.is_empty() {
            if run.fetching {
                return;
            }
            return self.fetch_values(id, missing, cx);
        }
        let Some(run) = self.user_runs.waiting.take() else {
            return;
        };
        self.launch_user_run(run, cx);
    }

    /// The prompt in front of `id` was answered (`None`: cancelled).
    pub(crate) fn user_prompt_answered(&mut self, id: RunId, answer: Option<String>, cx: &mut Cx) {
        let Some(run) = self.user_runs.waiting.as_mut().filter(|run| run.id == id) else {
            return;
        };
        let Some(answer) = answer else {
            self.user_runs.waiting = None;
            return;
        };
        let Some(prompt) = run.command.prompts.get(run.answered) else {
            return;
        };
        run.values.insert(Var::Prompt(prompt.name.clone()), answer);
        run.answered += 1;
        self.advance_user_run(cx);
    }

    /// The confirm dialog in front of `id` was answered: a confirm prompt,
    /// or the command's own `confirm`.
    pub(crate) fn user_run_confirmed(&mut self, id: RunId, accepted: bool, cx: &mut Cx) {
        let Some(run) = self.user_runs.waiting.as_mut().filter(|run| run.id == id) else {
            return;
        };
        if !accepted {
            self.user_runs.waiting = None;
            return;
        }
        if run.answered < run.command.prompts.len() {
            return self.user_prompt_answered(id, Some("true".to_owned()), cx);
        }
        run.confirmed = true;
        self.advance_user_run(cx);
    }

    fn drop_user_run(&mut self, id: RunId, why: &str) {
        if let Some(run) = self.user_runs.waiting.take_if(|run| run.id == id) {
            self.push_error_toast(format!("{} didn't run", run.command.label), why);
        }
    }

    /// Read the variables that come from the repository, then carry on.
    fn fetch_values(&mut self, id: RunId, vars: Vec<Var>, cx: &mut Cx) {
        let Some(run) = self.user_runs.waiting.as_ref() else {
            return;
        };
        let commit = run
            .values
            .get(&Var::CommitId)
            .cloned()
            .or_else(|| super::value(self, &run.arg, &Var::CommitId).ok().flatten());
        let Some(repository) = self.tab(id.tab).and_then(|tab| tab.repository.clone()) else {
            return self.drop_user_run(id, "the repository went away");
        };
        if let Some(run) = self.user_runs.waiting.as_mut() {
            run.fetching = true;
        }
        let work = async move {
            let mut values = Values::new();
            for var in vars {
                let value = match var {
                    Var::Diff => {
                        let Some(commit) = &commit else {
                            return Err("No revision here".to_owned());
                        };
                        read_diff(&repository, commit).await?
                    }
                    Var::RecentDescriptions => read_recent_descriptions(&repository).await?,
                    _ => continue,
                };
                values.insert(var, value);
            }
            Ok(values)
        };
        Self::run_async(
            work,
            cx,
            move |this, fetched: Result<Values, String>, cx| {
                let Some(run) = this.user_runs.waiting.as_mut().filter(|run| run.id == id) else {
                    return;
                };
                run.fetching = false;
                match fetched {
                    Ok(values) => {
                        run.values.extend(values);
                        this.advance_user_run(cx);
                    }
                    Err(error) => this.drop_user_run(id, &error),
                }
            },
        );
    }

    fn launch_user_run(&mut self, run: Waiting, cx: &mut Cx) {
        let Waiting {
            id,
            command,
            arg,
            values,
            ..
        } = run;
        let render = |template: &Template| {
            template
                .render(&values)
                .map_err(|var| format!("{{{}}} has no value here", var.name()))
        };
        let rendered: Result<Vec<String>, String> = match &command.step {
            Step::Exec(words) | Step::Run(words) => words.iter().map(render).collect(),
            Step::Agent(prompt) => render(prompt).and_then(|prompt| {
                let mut argv = agent_command(self.user_config.ai.as_deref())?;
                argv.push(prompt);
                Ok(argv)
            }),
            Step::Edit(path) => render(path).and_then(|path| {
                let root = values.get(&Var::Root).ok_or("No local repository here")?;
                let line = values.get(&Var::Line).map_or("1", String::as_str);
                let file = std::path::Path::new(root).join(path);
                Ok(editor_command(
                    self.user_config.editor.as_deref(),
                    &file.display().to_string(),
                    line,
                ))
            }),
        };
        let stdin = command.stdin.as_ref().map(render).transpose();
        let (argv, stdin) = match (rendered, stdin) {
            (Ok(argv), Ok(stdin)) => (argv, stdin),
            (Err(error), _) | (_, Err(error)) => {
                return self.push_error_toast(format!("{} didn't run", command.label), &error);
            }
        };
        if !matches!(command.step, Step::Run(_))
            && values
                .get(&Var::Diff)
                .is_some_and(|diff| diff.len() > ARG_DIFF_LIMIT)
            && command
                .templates_in_argv()
                .any(|template| template.vars().any(|var| *var == Var::Diff))
        {
            return self.push_error_toast(
                format!("{} didn't run", command.label),
                "The diff is too big for an argument; pass it with stdin = \"{diff}\"",
            );
        }
        if let Step::Run(_) = command.step {
            let line = jj_line(&argv);
            let label = command.label.clone();
            let reply: crate::cmdline::LineReply = Box::new(move |ui, outcome| {
                if let Err(error) = outcome {
                    ui.push_error_toast(format!("{label} didn't run"), &error.0);
                }
            });
            if let Err(error) = self.run_jj_line(id.tab, &line, Some(reply), cx) {
                self.push_error_toast(format!("{} didn't run", command.label), &error.0);
            }
            return;
        }
        let target = matches!(command.output, Output::Description)
            .then(|| crate::commands::target_revision(self, &arg))
            .flatten();
        let output = match command.step {
            Step::Edit(_) => Output::None,
            _ => command.output,
        };
        self.spawn_program(
            Launch {
                tab: id.tab,
                label: command.label.clone(),
                argv,
                stdin,
                output,
                timeout: command.timeout,
                target,
            },
            cx,
        );
    }

    fn spawn_program(&mut self, launch: Launch, cx: &mut Cx) {
        let Launch {
            tab,
            label,
            argv,
            stdin,
            output,
            timeout,
            target,
        } = launch;
        let Some(root) = self
            .tab(tab)
            .and_then(|state| state.repository.as_ref())
            .map(|repository| repository.root.clone())
        else {
            return self
                .push_error_toast(format!("{label} didn't run"), "No local repository here");
        };
        let Some((program, args)) = argv.split_first() else {
            return;
        };
        let (activity, _) = self.begin_activity(tab, Prose::from(label.clone()), false);
        let cancel = CancelFlag::default();
        if output == Output::Description
            && let Some(target) = target
        {
            self.perform(
                Action::EditDescription {
                    target: Some(target),
                },
                cx,
            );
            if let Some(editor) = self
                .tab_mut(tab)
                .and_then(|state| state.description_editor_mut())
            {
                editor.generating = Some(Generation {
                    activity,
                    cancel: cancel.clone(),
                    before: std::mem::take(&mut editor.content),
                    running: true,
                });
            }
        }
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let job = state.session.next_job();
        crate::cli_output::keep_run(
            &mut state.cmdline.runs,
            crate::cli_output::CliRun::program(
                activity,
                job,
                program.clone(),
                args.to_vec(),
                cancel.clone(),
            ),
        );
        if let Some(log) = self.activity_log_for(tab) {
            log.set_command(activity, Some(display_argv(&argv)));
        }

        let (sender, events) = futures::channel::mpsc::unbounded();
        let mut process = tokio::process::Command::new(program);
        process.args(args).current_dir(&root);
        crate::spawn::runtime().spawn(async move {
            let lines = sender.clone();
            let status = diffui_core::process::stream(
                &mut process,
                stdin.map(String::into_bytes),
                Some(timeout),
                Some(&cancel),
                move |line| {
                    let _ = lines.unbounded_send(Event::Line(line));
                },
            )
            .await;
            let _ = sender.unbounded_send(Event::Exit(status));
        });
        let running = Running {
            tab,
            job,
            activity,
            label,
            program: program.clone(),
            output,
        };
        cx.spawn(async move |this, cx| {
            let mut events = events.ready_chunks(256);
            while let Some(batch) = events.next().await {
                let updated = this.update(cx, |this, cx| {
                    this.act(cx, |this, cx| {
                        for event in batch {
                            this.on_program_event(&running, event, cx);
                        }
                    })
                });
                if updated.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn on_program_event(&mut self, running: &Running, event: Event, cx: &mut Cx) {
        let Running {
            tab,
            job,
            activity,
            ref label,
            ref program,
            output,
        } = *running;
        match event {
            Event::Line(line) => {
                if output == Output::Description
                    && line.stream == CliStream::Stdout
                    && let Some(editor) = self.generating_editor(tab, activity)
                {
                    // Blank lines wait for text after them, so a trailing
                    // one never lands.
                    match line.text.trim_end() {
                        "" if editor.content.is_empty() => {}
                        "" => editor.content.push('\n'),
                        text => {
                            if !editor.content.is_empty() && !editor.content.ends_with("\n\n") {
                                editor.content.push('\n');
                            }
                            editor.content.push_str(text);
                        }
                    }
                }
                self.cli_output(tab, job, std::slice::from_ref(&line));
            }
            Event::Exit(status) => {
                let stdout = self.program_stdout(tab, job);
                let exit = match &status {
                    Ok(status) => Exit::Code(status.code()),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                        Exit::Cancelled
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Exit::Failed(if program.contains('/') {
                            format!("`{program}` doesn't exist")
                        } else {
                            format!("`{program}` isn't on PATH")
                        })
                    }
                    Err(error) => Exit::Failed(error.to_string()),
                };
                self.finish_cli_run(tab, job, activity, exit.clone());
                let ok = exit == Exit::Code(Some(0));
                if let Some(editor) = self.generating_editor(tab, activity)
                    && let Some(generation) = editor.generating.as_mut()
                {
                    generation.running = false;
                    if ok {
                        editor.content = editor.content.trim_end().to_owned();
                    } else {
                        editor.content = generation.before.clone();
                    }
                }
                if !ok {
                    if exit != Exit::Cancelled {
                        let why = match &exit {
                            Exit::Failed(error) => error.clone(),
                            _ => self
                                .program_last_error(tab, job)
                                .unwrap_or_else(|| "It exited with an error".to_owned()),
                        };
                        self.push_error_toast(format!("{label} failed"), &why);
                    }
                    return;
                }
                let text = stdout.trim_end().to_owned();
                match output {
                    Output::Toast if !text.is_empty() => {
                        self.push_info_toast(label.to_owned(), &text)
                    }
                    Output::Clipboard => crate::write_clipboard(text, cx),
                    Output::OpenUrl => match text.lines().next().map(str::trim) {
                        Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
                            crate::open_url(url)
                        }
                        _ => self.push_error_toast(
                            format!("{label} printed no link"),
                            text.lines().next().unwrap_or(""),
                        ),
                    },
                    Output::Activity | Output::Toast | Output::Description | Output::None => {}
                }
            }
        }
    }

    /// The description editor `activity` is writing into, on `tab`.
    fn generating_editor(
        &mut self,
        tab: TabId,
        activity: ActivityId,
    ) -> Option<&mut crate::DescriptionEditor> {
        self.tab_mut(tab)?
            .description_editor_mut()
            .filter(|editor| {
                editor
                    .generating
                    .as_ref()
                    .is_some_and(|generation| generation.activity == activity)
            })
    }

    fn program_stdout(&self, tab: TabId, job: diffui_core::JobId) -> String {
        let Some(run) = self.cli_run(tab, job) else {
            return String::new();
        };
        let mut out = String::new();
        for line in run
            .lines
            .iter()
            .filter(|line| line.stream == CliStream::Stdout)
        {
            out.push_str(&line.text);
            out.push('\n');
        }
        out
    }

    fn program_last_error(&self, tab: TabId, job: diffui_core::JobId) -> Option<String> {
        self.cli_run(tab, job)?
            .lines
            .iter()
            .rev()
            .find(|line| line.stream == CliStream::Stderr && !line.text.trim().is_empty())
            .map(|line| line.text.trim().to_owned())
    }

    /// Stop the program writing into the active tab's description editor.
    pub(crate) fn stop_generating(&mut self) {
        if let Some(generation) = self
            .active()
            .description_editor()
            .and_then(|editor| editor.generating.as_ref())
            .filter(|generation| generation.running)
        {
            generation.cancel.cancel();
        }
    }
}

impl UserCommand {
    /// The templates that end up as arguments rather than stdin.
    fn templates_in_argv(&self) -> impl Iterator<Item = &Template> {
        let words: Vec<&Template> = match &self.step {
            Step::Exec(words) | Step::Run(words) => words.iter().collect(),
            Step::Agent(word) | Step::Edit(word) => vec![word],
        };
        words.into_iter()
    }
}

/// A `run` row's rendered words as the jj line the palette would read: each
/// word quoted so the parse splits it exactly where the row did.
pub(crate) fn jj_line(words: &[String]) -> String {
    std::iter::once("jj".to_owned())
        .chain(
            words
                .iter()
                .map(|word| diffui_core::cli::tokenize::shell_quote(word)),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

/// `argv` as one line for the activity row, each long word cut short.
fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            let word = word.lines().next().unwrap_or("");
            let short: String = word.chars().take(40).collect();
            let short = if short.len() < word.len() {
                format!("{short}\u{2026}")
            } else {
                short
            };
            diffui_core::cli::tokenize::shell_quote(&short)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The jj CLI this binary embeds, as a child process.
fn jj() -> tokio::process::Command {
    #[cfg(test)]
    return tokio::process::Command::new(crate::harness::fixture_jj());
    #[cfg(not(test))]
    {
        let exe = std::env::current_exe().unwrap_or_else(|_| "diffui".into());
        let mut command = tokio::process::Command::new(exe);
        command.arg("jj");
        command
    }
}

async fn output_of(mut command: tokio::process::Command, what: &str) -> Result<String, String> {
    let output = command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|error| format!("Couldn't read {what}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Couldn't read {what}: {}",
            stderr.lines().next().unwrap_or("it failed")
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `commit`'s diff in git format.
async fn read_diff(repository: &crate::Repository, commit: &str) -> Result<String, String> {
    let root = repository.root.display().to_string();
    let command = match repository.vcs {
        crate::Vcs::Jj => {
            let mut command = jj();
            command.args([
                "--ignore-working-copy",
                "--color=never",
                "-R",
                &root,
                "diff",
                "--git",
                "-r",
                commit,
            ]);
            command
        }
        crate::Vcs::Git => {
            let mut command = tokio::process::Command::new("git");
            command.args([
                "-C",
                &root,
                "show",
                "--format=",
                "--patch",
                "--no-color",
                commit,
            ]);
            command
        }
    };
    output_of(command, "the diff").await
}

/// The ten latest descriptions below the working copy.
async fn read_recent_descriptions(repository: &crate::Repository) -> Result<String, String> {
    let root = repository.root.display().to_string();
    let mut command = jj();
    command.args([
        "--ignore-working-copy",
        "--color=never",
        "-R",
        &root,
        "log",
        "--no-graph",
        "-r",
        "latest(::@- ~ root() ~ description(exact:\"\"), 10)",
        "-T",
        "description ++ \"---\\n\"",
    ]);
    output_of(command, "recent descriptions").await
}

/// Whether `program` is a file on `PATH`.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// The agent CLI's argv, before the prompt: `[ai] command`, else `claude -p`,
/// else `codex exec`.
fn agent_command(configured: Option<&[String]>) -> Result<Vec<String>, String> {
    if let Some(argv) = configured.filter(|argv| !argv.is_empty()) {
        return Ok(argv.to_vec());
    }
    for argv in [["claude", "-p"], ["codex", "exec"]] {
        if on_path(argv[0]) {
            return Ok(argv.iter().map(|word| (*word).to_owned()).collect());
        }
    }
    Err("No agent CLI: install claude or codex, or set [ai] command in config.toml".to_owned())
}

/// Editors that open a window of their own, so they can run without a
/// terminal, with how each takes a line.
const GUI_EDITORS: &[(&str, LineStyle)] = &[
    ("code", LineStyle::Goto),
    ("code-insiders", LineStyle::Goto),
    ("codium", LineStyle::Goto),
    ("cursor", LineStyle::Goto),
    ("windsurf", LineStyle::Goto),
    ("zed", LineStyle::Suffix),
    ("zeditor", LineStyle::Suffix),
    ("subl", LineStyle::Suffix),
    ("idea", LineStyle::LineFlag),
    ("rustrover", LineStyle::LineFlag),
    ("clion", LineStyle::LineFlag),
    ("goland", LineStyle::LineFlag),
    ("pycharm", LineStyle::LineFlag),
    ("webstorm", LineStyle::LineFlag),
    ("fleet", LineStyle::LineFlag),
    ("mate", LineStyle::DashL),
    ("nova", LineStyle::Plain),
    ("bbedit", LineStyle::Plain),
];

/// How an editor is told the line to open at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineStyle {
    /// `code -g path:line`
    Goto,
    /// `zed path:line`
    Suffix,
    /// `idea --line line path`
    LineFlag,
    /// `mate -l line path`
    DashL,
    /// The path alone.
    Plain,
}

fn line_style(program: &str) -> Option<LineStyle> {
    let name = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    GUI_EDITORS
        .iter()
        .find(|(editor, _)| *editor == name)
        .map(|(_, style)| *style)
}

/// The argv that opens `path` at `line`: `[editor] command`, else `$VISUAL`
/// or `$EDITOR` when it's a GUI editor, else the system's opener.
fn editor_command(configured: Option<&[String]>, path: &str, line: &str) -> Vec<String> {
    let from_env = || {
        ["VISUAL", "EDITOR"].iter().find_map(|var| {
            let value = std::env::var(var).ok()?;
            let argv: Vec<String> = value
                .split_whitespace()
                // Waiting for the window to close would hold the run open.
                .filter(|word| !matches!(*word, "--wait" | "-w"))
                .map(str::to_owned)
                .collect();
            line_style(argv.first()?).map(|_| argv)
        })
    };
    let base = configured
        .filter(|argv| !argv.is_empty())
        .map(<[String]>::to_vec)
        .or_else(from_env);
    let Some(mut argv) = base else {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        return vec![opener.to_owned(), path.to_owned()];
    };
    let style = argv
        .first()
        .and_then(|program| line_style(program))
        .unwrap_or(LineStyle::Plain);
    match style {
        LineStyle::Goto => argv.extend(["-g".to_owned(), format!("{path}:{line}")]),
        LineStyle::Suffix => argv.push(format!("{path}:{line}")),
        LineStyle::LineFlag => argv.extend(["--line".to_owned(), line.to_owned(), path.to_owned()]),
        LineStyle::DashL => argv.extend(["-l".to_owned(), line.to_owned(), path.to_owned()]),
        LineStyle::Plain => argv.push(path.to_owned()),
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_editor_gets_the_line_its_way() {
        let argv = |editor: &[&str]| {
            let editor: Vec<String> = editor.iter().map(|w| (*w).to_owned()).collect();
            editor_command(Some(&editor), "/r/a b.rs", "7")
        };
        assert_eq!(argv(&["code"]), ["code", "-g", "/r/a b.rs:7"]);
        assert_eq!(
            argv(&["/usr/local/bin/zed"]),
            ["/usr/local/bin/zed", "/r/a b.rs:7"]
        );
        assert_eq!(argv(&["subl"]), ["subl", "/r/a b.rs:7"]);
        assert_eq!(argv(&["idea"]), ["idea", "--line", "7", "/r/a b.rs"]);
        assert_eq!(argv(&["mate"]), ["mate", "-l", "7", "/r/a b.rs"]);
        assert_eq!(
            argv(&["my-editor", "--new"]),
            ["my-editor", "--new", "/r/a b.rs"]
        );
    }

    #[test]
    fn a_configured_agent_wins() {
        let configured = vec!["llm".to_owned(), "-m".to_owned(), "x".to_owned()];
        assert_eq!(agent_command(Some(&configured)), Ok(configured.clone()));
    }

    #[test]
    fn long_arguments_are_cut_for_the_row() {
        let prompt = "x".repeat(100);
        let shown = display_argv(&["claude".to_owned(), "-p".to_owned(), prompt]);
        assert!(shown.starts_with("claude -p "), "{shown}");
        assert!(shown.chars().count() < 60, "{shown}");
    }
}

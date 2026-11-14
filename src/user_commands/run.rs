//! Running a declared command: ask its prompts and its confirm, read the
//! variables that come from the repository, then run it on the tab it was
//! started from.
//!
//! A program runs through [`diffui_core::process::stream`] with the launch
//! `PATH` (see [`crate::launch_env`]), its timeout, and a cancel the output
//! panel's Stop (or the description editor's) raises. An `agent` row goes to
//! the AI provider ([`crate::ai`]): the custom command runs as a program, a
//! provider plugin streams its text back. Either way the text lands in an
//! activity row and the output panel the way a jj CLI run's does, and goes
//! where `output` says: once it ends, or as it arrives for the description
//! editor and the pull request dialog.

use std::sync::Arc;

use diffui_core::cli::{CliLine, CliStream};
use diffui_core::repo::CancelFlag;
use futures::StreamExt as _;

use super::template::{ARG_DIFF_LIMIT, Template, Values, Var};
use super::{Output, PromptKind, Step, UserCommand, context_values};
use crate::activity::ActivityId;
use crate::ai::Provider;
use crate::cli_output::Exit;
use crate::commands::CommandArg;
use crate::modes::{Mode, ModeKind};
use crate::plugins::{Ended, Streamed};
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

impl Runs {
    /// An id for something that asks the user on `tab` without being a
    /// declared command's run: a plugin's prompt.
    pub(crate) fn mint(&mut self, tab: TabId) -> RunId {
        self.next += 1;
        RunId {
            tab,
            serial: self.next,
        }
    }
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
    /// A diff it reads was cut at [`DIFF_LIMIT`].
    cut: bool,
}

/// The most of a diff a command is given; past it the diff is cut, with a
/// line that says so.
const DIFF_LIMIT: usize = 200 * 1024;

/// A run writing into the description editor (or the pull request dialog,
/// or the bookmark name prompt): what Stop raises, and the text to put back
/// if it fails. Kept once it has exited, so what it wrote
/// stays on the field's undo stack until the editor closes.
#[derive(Debug, Clone)]
pub(crate) struct Generation {
    pub(crate) activity: ActivityId,
    pub(crate) cancel: CancelFlag,
    pub(crate) before: String,
    /// Still writing: the field is read-only.
    pub(crate) running: bool,
}

/// What a running program, or a provider plugin, reports.
enum Event {
    Line(CliLine),
    Exit(std::io::Result<std::process::ExitStatus>),
    /// A provider's text, cut anywhere.
    Text(String),
    Ended(Ended),
}

/// A run that has started: where what it reports goes.
struct Running {
    tab: TabId,
    job: diffui_core::JobId,
    activity: ActivityId,
    label: String,
    program: String,
    output: Output,
    /// The provider plugin, for its errors' wording.
    provider: Option<String>,
    cut: bool,
    /// Everything it wrote to stdout so far.
    text: String,
    /// A provider's last line, until its end comes.
    partial: String,
}

/// A run: everything rendered, nothing left to ask.
struct Launch {
    tab: TabId,
    label: String,
    argv: Vec<String>,
    stdin: Option<String>,
    output: Output,
    timeout: std::time::Duration,
    /// What the command was started on: the revision its output belongs to.
    arg: CommandArg,
    cut: bool,
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
            cut: false,
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
                PromptKind::Input => self.ask_input(id, &prompt.label, "", "", cx),
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
        let answer = match self.plugin_prompt_answered(id, answer) {
            Ok(()) => return,
            Err(answer) => answer,
        };
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
        let answer = accepted.then(String::new);
        if self.plugin_prompt_answered(id, answer).is_ok() {
            return;
        }
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
            let mut cut = false;
            let commit = commit.ok_or_else(|| "No revision here".to_owned());
            for var in vars {
                let value = match var {
                    Var::Diff => read_diff(&repository, &commit.clone()?).await?,
                    Var::StackDiff => read_stack_diff(&repository, &commit.clone()?).await?,
                    Var::StackDescriptions => {
                        read_stack_descriptions(&repository, &commit.clone()?).await?
                    }
                    Var::RecentDescriptions => read_recent_descriptions(&repository).await?,
                    _ => continue,
                };
                let value = match var {
                    Var::Diff | Var::StackDiff => {
                        let (value, was_cut) = cut_diff(value);
                        cut |= was_cut;
                        value
                    }
                    _ => value,
                };
                values.insert(var, value);
            }
            Ok((values, cut))
        };
        Self::run_async(
            work,
            cx,
            move |this, fetched: Result<(Values, bool), String>, cx| {
                let Some(run) = this.user_runs.waiting.as_mut().filter(|run| run.id == id) else {
                    return;
                };
                run.fetching = false;
                match fetched {
                    Ok((values, cut)) => {
                        run.values.extend(values);
                        run.cut |= cut;
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
            cut,
            ..
        } = run;
        let render = |template: &Template| {
            template
                .render(&values)
                .map_err(|var| format!("{{{}}} has no value here", var.name()))
        };
        let provider = match &command.step {
            Step::Agent(_) => match self.ai_provider() {
                Ok(provider) => Some(provider),
                Err(error) => {
                    return self.push_error_toast(format!("{} didn't run", command.label), &error);
                }
            },
            _ => None,
        };
        let rendered: Result<Vec<String>, String> = match &command.step {
            Step::Exec(words) | Step::Run(words) => words.iter().map(render).collect(),
            Step::Agent(prompt) => render(prompt).map(|prompt| match &provider {
                Some(Provider::Custom(argv)) => argv.iter().cloned().chain([prompt]).collect(),
                _ => vec![prompt],
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
        let output = match command.step {
            Step::Edit(_) => Output::None,
            _ => command.output,
        };
        let launch = Launch {
            tab: id.tab,
            label: command.label.clone(),
            argv,
            stdin,
            output,
            timeout: command.timeout,
            arg,
            cut,
        };
        match provider {
            Some(Provider::Plugin { id, name }) => self.spawn_ai(launch, id, name, cx),
            _ => self.spawn_program(launch, cx),
        }
    }

    /// Start what every run shows: its activity row, its output panel run,
    /// and the field it writes into, opened if it isn't. `None` when the tab
    /// has no repository.
    fn begin_run(
        &mut self,
        launch: &Launch,
        program: String,
        args: Vec<String>,
        shown: String,
        provider: Option<(String, String)>,
        cx: &mut Cx,
    ) -> Option<(Running, CancelFlag, std::path::PathBuf)> {
        let tab = launch.tab;
        let label = launch.label.clone();
        let Some(root) = self
            .tab(tab)
            .and_then(|state| state.repository.as_ref())
            .map(|repository| repository.root.clone())
        else {
            self.push_error_toast(format!("{label} didn't run"), "No local repository here");
            return None;
        };
        let (activity, _) = self.begin_activity(tab, Prose::from(label.clone()), false);
        let cancel = CancelFlag::default();
        self.open_output_field(launch, activity, &cancel, cx);
        let state = self.tab_mut(tab)?;
        let job = state.session.next_job();
        let mut run = crate::cli_output::CliRun::program(
            activity,
            job,
            program.clone(),
            args,
            cancel.clone(),
        );
        if launch.output == Output::Panel {
            run.heading = Some(match &provider {
                Some((_, name)) => format!("{label} \u{b7} {name}"),
                None => label.clone(),
            });
        }
        crate::cli_output::keep_run(&mut state.cmdline.runs, run);
        if let Some(log) = self.activity_log_for(tab) {
            log.set_command(activity, Some(shown));
        }
        if launch.output == Output::Panel {
            if launch.cut {
                self.cli_output(tab, job, &[cut_note()]);
            }
            self.open_cli_output(tab, activity);
        }
        let running = Running {
            tab,
            job,
            activity,
            label,
            program,
            output: launch.output,
            provider: provider.map(|(id, _)| id),
            cut: launch.cut,
            text: String::new(),
            partial: String::new(),
        };
        Some((running, cancel, root))
    }

    /// Open the field `launch` writes into, if it isn't, and mark it as
    /// being written.
    fn open_output_field(
        &mut self,
        launch: &Launch,
        activity: ActivityId,
        cancel: &CancelFlag,
        cx: &mut Cx,
    ) {
        let generation = |before: String| Generation {
            activity,
            cancel: cancel.clone(),
            before,
            running: true,
        };
        match launch.output {
            Output::Description => {
                let Some(target) = crate::commands::target_revision(self, &launch.arg) else {
                    return;
                };
                self.perform(
                    Action::EditDescription {
                        target: Some(target),
                    },
                    cx,
                );
                if let Some(editor) = self
                    .tab_mut(launch.tab)
                    .and_then(|state| state.description_editor_mut())
                {
                    editor.generating = Some(generation(std::mem::take(&mut editor.content)));
                }
            }
            Output::PullRequest => {
                if self.pull_request_dialog().is_none() {
                    self.perform(
                        Action::Forge(crate::forge_dialog::ForgeAction::Open(launch.arg.clone())),
                        cx,
                    );
                }
                if let Some(Mode::PullRequest(dialog)) = self.mode_mut(ModeKind::PullRequest) {
                    let before = format!("{}\n\n{}", dialog.title, dialog.body);
                    dialog.generating = Some(generation(before));
                }
            }
            Output::Bookmark => {
                let creating = matches!(
                    self.name_prompt(),
                    Some(prompt) if matches!(prompt.kind, crate::bookmarks::PromptKind::Create { .. })
                );
                if !creating {
                    let Some(at) = crate::commands::target_revision(self, &launch.arg) else {
                        return;
                    };
                    self.perform(
                        Action::Bookmark(crate::bookmarks::BookmarkAction::Create { at }),
                        cx,
                    );
                }
                if let Some(prompt) = self.name_prompt_mut() {
                    prompt.generating = Some(generation(prompt.value.clone()));
                }
            }
            _ => {}
        }
    }

    fn spawn_program(&mut self, launch: Launch, cx: &mut Cx) {
        let Some((program, args)) = launch.argv.split_first() else {
            return;
        };
        let (program, args) = (program.clone(), args.to_vec());
        let shown = display_argv(&launch.argv);
        let Some((running, cancel, root)) =
            self.begin_run(&launch, program.clone(), args.clone(), shown, None, cx)
        else {
            return;
        };
        let (sender, events) = futures::channel::mpsc::unbounded();
        let mut process = tokio::process::Command::new(&program);
        process.args(&args).current_dir(&root);
        let (stdin, timeout) = (launch.stdin, launch.timeout);
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
        self.follow_run(running, events, cx);
    }

    /// Send the prompt to the provider plugin `id`.
    fn spawn_ai(&mut self, launch: Launch, id: String, name: String, cx: &mut Cx) {
        let Some((mut running, cancel, root)) = self.begin_run(
            &launch,
            name.clone(),
            Vec::new(),
            name.clone(),
            Some((id.clone(), name)),
            cx,
        ) else {
            return;
        };
        let request = diffui_plugins::protocol::AiRequest {
            prompt: launch.argv.into_iter().next().unwrap_or_default(),
            input: launch.stdin,
            cwd: Some(root.display().to_string()),
            ..Default::default()
        };
        match self.ai_complete(&id, request, cancel) {
            Ok(stream) => {
                let events = stream.map(|streamed| match streamed {
                    Streamed::Text(text) => Event::Text(text),
                    Streamed::Ended(ended) => Event::Ended(ended),
                });
                self.follow_run(running, events, cx);
            }
            Err(error) => {
                let ended = Ended::Failed {
                    kind: None,
                    message: error,
                };
                self.on_program_event(&mut running, Event::Ended(ended), cx);
            }
        }
    }

    /// Take in what a run reports until it ends.
    fn follow_run(
        &mut self,
        running: Running,
        events: impl futures::Stream<Item = Event> + Unpin + 'static,
        cx: &mut Cx,
    ) {
        let mut running = running;
        cx.spawn(async move |this, cx| {
            let mut events = events.ready_chunks(256);
            while let Some(batch) = events.next().await {
                let updated = this.update(cx, |this, cx| {
                    this.act(cx, |this, cx| {
                        for event in batch {
                            this.on_program_event(&mut running, event, cx);
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

    fn on_program_event(&mut self, running: &mut Running, event: Event, cx: &mut Cx) {
        match event {
            Event::Line(line) => {
                if line.stream == CliStream::Stdout {
                    self.take_text(running, &format!("{}\n", line.text));
                }
                self.cli_output(running.tab, running.job, std::slice::from_ref(&line));
            }
            Event::Text(text) => {
                self.take_text(running, &text);
                running.partial.push_str(&text);
                let lines: Vec<CliLine> = match running.partial.rfind('\n') {
                    Some(end) => {
                        let complete: String = running.partial.drain(..=end).collect();
                        complete
                            .lines()
                            .map(|line| CliLine {
                                stream: CliStream::Stdout,
                                text: line.to_owned(),
                            })
                            .collect()
                    }
                    None => Vec::new(),
                };
                if !lines.is_empty() {
                    self.cli_output(running.tab, running.job, &lines);
                }
            }
            Event::Exit(status) => {
                let exit = match &status {
                    Ok(status) => Exit::Code(status.code()),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                        Exit::Cancelled
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let program = &running.program;
                        Exit::Failed(if program.contains('/') {
                            format!("`{program}` doesn't exist")
                        } else {
                            format!("`{program}` isn't on PATH")
                        })
                    }
                    Err(error) => Exit::Failed(error.to_string()),
                };
                self.finish_run(running, exit, cx);
            }
            Event::Ended(ended) => {
                if !running.partial.is_empty() {
                    let line = CliLine {
                        stream: CliStream::Stdout,
                        text: std::mem::take(&mut running.partial),
                    };
                    self.cli_output(running.tab, running.job, &[line]);
                }
                let exit = match ended {
                    Ended::Done => Exit::Code(Some(0)),
                    Ended::Cancelled => Exit::Cancelled,
                    Ended::Failed { kind, message } => Exit::Failed(match &running.provider {
                        Some(id) => self.ai_failure(id, kind.as_deref(), &message),
                        None => message,
                    }),
                };
                self.finish_run(running, exit, cx);
            }
        }
    }

    /// Text a run wrote: kept, and shown in the field it writes into.
    fn take_text(&mut self, running: &mut Running, text: &str) {
        running.text.push_str(text);
        // Blank lines at either end wait for text after them.
        let shown = running.text.trim().to_owned();
        let activity = running.activity;
        match running.output {
            Output::Description => {
                if let Some(editor) = self.generating_editor(running.tab, activity) {
                    editor.content = shown;
                }
            }
            Output::PullRequest => {
                if let Some(dialog) = self.generating_dialog(activity) {
                    let (title, body) = crate::forge_dialog::title_and_body(&shown);
                    dialog.title = title;
                    dialog.body = body;
                }
            }
            _ => {}
        }
    }

    fn finish_run(&mut self, running: &Running, exit: Exit, cx: &mut Cx) {
        let Running {
            tab,
            job,
            activity,
            ref label,
            output,
            cut,
            ..
        } = *running;
        self.finish_cli_run(tab, job, activity, exit.clone());
        let ok = exit == Exit::Code(Some(0));
        let text = running.text.trim().to_owned();
        if let Some(editor) = self.generating_editor(tab, activity)
            && let Some(generation) = editor.generating.as_mut()
        {
            generation.running = false;
            editor.content = if ok {
                text.clone()
            } else {
                generation.before.clone()
            };
        }
        // Kept, stopped: the head's description, should it come after,
        // doesn't overwrite what was written.
        if let Some(dialog) = self.generating_dialog(activity)
            && let Some(generation) = dialog.generating.as_mut()
        {
            generation.running = false;
            let (title, body) =
                crate::forge_dialog::title_and_body(if ok { &text } else { &generation.before });
            dialog.title = title;
            dialog.body = body;
        }
        if let Some(prompt) = self.name_prompt_mut()
            && let Some(generation) = prompt
                .generating
                .take_if(|generation| generation.activity == activity)
        {
            prompt.value = match bookmark_name(&text) {
                name if ok && !name.is_empty() => name,
                _ => generation.before,
            };
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
        if cut && output != Output::Panel {
            self.push_info_toast(
                format!("{label}: the diff was cut"),
                &format!("It went with its first {} KB only.", DIFF_LIMIT / 1024),
            );
        }
        match output {
            Output::Toast if !text.is_empty() => self.push_info_toast(label.to_owned(), &text),
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
            Output::Activity
            | Output::Toast
            | Output::Description
            | Output::PullRequest
            | Output::Bookmark
            | Output::Panel
            | Output::None => {}
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

    /// The pull request dialog `activity` is writing into.
    fn generating_dialog(
        &mut self,
        activity: ActivityId,
    ) -> Option<&mut crate::forge_dialog::PrDialog> {
        match self.mode_mut(ModeKind::PullRequest) {
            Some(Mode::PullRequest(dialog))
                if dialog
                    .generating
                    .as_ref()
                    .is_some_and(|generation| generation.activity == activity) =>
            {
                Some(dialog)
            }
            _ => None,
        }
    }

    fn program_last_error(&self, tab: TabId, job: diffui_core::JobId) -> Option<String> {
        self.cli_run(tab, job)?
            .lines
            .iter()
            .rev()
            .find(|line| line.stream == CliStream::Stderr && !line.text.trim().is_empty())
            .map(|line| line.text.trim().to_owned())
    }

    /// Stop what writes into the active tab's description editor, the pull
    /// request dialog or the bookmark name prompt.
    pub(crate) fn stop_generating(&mut self) {
        let editor = self
            .active()
            .description_editor()
            .and_then(|editor| editor.generating.as_ref());
        let dialog = self
            .pull_request_dialog()
            .and_then(|dialog| dialog.generating.as_ref());
        let prompt = self
            .name_prompt()
            .and_then(|prompt| prompt.generating.as_ref());
        for generation in [editor, dialog, prompt].into_iter().flatten() {
            if generation.running {
                generation.cancel.cancel();
            }
        }
    }

    /// Whether the command `id` is an `agent` row: it runs the AI provider.
    pub(crate) fn runs_ai(&self, id: &str) -> bool {
        matches!(
            self.registry.get(id).map(|entry| &entry.handler),
            Some(crate::commands::Handler::User(command)) if matches!(command.step, Step::Agent(_))
        )
    }

    /// The AI provider an `agent` row would go to, for a tooltip or a
    /// palette hint: "Sends the diff to Anthropic".
    pub(crate) fn agent_destination(&self, id: &str) -> Option<String> {
        let crate::commands::Handler::User(command) = &self.registry.get(id)?.handler else {
            return None;
        };
        if !matches!(command.step, Step::Agent(_)) {
            return None;
        }
        Some(match self.ai_provider_shown() {
            Ok(provider) => format!("Sends the diff to {}", provider.name()),
            Err(why) => why,
        })
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
pub(crate) fn jj() -> tokio::process::Command {
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

/// The revisions of `commit`'s stack: what it adds over trunk.
fn stack_revset(commit: &str) -> String {
    format!("trunk()..{commit}")
}

/// The diff of `commit`'s stack, in git format.
async fn read_stack_diff(repository: &crate::Repository, commit: &str) -> Result<String, String> {
    let root = repository.root.display().to_string();
    let mut command = jj();
    command.args([
        "--ignore-working-copy",
        "--color=never",
        "-R",
        &root,
        "diff",
        "--git",
        "-r",
        &stack_revset(commit),
    ]);
    output_of(command, "the stack's diff").await
}

/// The descriptions of `commit`'s stack, oldest first, separated by lines
/// of `---`.
async fn read_stack_descriptions(
    repository: &crate::Repository,
    commit: &str,
) -> Result<String, String> {
    let root = repository.root.display().to_string();
    let mut command = jj();
    command.args([
        "--ignore-working-copy",
        "--color=never",
        "-R",
        &root,
        "log",
        "--no-graph",
        "--reversed",
        "-r",
        &stack_revset(commit),
        "-T",
        "description ++ \"---\\n\"",
    ]);
    output_of(command, "the stack's descriptions").await
}

/// `diff` cut at [`DIFF_LIMIT`], on a line, with a line that says how much
/// was left out; and whether it was.
fn cut_diff(diff: String) -> (String, bool) {
    if diff.len() <= DIFF_LIMIT {
        return (diff, false);
    }
    let mut end = DIFF_LIMIT;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    let end = diff[..end].rfind('\n').map_or(end, |line| line + 1);
    let left_out = diff.len() - end;
    let mut cut = diff[..end].to_owned();
    cut.push_str(&format!(
        "[diffui: the diff is cut here; the other {left_out} bytes of the diff are left out here]\n"
    ));
    (cut, true)
}

/// The output panel's first line when the diff was cut.
fn cut_note() -> CliLine {
    CliLine {
        stream: CliStream::Stderr,
        text: format!(
            "The diff was cut to its first {} KB: what follows is about that part.",
            DIFF_LIMIT / 1024
        ),
    }
}

/// A provider's suggestion as a bookmark name: its first line, lowercase,
/// words joined by dashes, at most 60 characters.
fn bookmark_name(suggestion: &str) -> String {
    let line = suggestion
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut name = String::new();
    for c in line.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '/' || c == '.' {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let mut name: String = name.chars().take(60).collect();
    while name.ends_with(['-', '.', '/']) {
        name.pop();
    }
    name
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
    fn a_suggestion_becomes_a_bookmark_name() {
        assert_eq!(bookmark_name("`Fix Parser crash`\n"), "fix-parser-crash");
        assert_eq!(
            bookmark_name("\nfeat/add_AI providers!!\nmore"),
            "feat/add-ai-providers"
        );
        assert_eq!(bookmark_name("Name: \"x  y\""), "name-x-y");
        assert!(bookmark_name(&"word ".repeat(40)).len() <= 60);
    }

    #[test]
    fn a_big_diff_is_cut_on_a_line_and_says_so() {
        let small = "diff --git a b\n+x\n".to_owned();
        assert_eq!(cut_diff(small.clone()), (small, false));
        let big = "+0123456789\n".repeat(DIFF_LIMIT / 10);
        let (cut, was_cut) = cut_diff(big.clone());
        assert!(was_cut);
        assert!(cut.len() < DIFF_LIMIT + 200, "{}", cut.len());
        assert!(cut.ends_with("bytes of the diff are left out here]\n"));
        assert_eq!(cut.lines().rev().nth(1), Some("+0123456789"));
    }

    #[test]
    fn long_arguments_are_cut_for_the_row() {
        let prompt = "x".repeat(100);
        let shown = display_argv(&["claude".to_owned(), "-p".to_owned(), prompt]);
        assert!(shown.starts_with("claude -p "), "{shown}");
        assert!(shown.chars().count() < 60, "{shown}");
    }
}

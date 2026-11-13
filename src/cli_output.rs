//! A passthrough run of the jj CLI and the panel that shows its output.
//!
//! The run is diffui's own binary started as `jj` (see
//! `diffui_core::cli::passthrough`), so the CLI always matches the repo
//! format diffui writes. Its output streams into the tab's [`CliRun`] and the
//! panel follows it live; the run's activity row reopens the panel later.

use crate::cmdline::CmdAction;
use crate::theme::{self, text_size};
use crate::{Action, Cx, icons, input, typography, ui};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, ClickEvent, Window, div, px};
use std::time::Instant;

use diffui_core::JobId;
use diffui_core::cli::tokenize::shell_quote;
use diffui_core::cli::{CliInvocation, CliLine, CliStream};

use crate::activity::{ActivityId, ActivityStatus};
use crate::modes::{Mode, ModeKind};
use crate::prose::Prose;
use crate::ui::Look;
use crate::{Diffui, TabId};

/// Lines a run keeps; older ones fall off the front.
const MAX_LINES: usize = 5_000;
/// Finished runs a tab keeps, for their activity rows.
const MAX_RUNS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Exit {
    /// The CLI exited, with this status (`None`: killed by a signal).
    Code(Option<i32>),
    Cancelled,
    /// It never ran, or the run broke.
    Failed(String),
}

/// One passthrough run, or a program a declared command started.
#[derive(Debug)]
pub(crate) struct CliRun {
    pub(crate) activity: ActivityId,
    pub(crate) job: JobId,
    /// What ran: `jj` for a passthrough.
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) lines: Vec<CliLine>,
    /// Lines dropped off the front past [`MAX_LINES`].
    pub(crate) dropped: usize,
    pub(crate) exit: Option<Exit>,
    pub(crate) started: Instant,
    /// Routes a `jj git push/fetch`'s credential prompts to the app; held
    /// for the run's life.
    scope: Option<Scope>,
    /// What Stop raises for a program the app runs itself; a passthrough
    /// is stopped through its repository's actor instead.
    cancel: Option<diffui_core::repo::CancelFlag>,
    /// Text to read rather than a run: `program` is its title, and there is
    /// no exit status to show.
    transcript: bool,
}

impl CliRun {
    /// A program a declared command started, cancelled through `cancel`.
    pub(crate) fn program(
        activity: ActivityId,
        job: JobId,
        program: String,
        args: Vec<String>,
        cancel: diffui_core::repo::CancelFlag,
    ) -> Self {
        Self {
            activity,
            job,
            program,
            args,
            lines: Vec::new(),
            dropped: 0,
            exit: None,
            started: Instant::now(),
            scope: None,
            cancel: Some(cancel),
            transcript: false,
        }
    }

    pub(crate) fn command(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            line.push_str(&shell_quote(arg));
        }
        line
    }

    /// Everything the run printed, as one text.
    pub(crate) fn transcript(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.text);
            out.push('\n');
        }
        out
    }
}

/// The askpass scope, which has no `Debug` of its own.
struct Scope(diffui_core::askpass::Scope);

impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Scope")
    }
}

/// Add `run` to a tab's runs, dropping the oldest finished one past
/// [`MAX_RUNS`].
pub(crate) fn keep_run(runs: &mut Vec<CliRun>, run: CliRun) {
    if runs.len() >= MAX_RUNS
        && let Some(oldest) = runs.iter().position(|run| run.exit.is_some())
    {
        runs.remove(oldest);
    }
    runs.push(run);
}

/// What the output panel's controls (and an activity row's Output pill)
/// ask for.
#[derive(Debug, Clone)]
pub(crate) enum OutputAction {
    Open(ActivityId),
    Close,
    Copy,
    /// Stop the run on screen.
    Stop,
}

/// The open output panel: which tab's run it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputView {
    pub(crate) tab: TabId,
    pub(crate) activity: ActivityId,
}

impl Diffui {
    /// Run `args` through the jj CLI in `tab`'s workspace, and show
    /// the output as it comes.
    pub(crate) fn start_cli_run(&mut self, tab: TabId, args: Vec<String>) {
        let program = match std::env::current_exe() {
            Ok(program) => program,
            Err(error) => {
                self.push_error_toast("Can't run jj", &error.to_string());
                return;
            }
        };
        let command = std::iter::once("jj".to_owned())
            .chain(args.iter().map(|arg| shell_quote(arg)))
            .collect::<Vec<_>>()
            .join(" ");
        let (activity, _) = self.begin_activity(tab, Prose::new().code(command), false);
        // git may ask for a password or passphrase: route it to the prompt.
        let scope = (args.first().map(String::as_str) == Some("git"))
            .then(diffui_core::askpass::Scope::new);
        let env = scope
            .as_ref()
            .map(|scope| {
                scope
                    .environment()
                    .into_iter()
                    .map(|(key, value)| {
                        (
                            key.to_string_lossy().into_owned(),
                            value.to_string_lossy().into_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let job = state.session.next_job();
        let invocation = CliInvocation {
            program,
            args: args.clone(),
            env,
        };
        let Some(handle) = state.handle.clone() else {
            return;
        };
        state.jobs.issue(job, crate::Pending::Cli(activity));
        keep_run(
            &mut state.cmdline.runs,
            CliRun {
                activity,
                job,
                program: "jj".to_owned(),
                args,
                lines: Vec::new(),
                dropped: 0,
                exit: None,
                started: Instant::now(),
                scope: scope.map(Scope),
                cancel: None,
                transcript: false,
            },
        );
        handle.send(diffui_core::Command::RunCli { job, invocation });
        self.open_cli_output(tab, activity);
    }

    /// Show `lines` under `title` in the output panel: something to read
    /// that no run printed (a plugin's log). It is kept with the active
    /// tab's runs, without an activity row.
    pub(crate) fn show_transcript(&mut self, title: String, lines: Vec<CliLine>) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        let activity = self.allocate_activity_id();
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let job = state.session.next_job();
        keep_run(
            &mut state.cmdline.runs,
            CliRun {
                activity,
                job,
                program: title,
                args: Vec::new(),
                lines,
                dropped: 0,
                exit: Some(Exit::Code(Some(0))),
                started: Instant::now(),
                scope: None,
                cancel: None,
                transcript: true,
            },
        );
        self.open_cli_output(tab, activity);
    }

    pub(crate) fn open_cli_output(&mut self, tab: TabId, activity: ActivityId) {
        self.pop_mode(ModeKind::ActivityPopover);
        self.push_mode(Mode::CliOutput(OutputView { tab, activity }));
    }

    pub(crate) fn cli_run(&self, tab: TabId, job: JobId) -> Option<&CliRun> {
        self.tab(tab)?
            .cmdline
            .runs
            .iter()
            .find(|run| run.job == job)
    }

    fn cli_run_mut(&mut self, tab: TabId, job: JobId) -> Option<&mut CliRun> {
        self.tab_mut(tab)?
            .cmdline
            .runs
            .iter_mut()
            .find(|run| run.job == job)
    }

    /// The run the output panel shows.
    pub(crate) fn shown_cli_run(&self) -> Option<&CliRun> {
        let Some(Mode::CliOutput(view)) = self.mode(ModeKind::CliOutput) else {
            return None;
        };
        self.tabs
            .iter()
            .find(|tab| tab.id == view.tab)?
            .state
            .cmdline
            .runs
            .iter()
            .find(|run| run.activity == view.activity)
    }

    pub(crate) fn cli_output(&mut self, tab: TabId, job: JobId, lines: &[CliLine]) {
        if let Some(run) = self.cli_run_mut(tab, job) {
            run.lines.extend(lines.iter().cloned());
            if run.lines.len() > MAX_LINES {
                let excess = run.lines.len() - MAX_LINES;
                run.lines.drain(..excess);
                run.dropped += excess;
            }
        }
    }

    /// The run ended: settle its row and its panel.
    pub(crate) fn finish_cli_run(
        &mut self,
        tab: TabId,
        job: JobId,
        activity: ActivityId,
        exit: Exit,
    ) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(run) = state.cmdline.runs.iter_mut().find(|run| run.job == job) else {
            return;
        };
        // A prompt the user declined reads as that, not as git's failure.
        let declined = run.scope.as_ref().is_some_and(|scope| scope.0.cancelled());
        let exit = if declined && exit != Exit::Code(Some(0)) {
            Exit::Cancelled
        } else {
            exit
        };
        run.scope = None;
        let (status, result) = match &exit {
            Exit::Code(Some(0)) => (ActivityStatus::Done, None),
            Exit::Code(Some(code)) => (ActivityStatus::Error, Some(format!("Exited with {code}"))),
            Exit::Code(None) => (ActivityStatus::Error, Some("Killed".to_owned())),
            Exit::Cancelled => (ActivityStatus::Cancelled, Some("Cancelled".to_owned())),
            Exit::Failed(error) => (ActivityStatus::Error, Some(error.clone())),
        };
        // The last thing the CLI said is the summary worth keeping.
        let summary = result.map(|result| {
            match run
                .lines
                .iter()
                .rev()
                .find(|line| line.stream == CliStream::Stderr && !line.text.trim().is_empty())
            {
                Some(line) if !matches!(exit, Exit::Cancelled) => {
                    format!("{result}: {}", line.text.trim())
                }
                _ => result,
            }
        });
        run.exit = Some(exit);
        state.activities.finish(activity, status, summary);
    }

    pub(crate) fn on_output_action(&mut self, action: OutputAction, cx: &mut Cx) {
        match action {
            OutputAction::Open(activity) => {
                if let Some(tab) = self.active_tab_id() {
                    self.open_cli_output(tab, activity);
                }
            }
            OutputAction::Close => {
                self.pop_mode(ModeKind::CliOutput);
            }
            OutputAction::Copy => {
                if let Some(run) = self.shown_cli_run() {
                    crate::write_clipboard(run.transcript(), cx)
                }
            }
            OutputAction::Stop => {
                let Some(&Mode::CliOutput(view)) = self.mode(ModeKind::CliOutput) else {
                    return;
                };
                let Some(run) = self.shown_cli_run().filter(|run| run.exit.is_none()) else {
                    return;
                };
                if let Some(cancel) = &run.cancel {
                    return cancel.cancel();
                }
                let job = run.job;
                if let Some(state) = self.tab_mut(view.tab)
                    && let Some(handle) = &state.handle
                {
                    handle.send(diffui_core::Command::Cancel { job });
                }
            }
        }
    }
}

/// Wide enough for a `jj log` line without wrapping most of them.
const OUTPUT_WIDTH: f32 = 680.0;

/// The output panel: the command, its output (stderr set apart), how it ended,
/// and Copy. The output is one read-only editor, so a drag selects across lines
/// and ⌘C copies the sweep; with nothing selected ⌘C copies all of it.
pub(crate) fn build_overlay(
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let Some(run) = ui.shown_cli_run() else {
        return div().into_any_element();
    };
    let font = look.font;

    let content = if run.lines.is_empty() && run.dropped == 0 {
        div()
            .p(px(theme::space::SM))
            .child(typography::line(
                if run.exit.is_none() {
                    "Waiting for output\u{2026}"
                } else {
                    "No output"
                },
                font,
                text_size::BODY,
                theme.subtle_text,
            ))
            .into_any_element()
    } else {
        output_text(run, look, window, cx)
    };
    let output = crate::field::field_well(false, theme)
        .overflow_hidden()
        .child(content);

    let (status, status_color) = match &run.exit {
        None => (
            format!("Running for {}s", run.started.elapsed().as_secs()),
            theme.muted_text,
        ),
        Some(Exit::Code(Some(0))) => ("Exit status 0".to_owned(), theme.added_text),
        Some(Exit::Code(Some(code))) => (format!("Exit status {code}"), theme.removed_text),
        Some(Exit::Code(None)) => ("Killed".to_owned(), theme.removed_text),
        Some(Exit::Cancelled) => ("Cancelled".to_owned(), theme.muted_text),
        Some(Exit::Failed(error)) => (format!("Didn't run: {error}"), theme.removed_text),
    };
    let status = typography::centered(
        typography::line(status, font, text_size::CAPTION, status_color),
        font,
        text_size::CAPTION,
    );
    let mut buttons: Vec<AnyElement> = vec![
        look.button("cli-output-copy", "Copy")
            .icon(icons::COPY, theme.muted_text)
            .on_click(send::<ClickEvent>(OutputAction::Copy, cx))
            .into_any_element(),
    ];
    if run.exit.is_none() {
        buttons.push(
            look.button("cli-output-stop", "Stop")
                .on_click(send::<ClickEvent>(OutputAction::Stop, cx))
                .into_any_element(),
        );
    } else {
        buttons.push(
            look.button("cli-output-done", "Close")
                .on_click(send::<ClickEvent>(OutputAction::Close, cx))
                .into_any_element(),
        );
    }
    let title = if run.transcript {
        Prose::from(run.program.clone())
    } else {
        Prose::new().code(run.command())
    };
    let mut card = crate::overlay_kit::Dialog::new(&title, look)
        .width(OUTPUT_WIDTH)
        .child(output)
        .bound_hint(
            ui,
            crate::commands::Context::Output,
            &["output.copy"],
            "Copy",
        )
        .bound_hint(
            ui,
            crate::commands::Context::Output,
            &["output.close"],
            "Close",
        );
    if !run.transcript {
        card = card.note(status);
    }
    let card = card.buttons(buttons).build();
    let dismiss = send::<()>(OutputAction::Close, cx);
    ui::modal(
        "cli-output-modal",
        card,
        crate::toolbar::modal_top(),
        move |window, cx| dismiss(&(), window, cx),
    )
    .into_any_element()
}

fn send<E: 'static>(
    action: OutputAction,
    cx: &mut Cx,
) -> impl Fn(&E, &mut Window, &mut gpui_kit::App) + 'static {
    Diffui::on_action(Action::CommandLine(CmdAction::Output(action)), cx)
}

/// The lines as one text, each stream in its colour. stderr carries a rule in the
/// gutter, so the two streams read apart even where jj's informational stderr
/// looks like output.
fn output_text(run: &CliRun, look: Look, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let Look { theme, mono, .. } = look;
    let mut text = String::new();
    let mut colors = Vec::new();
    let mut marks = Vec::new();
    if run.dropped > 0 {
        let line = format!("\u{2026} {} earlier lines", run.dropped);
        colors.push((0..line.len(), theme.subtle_text));
        text.push_str(&line);
    }
    let rule = theme::mix(theme.border, theme.muted_text, 0.3);
    for line in &run.lines {
        if !text.is_empty() {
            text.push('\n');
        }
        let range = text.len()..text.len() + line.text.len();
        text.push_str(&line.text);
        match line.stream {
            CliStream::Stdout => colors.push((range, theme.text)),
            CliStream::Stderr => {
                colors.push((range.clone(), theme.muted_text));
                marks.push((range, rule));
            }
        }
    }
    let inner =
        OUTPUT_WIDTH - 2.0 * crate::overlay_kit::DIALOG_PAD - 2.0 - OUTPUT_INSET * 2.0 - RULE_ROOM;
    let line_height = (text_size::BODY * typography::DEFAULT_LINE_HEIGHT).ceil();
    let lines =
        crate::measure::wrapped_line_count(&text, text_size::BODY, mono, line_height, inner);
    let height = (lines as f32 * line_height + 2.0 * OUTPUT_INSET).min(OUTPUT_MAX_HEIGHT);
    let look = input::StyledLook {
        wrap: true,
        follow: true,
        mark_width: Some(RULE_WIDTH),
        colors,
        marks,
        ..Default::default()
    };
    let spec = input::FieldSpec::new(OUTPUT_TEXT_ID, &text);
    input::styled_text(spec, look, mono, text_size::BODY, window, cx)
        .with_size(gpui_kit::component::Size::XSmall)
        .line_height(px(line_height))
        .h(px(height))
        .py(px(OUTPUT_INSET))
        .into_any_element()
}

const OUTPUT_TEXT_ID: &str = "cli-output-text";
/// The panel grows with its output up to here, then scrolls.
const OUTPUT_MAX_HEIGHT: f32 = 360.0;
const OUTPUT_INSET: f32 = theme::space::SM;
const RULE_WIDTH: f32 = 2.0;
/// What the rule's strip takes from the text's width.
const RULE_ROOM: f32 = 16.0;

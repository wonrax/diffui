//! A passthrough run of the jj CLI and the panel that shows its output.
//!
//! The run is diffui's own binary started as `jj` (see
//! `diffui_core::cli::passthrough`), so the CLI always matches the repo
//! format diffui writes. Its output streams into the tab's [`CliRun`] and the
//! panel follows it live; the run's activity row reopens the panel later.

use crate::Cx;
use gpui_kit::{AnyElement, Window};
use std::time::Instant;

use diffui_core::cli::tokenize::shell_quote;
use diffui_core::cli::{CliInvocation, CliLine, CliStream};
use diffui_core::{JobId, RepoError};

use crate::activity::{ActivityId, ActivityStatus};
use crate::modes::{Mode, ModeKind};
use crate::prose::Prose;
use crate::theme::ThemeSpec;
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

/// One passthrough run.
#[derive(Debug)]
pub(crate) struct CliRun {
    pub(crate) activity: ActivityId,
    pub(crate) job: JobId,
    pub(crate) args: Vec<String>,
    pub(crate) lines: Vec<CliLine>,
    /// Lines dropped off the front past [`MAX_LINES`].
    pub(crate) dropped: usize,
    pub(crate) exit: Option<Exit>,
    pub(crate) started: Instant,
    /// Routes a `jj git push/fetch`'s credential prompts to the app; held
    /// for the run's life.
    scope: Option<Scope>,
}

impl CliRun {
    pub(crate) fn command(&self) -> String {
        let mut line = String::from("jj");
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
    /// Run `args` through the jj CLI in the active tab's workspace, and show
    /// the output as it comes.
    pub(crate) fn start_cli_run(&mut self, args: Vec<String>, cx: &mut Cx) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
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
        state.jobs.cli.insert(job, activity);
        let runs = &mut state.cmdline.runs;
        if runs.len() >= MAX_RUNS
            && let Some(oldest) = runs.iter().position(|run| run.exit.is_some())
        {
            runs.remove(oldest);
        }
        runs.push(CliRun {
            activity,
            job,
            args,
            lines: Vec::new(),
            dropped: 0,
            exit: None,
            started: Instant::now(),
            scope: scope.map(Scope),
        });
        handle.send(diffui_core::Command::RunCli { job, invocation });
        self.open_cli_output(tab, activity);
    }

    pub(crate) fn open_cli_output(&mut self, tab: TabId, activity: ActivityId) {
        self.pop_mode(ModeKind::ActivityPopover);
        self.push_mode(Mode::CliOutput(OutputView { tab, activity }));
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
    pub(crate) fn finish_cli_run(&mut self, tab: TabId, job: JobId, exit: Exit, cx: &mut Cx) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(activity) = state.jobs.cli.remove(&job) else {
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

    pub(crate) fn fail_cli_run(&mut self, tab: TabId, job: JobId, error: RepoError, cx: &mut Cx) {
        self.finish_cli_run(tab, job, Exit::Failed(error.to_string()), cx)
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
            OutputAction::Copy => match self.shown_cli_run() {
                Some(run) => crate::write_clipboard(run.transcript(), cx),
                None => {}
            },
            OutputAction::Stop => {
                let Some(&Mode::CliOutput(view)) = self.mode(ModeKind::CliOutput) else {
                    return;
                };
                let job = self
                    .shown_cli_run()
                    .filter(|run| run.exit.is_none())
                    .map(|run| run.job);
                if let Some(job) = job
                    && let Some(state) = self.tab_mut(view.tab)
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

/// Stub: the cli_output view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::overlay(
        "cli_output::build_overlay",
        ui.mode(ModeKind::CliOutput).is_some(),
    )
}

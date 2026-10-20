//! A passthrough run of the jj CLI and the panel that shows its output.
//!
//! The run is diffui's own binary started as `jj` (see
//! `diffui_core::cli::passthrough`), so the CLI always matches the repo
//! format diffui writes. Its output streams into the tab's [`CliRun`] and the
//! panel follows it live; the run's activity row reopens the panel later.

use std::time::Instant;

use diffui_core::cli::tokenize::shell_quote;
use diffui_core::cli::{CliInvocation, CliLine, CliStream};
use diffui_core::{JobId, RepoError};
use iced::advanced::text::Wrapping;
use iced::widget::{Space, column, container, row, text};
use iced::{Background, Border, Element, Length, Padding, Task};

use crate::activity::{ActivityId, ActivityStatus};
use crate::cmdline::CmdAction;
use crate::modes::{Mode, ModeKind};
use crate::prose::Prose;
use crate::theme::{self, ThemeSpec, text_size};
use crate::{Action, Diffui, Message, TabId, UiEvent, ui};

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
    pub(crate) fn start_cli_run(&mut self, args: Vec<String>) -> Task<Message> {
        let Some(tab) = self.active_tab_id() else {
            return Task::none();
        };
        let program = match std::env::current_exe() {
            Ok(program) => program,
            Err(error) => {
                self.push_error_toast("Can't run jj", &error.to_string());
                return Task::none();
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
            return Task::none();
        };
        let job = state.session.next_job();
        let invocation = CliInvocation {
            program,
            args: args.clone(),
            env,
        };
        let Some(handle) = state.handle.clone() else {
            return Task::none();
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
        Task::none()
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
    pub(crate) fn finish_cli_run(&mut self, tab: TabId, job: JobId, exit: Exit) -> Task<Message> {
        let Some(state) = self.tab_mut(tab) else {
            return Task::none();
        };
        let Some(activity) = state.jobs.cli.remove(&job) else {
            return Task::none();
        };
        let Some(run) = state.cmdline.runs.iter_mut().find(|run| run.job == job) else {
            return Task::none();
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
        Task::none()
    }

    pub(crate) fn fail_cli_run(
        &mut self,
        tab: TabId,
        job: JobId,
        error: RepoError,
    ) -> Task<Message> {
        self.finish_cli_run(tab, job, Exit::Failed(error.to_string()))
    }

    pub(crate) fn on_output_action(&mut self, action: OutputAction) -> Task<Message> {
        match action {
            OutputAction::Open(activity) => {
                if let Some(tab) = self.active_tab_id() {
                    self.open_cli_output(tab, activity);
                }
                Task::none()
            }
            OutputAction::Close => {
                self.pop_mode(ModeKind::CliOutput);
                Task::none()
            }
            OutputAction::Copy => match self.shown_cli_run() {
                Some(run) => iced::clipboard::write(run.transcript()).discard(),
                None => Task::none(),
            },
            OutputAction::Stop => {
                let Some(&Mode::CliOutput(view)) = self.mode(ModeKind::CliOutput) else {
                    return Task::none();
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
                Task::none()
            }
        }
    }
}

/// The output panel: the command, its output (stderr set apart), how it
/// ended, and Copy.
pub(crate) fn build_overlay(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(run) = ui.shown_cli_run() else {
        return Space::new().into();
    };
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let send = |action| Message::Action(Action::CommandLine(CmdAction::Output(action)));

    let title = row![
        container(ui::dialog_title(
            &Prose::new().code(run.command()),
            font,
            mono,
            theme
        ))
        .width(Length::Fill),
        ui::close_button(theme.muted_text, theme).on_press(send(OutputAction::Close)),
    ]
    .align_y(iced::alignment::Vertical::Center)
    .spacing(theme::space::SM);

    let mut lines = column![].spacing(1);
    if run.dropped > 0 {
        lines = lines.push(
            text(format!("\u{2026} {} earlier lines", run.dropped))
                .size(text_size::UI)
                .font(mono)
                .color(theme.subtle_text),
        );
    }
    for line in &run.lines {
        let color = match line.stream {
            CliStream::Stdout => theme.text,
            CliStream::Stderr => theme.muted_text,
        };
        let shown = if line.text.is_empty() {
            " "
        } else {
            &line.text
        };
        let body = text(shown.to_owned())
            .size(text_size::UI)
            .font(mono)
            .color(color)
            .wrapping(Wrapping::WordOrGlyph);
        // stderr carries a rule in the gutter, so the two streams read apart
        // even where jj's informational stderr looks like output.
        let marker = container(Space::new())
            .width(Length::Fixed(2.0))
            .height(Length::Fill)
            .style(move |_| {
                container::Style::default().background(match line.stream {
                    CliStream::Stdout => iced::Color::TRANSPARENT,
                    CliStream::Stderr => theme::mix(theme.border, theme.muted_text, 0.3),
                })
            });
        lines = lines.push(
            row![marker, container(body).width(Length::Fill)]
                .spacing(theme::space::SM)
                .height(Length::Shrink),
        );
    }
    if run.lines.is_empty() && run.dropped == 0 {
        lines = lines.push(
            text(if run.exit.is_none() {
                "Waiting for output\u{2026}"
            } else {
                "No output"
            })
            .size(text_size::UI)
            .font(font)
            .color(theme.subtle_text),
        );
    }
    let output = container(
        ui::scrollable(
            container(lines).padding(Padding {
                top: theme::space::SM,
                right: crate::scrollbar::GUTTER,
                bottom: theme::space::SM,
                left: theme::space::SM,
            }),
            theme,
        )
        .anchor_bottom()
        .width(Length::Fill)
        .height(Length::Shrink),
    )
    .width(Length::Fill)
    .height(Length::Fit.max(360.0))
    .style(move |_| container::Style {
        background: Some(Background::Color(theme.panel_background)),
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: theme::radius::CONTROL.into(),
        },
        ..container::Style::default()
    });

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
    let leading = text(status)
        .size(text_size::UI)
        .font(font)
        .color(status_color)
        .into();
    let mut buttons: Vec<Element<'_, Message>> = vec![
        ui::text_button(
            Some((crate::icons::COPY, theme.muted_text)),
            "Copy",
            ui::Variant::Raised,
            ui::Size::Standard,
            font,
            theme,
        )
        .on_press(send(OutputAction::Copy))
        .into(),
    ];
    if run.exit.is_none() {
        buttons.push(
            ui::text_button(
                None,
                "Stop",
                ui::Variant::Raised,
                ui::Size::Standard,
                font,
                theme,
            )
            .on_press(send(OutputAction::Stop))
            .into(),
        );
    } else {
        buttons.push(
            ui::text_button(
                None,
                "Close",
                ui::Variant::Primary,
                ui::Size::Standard,
                font,
                theme,
            )
            .on_press(send(OutputAction::Close))
            .into(),
        );
    }
    let card = ui::dialog_card(
        column![title, output, ui::button_row(Some(leading), buttons)].spacing(ui::DIALOG_SPACING),
        OUTPUT_WIDTH,
        theme,
    );
    ui::modal(
        card,
        send(OutputAction::Close),
        Message::Ui(UiEvent::ConfirmNoOp),
    )
}

/// Wide enough for a `jj log` line without wrapping most of them.
const OUTPUT_WIDTH: f32 = 680.0;

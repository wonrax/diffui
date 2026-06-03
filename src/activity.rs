//! Per-tab activity / progress log: the single place every long-running
//! operation reports into — cold load, refresh, revset eval, fetch, undo, push.
//!
//! The toolbar shows the *active* tab's log: a right-edge indicator (spinner +
//! the first running task's label + a `+N` chip when several run at once) and a
//! thin progress line along the toolbar's bottom edge. Clicking the indicator
//! opens a popover listing every entry with a progress bar, percentage, and a
//! status icon; each row expands to its captured output (remote sideband, e.g.
//! GitHub's "Create a pull request" hint, with clickable URLs).
//!
//! Entries are **persistent**: running and finished alike stay until the user
//! clears them or the app restarts — nothing auto-dismisses.

use std::time::Instant;

use iced::{
    Background, Border, Color, Element, Length, Padding, alignment,
    font::Weight,
    widget::{Space, button, column, container, mouse_area, row, scrollable, stack, text},
};

use crate::backend::LoadProgress;
use crate::theme::{ThemeSpec, chip_background, emphasis_font, iced_scrollable_style};
use crate::{Diffui, Message};

/// Frames for the running spinner. ASCII so it renders under any configured
/// font; advanced at ~12fps off the activity's elapsed time (the toolbar tick
/// keeps `view()` re-running while anything is in flight).
const SPINNER_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

/// Stable identity for a log entry, handed back by [`ActivityLog::start`] so the
/// operation's later `append_output`/`finish` calls (and the row's expand
/// toggle) address the right entry as the list grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActivityId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Done,
    Error,
}

/// One tracked operation.
#[derive(Debug, Clone)]
pub struct Activity {
    pub id: ActivityId,
    pub label: String,
    pub status: ActivityStatus,
    /// Live progress handle the worker bumps from its thread; the UI polls it
    /// each frame. `total == 0` ⇒ indeterminate (spinner / pulsing line).
    pub progress: LoadProgress,
    /// Whether the op reports real progress. A determinate op still reads as
    /// indeterminate until its worker calls `set_total` (so the bar doesn't
    /// jump from a fake 0/0); an indeterminate op never sets a total.
    pub determinate: bool,
    /// Captured output lines (remote messages / errors), shown when expanded.
    pub detail: Vec<String>,
    /// One-line summary recorded on finish.
    pub result: Option<String>,
    pub started: Instant,
    pub expanded: bool,
}

impl Activity {
    /// `(loaded, total)` and whether it should render as a determinate bar.
    fn progress_snapshot(&self) -> (usize, usize, bool) {
        let (loaded, total) = self.progress.snapshot();
        (loaded, total, self.determinate && total > 0)
    }
}

/// The per-tab activity list. Held inline on `Diffui` for the active tab and in
/// each `RepoState` stash for the rest.
#[derive(Debug, Clone, Default)]
pub struct ActivityLog {
    activities: Vec<Activity>,
}

impl ActivityLog {
    /// Begin tracking an op. The caller allocates the `id` (from
    /// `Diffui::next_activity_id`); the returned [`LoadProgress`] is handed to
    /// the worker so it can report progress without going through the message
    /// loop.
    pub fn start(
        &mut self,
        id: ActivityId,
        label: impl Into<String>,
        determinate: bool,
    ) -> LoadProgress {
        let progress = LoadProgress::default();
        self.activities.push(Activity {
            id,
            label: label.into(),
            status: ActivityStatus::Running,
            progress: progress.clone(),
            determinate,
            detail: Vec::new(),
            result: None,
            started: Instant::now(),
            expanded: false,
        });
        progress
    }

    pub fn append_output(&mut self, id: ActivityId, line: impl Into<String>) {
        if let Some(activity) = self.get_mut(id) {
            activity.detail.push(line.into());
        }
    }

    /// Append several output lines at once (e.g. the sideband captured during a
    /// push/fetch, delivered together on completion).
    pub fn extend_output(&mut self, id: ActivityId, lines: impl IntoIterator<Item = String>) {
        if let Some(activity) = self.get_mut(id) {
            activity.detail.extend(lines);
        }
    }

    pub fn finish(&mut self, id: ActivityId, status: ActivityStatus, result: Option<String>) {
        if let Some(activity) = self.get_mut(id) {
            activity.status = status;
            if result.is_some() {
                activity.result = result;
            }
        }
    }

    pub fn toggle_expand(&mut self, id: ActivityId) {
        if let Some(activity) = self.get_mut(id) {
            activity.expanded = !activity.expanded;
        }
    }

    /// Drop finished entries, keeping anything still running (so the log never
    /// hides in-flight work). Bound to the popover's "Clear".
    pub fn clear_finished(&mut self) {
        self.activities
            .retain(|activity| matches!(activity.status, ActivityStatus::Running));
    }

    pub fn is_empty(&self) -> bool {
        self.activities.is_empty()
    }

    pub fn any_running(&self) -> bool {
        self.activities
            .iter()
            .any(|a| matches!(a.status, ActivityStatus::Running))
    }

    pub fn running_count(&self) -> usize {
        self.activities
            .iter()
            .filter(|a| matches!(a.status, ActivityStatus::Running))
            .count()
    }

    /// The first running entry, in start order — drives the toolbar label and
    /// the bottom progress line. When it finishes the next running entry (if
    /// any) takes over.
    pub fn first_running(&self) -> Option<&Activity> {
        self.activities
            .iter()
            .find(|a| matches!(a.status, ActivityStatus::Running))
    }

    fn get_mut(&mut self, id: ActivityId) -> Option<&mut Activity> {
        self.activities.iter_mut().find(|a| a.id == id)
    }
}

/// Spinner glyph for an activity that has been running for `elapsed`.
fn spinner_glyph(started: Instant) -> &'static str {
    let frame = (started.elapsed().as_millis() / 90) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[frame]
}

/// The right-edge toolbar control: a clickable chip that shows the first
/// running task (spinner + label + `+N`) while work is in flight, or a quiet
/// "Activity" affordance otherwise. Click → open/close the popover.
pub fn activity_indicator(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let mono = ui.config.mono_font;
    let running = ui.activities.running_count();

    let body: Element<'_, Message> = if let Some(active) = ui.activities.first_running() {
        let mut chips = row![
            text(spinner_glyph(active.started))
                .size(12)
                .font(mono)
                .color(theme.accent),
            text(active.label.clone())
                .size(11.5)
                .font(ui.config.ui_font)
                .color(theme.text),
        ]
        .spacing(6)
        .align_y(alignment::Vertical::Center);
        if running > 1 {
            chips = chips.push(
                container(
                    text(format!("+{}", running - 1))
                        .size(10)
                        .font(mono)
                        .color(theme.muted_text),
                )
                .padding(Padding::from([1, 5]))
                .style(move |_| chip_style(chip_background(theme.muted_text))),
            );
        }
        chips.into()
    } else {
        // Idle: a muted glyph that still opens the popover so finished entries
        // remain reachable.
        let glyph = if ui.activities.is_empty() {
            "\u{25CB}" // ○ no activity yet
        } else {
            "\u{2713}" // ✓ idle — everything finished
        };
        row![
            text(glyph).size(12).font(mono).color(theme.subtle_text),
            text("Activity")
                .size(11.5)
                .font(ui.config.ui_font)
                .color(theme.subtle_text),
        ]
        .spacing(6)
        .align_y(alignment::Vertical::Center)
        .into()
    };

    button(body)
        .padding(Padding::from([4, 9]))
        .on_press(Message::ActivityToggle)
        .style(move |_, status| button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Some(Background::Color(chip_background(theme.muted_text)))
                }
                _ => None,
            },
            text_color: theme.text,
            border: Border {
                width: 1.0,
                color: theme.border,
                radius: 6.0.into(),
            },
            shadow: Default::default(),
            snap: true,
        })
        .into()
}

/// The thin progress line drawn along the toolbar's bottom edge. Tracks the
/// first running activity (determinate → fractional fill; otherwise a pulsing
/// accent bar). `diff_loading` lights the line for a transient revision-switch
/// diff load that isn't a logged activity. A fixed 2px height keeps the toolbar
/// from shifting when idle.
pub fn activity_progress_line(ui: &Diffui, theme: ThemeSpec) -> Element<'static, Message> {
    const HEIGHT: f32 = 2.0;

    let active = ui.activities.first_running();
    let diff_loading = ui.pending_revision.is_some();

    // Determinate fill — only when the running op reports a real total.
    if let Some(activity) = active {
        let (loaded, total, determinate) = activity.progress_snapshot();
        if determinate {
            let fraction = (loaded as f32 / total as f32).clamp(0.0, 1.0);
            let fill = (fraction * 1000.0) as u16;
            let rest = 1000u16.saturating_sub(fill);
            let accent = theme.accent;
            let mut bar = row![].height(Length::Fixed(HEIGHT));
            if fill > 0 {
                bar = bar.push(
                    container(Space::new())
                        .width(Length::FillPortion(fill))
                        .height(Length::Fixed(HEIGHT))
                        .style(move |_| solid(accent)),
                );
            }
            if rest > 0 {
                bar = bar.push(Space::new().width(Length::FillPortion(rest)));
            }
            return bar.into();
        }
    }

    // Indeterminate (running with no total, or a diff load): a pulsing accent
    // bar. Opacity oscillates off elapsed time; the toolbar tick re-renders.
    if active.is_some() || diff_loading {
        let phase = active.map(|a| a.started.elapsed().as_millis()).unwrap_or(0) as f32 / 1000.0;
        // 0.30..0.85 sine pulse.
        let alpha = 0.30 + 0.55 * (0.5 + 0.5 * (phase * std::f32::consts::TAU * 0.8).sin());
        let color = Color {
            a: alpha,
            ..theme.accent
        };
        return container(Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(HEIGHT))
            .style(move |_| solid(color))
            .into();
    }

    // Idle: an invisible spacer so the toolbar height is stable.
    Space::new().height(Length::Fixed(HEIGHT)).into()
}

/// The activity popover: a scrim + a top-right card listing every entry. Empty
/// `Space` when closed.
pub fn activity_popover(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if !ui.activity_popover_open {
        return Space::new().into();
    }

    let scrim = mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_press(Message::ActivityToggle);

    let header = row![
        text("Activity")
            .size(13)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium))
            .color(theme.text),
        Space::new().width(Length::Fill),
        clear_button(ui, theme),
    ]
    .align_y(alignment::Vertical::Center)
    .spacing(8);

    let mut list = column![].spacing(2);
    if ui.activities.is_empty() {
        list = list.push(
            text("No activity yet.")
                .size(12)
                .font(ui.config.ui_font)
                .color(theme.subtle_text),
        );
    } else {
        // Newest first so the latest op is on top.
        for activity in ui.activities.activities.iter().rev() {
            list = list.push(activity_row(ui, theme, activity));
        }
    }

    let body = column![
        header,
        container(Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| solid(theme.border)),
        container(
            scrollable(list)
                .height(Length::Shrink)
                .style(move |_, s| iced_scrollable_style(theme, s)),
        )
        .max_height(440.0),
    ]
    .spacing(10);

    let card = mouse_area(
        container(body)
            .width(Length::Fixed(360.0))
            .padding(Padding::from([14, 16]))
            .style(move |_| card_style(theme)),
    )
    .on_press(Message::ActivityNoOp);

    // Anchor under the toolbar's right edge.
    let anchored = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Right)
        .padding(Padding {
            top: 78.0,
            right: 8.0,
            bottom: 0.0,
            left: 0.0,
        });

    stack![scrim, anchored].into()
}

fn activity_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    activity: &'a Activity,
) -> Element<'a, Message> {
    let mono = ui.config.mono_font;
    let (loaded, total, determinate) = activity.progress_snapshot();

    let (icon, icon_color) = match activity.status {
        ActivityStatus::Running => (spinner_glyph(activity.started).to_owned(), theme.accent),
        ActivityStatus::Done => ("\u{2713}".to_owned(), theme.added_text),
        ActivityStatus::Error => ("\u{2715}".to_owned(), theme.removed_text),
    };

    let label = activity.result.as_deref().unwrap_or(&activity.label);
    let mut head = row![
        text(icon).size(12).font(mono).color(icon_color),
        text(label.to_owned())
            .size(12)
            .font(ui.config.ui_font)
            .color(theme.text),
        Space::new().width(Length::Fill),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);

    if determinate {
        let pct = (loaded * 100 / total.max(1)).min(100);
        head = head.push(
            text(format!("{pct}%"))
                .size(11)
                .font(mono)
                .color(theme.muted_text),
        );
    } else if matches!(activity.status, ActivityStatus::Running) && loaded > 0 {
        // Indeterminate but counting (the jj lazy walk has no known total):
        // surface the running count.
        head = head.push(
            text(loaded.to_string())
                .size(11)
                .font(mono)
                .color(theme.muted_text),
        );
    }
    if !activity.detail.is_empty() {
        let chevron = if activity.expanded {
            "\u{25BE}"
        } else {
            "\u{25B8}"
        };
        head = head.push(text(chevron).size(11).font(mono).color(theme.subtle_text));
    }

    // The determinate progress bar sits under the header while running.
    let mut block = column![head].spacing(4);
    if matches!(activity.status, ActivityStatus::Running) && determinate {
        let accent = theme.accent;
        let fill = ((loaded as f32 / total as f32).clamp(0.0, 1.0) * 1000.0) as u16;
        let rest = 1000u16.saturating_sub(fill);
        let mut bar = row![].height(Length::Fixed(4.0));
        if fill > 0 {
            bar = bar.push(
                container(Space::new())
                    .width(Length::FillPortion(fill))
                    .height(Length::Fixed(4.0))
                    .style(move |_| rounded(accent)),
            );
        }
        if rest > 0 {
            bar = bar.push(
                container(Space::new())
                    .width(Length::FillPortion(rest))
                    .height(Length::Fixed(4.0))
                    .style(move |_| rounded(chip_background(theme.muted_text))),
            );
        }
        block = block.push(bar);
    }

    if activity.expanded {
        for line in &activity.detail {
            block = block.push(detail_line(line, mono, theme));
        }
    }

    let id = activity.id;
    let interactive = mouse_area(
        container(block)
            .width(Length::Fill)
            .padding(Padding::from([6, 8])),
    );
    // Only the rows with output toggle on click.
    if activity.detail.is_empty() {
        interactive.into()
    } else {
        interactive.on_press(Message::ActivityExpand(id)).into()
    }
}

/// Render one output line, turning whitespace-delimited `http(s)://…` tokens
/// into clickable links. Preserves the rest as monospace text.
fn detail_line<'a>(line: &'a str, mono: iced::Font, theme: ThemeSpec) -> Element<'a, Message> {
    if !line.contains("http://") && !line.contains("https://") {
        return text(line.to_owned())
            .size(11)
            .font(mono)
            .color(theme.muted_text)
            .into();
    }

    let mut row_widgets = row![].spacing(0).align_y(alignment::Vertical::Center);
    let mut first = true;
    for token in line.split(' ') {
        let token = if first {
            first = false;
            token.to_owned()
        } else {
            format!(" {token}")
        };
        let trimmed = token.trim_start();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            let url = trimmed.to_owned();
            row_widgets = row_widgets.push(
                button(text(token).size(11).font(mono).color(theme.info))
                    .padding(0)
                    .on_press(Message::OpenUrl(url))
                    .style(move |_, _| button::Style {
                        background: None,
                        text_color: theme.info,
                        border: Border {
                            width: 0.0,
                            color: Color::TRANSPARENT,
                            radius: 0.0.into(),
                        },
                        shadow: Default::default(),
                        snap: true,
                    }),
            );
        } else {
            row_widgets = row_widgets.push(text(token).size(11).font(mono).color(theme.muted_text));
        }
    }
    row_widgets.into()
}

fn clear_button(ui: &Diffui, theme: ThemeSpec) -> Element<'static, Message> {
    let enabled = ui
        .activities
        .activities
        .iter()
        .any(|a| !matches!(a.status, ActivityStatus::Running));
    let label = text("Clear")
        .size(11.5)
        .font(ui.config.ui_font)
        .color(if enabled {
            theme.muted_text
        } else {
            theme.subtle_text
        });
    let mut b = button(label)
        .padding(Padding::from([2, 8]))
        .style(move |_, status| button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Some(Background::Color(chip_background(theme.muted_text)))
                }
                _ => None,
            },
            text_color: theme.text,
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 6.0.into(),
            },
            shadow: Default::default(),
            snap: true,
        });
    if enabled {
        b = b.on_press(Message::ActivityClear);
    }
    b.into()
}

fn chip_style(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 4.0.into(),
        },
        ..container::Style::default()
    }
}

fn solid(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        ..container::Style::default()
    }
}

fn rounded(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 2.0.into(),
        },
        ..container::Style::default()
    }
}

fn card_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme.panel_background_elevated)),
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: 12.0.into(),
        },
        shadow: iced::Shadow {
            color: Color {
                a: 0.12,
                ..Color::BLACK
            },
            offset: iced::Vector::new(0.0, 3.0),
            blur_radius: 10.0,
        },
        ..container::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_run_finish_lifecycle() {
        let mut log = ActivityLog::default();
        assert!(log.is_empty());

        log.start(ActivityId(1), "Fetch", false);
        assert!(log.any_running());
        assert_eq!(log.running_count(), 1);
        assert_eq!(log.first_running().map(|a| a.label.as_str()), Some("Fetch"));

        log.append_output(ActivityId(1), "remote: hi");
        log.finish(ActivityId(1), ActivityStatus::Done, Some("done".to_owned()));
        assert!(!log.any_running());
        assert!(log.first_running().is_none());
    }

    #[test]
    fn first_running_follows_order() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "a", false);
        log.start(ActivityId(2), "b", false);
        // First *running* is the earliest still-running entry.
        assert_eq!(log.first_running().map(|a| a.label.as_str()), Some("a"));
        log.finish(ActivityId(1), ActivityStatus::Done, None);
        assert_eq!(log.first_running().map(|a| a.label.as_str()), Some("b"));
    }

    #[test]
    fn clear_finished_keeps_running() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "done-one", false);
        log.start(ActivityId(2), "still-running", false);
        log.finish(
            ActivityId(1),
            ActivityStatus::Error,
            Some("boom".to_owned()),
        );
        log.clear_finished();
        // The finished (errored) entry is gone; the running one survives.
        assert_eq!(log.running_count(), 1);
        assert_eq!(
            log.first_running().map(|a| a.label.as_str()),
            Some("still-running")
        );
    }
}

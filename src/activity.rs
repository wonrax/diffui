//! Per-tab activity / progress log: the single place every long-running
//! operation reports into — cold load, refresh, revset eval, fetch, undo, push.
//!
//! The toolbar shows the *active* tab's log: a right-edge indicator (spinner +
//! the first running task's label + a `+N` chip when several run at once) and a
//! thin progress line along the toolbar's bottom edge. Clicking the indicator
//! opens a popover listing every entry: a status icon, the title, a one-line
//! result summary underneath, and the elapsed time. Each line is cut to the
//! panel's width; a row with a line cut short expands to show it whole, and
//! rows with captured output expand into a code block showing the remote
//! sideband (e.g. GitHub's "Create a pull request" hint, with clickable URLs).
//!
//! Entries are **persistent**: running and finished alike stay until the user
//! clears them or the app restarts — nothing auto-dismisses.

use crate::overlay_kit::{Tone, count_chip, glyph};
use crate::theme::{self, Color, Font, Padding, Weight, emphasis_font, text_size};
use crate::typography::{self, Aligned};
use crate::ui::Tip as _;
use crate::{Action, Cx, ellipsis, icons, input, ui};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Div, MouseButton, Window, div, px, relative};
use std::time::{Duration, Instant};

use crate::Diffui;
use crate::prose::Prose;
use crate::theme::ThemeSpec;
use diffui_core::LoadProgress;

/// Frames for the running spinner. Braille-dot frames read as a smooth orbit
/// at small sizes (the cargo/npm convention) where the old `|/-\` ASCII set
/// looked like a teletype; advanced off the activity's elapsed time (the
/// toolbar tick keeps `view()` re-running while anything is in flight).
/// Falls back to `.notdef` boxes only if the mono font lacks Braille — every
/// bundled/system mono we target ships it.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long an operation must run before its spinner / progress line is painted.
/// Anything that finishes faster than this never shows a visual, so a quick
/// refresh / revset eval / mutation / diff load doesn't flash the orange
/// indicator. The op is still tracked the whole time (and lands in the popover
/// log on finish) — only the in-flight toolbar visual is held back.
pub const ACTIVITY_DISPLAY_DELAY: Duration = Duration::from_millis(150);

/// Stable identity for a log entry, handed back by [`ActivityLog::start`] so the
/// operation's later `append_output`/`finish` calls (and the row's expand
/// toggle) address the right entry as the list grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActivityId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    /// Accepted but not started — waiting behind an op that holds the same
    /// resource (e.g. a mutation queued behind one already running). Counts as
    /// unfinished (survives "Clear"), but doesn't drive the toolbar spinner.
    Queued,
    Running,
    Done,
    /// The operation never ran: a confirmation dialog was dismissed, or the
    /// job was superseded and cancelled. Distinct from `Done` because a log
    /// that reports work it didn't do is a log you can't check anything
    /// against.
    Cancelled,
    Error,
}

/// One tracked operation.
#[derive(Debug, Clone)]
pub struct Activity {
    pub id: ActivityId,
    pub label: Prose,
    pub status: ActivityStatus,
    /// Live progress handle the worker bumps from its thread; the UI polls it
    /// each frame. `total == 0` ⇒ indeterminate (spinner / pulsing line).
    pub progress: LoadProgress,
    /// Whether the op reports real progress. A determinate op still reads as
    /// indeterminate until its worker calls `set_total` (so the bar doesn't
    /// jump from a fake 0/0); an indeterminate op never sets a total.
    pub determinate: bool,
    /// Captured output lines (remote messages / errors), shown when expanded.
    /// Capped at [`MAX_OUTPUT_LINES`]; see [`Activity::dropped_output`].
    pub detail: Vec<String>,
    /// How many lines the cap has dropped off the front. The row says so
    /// rather than quietly showing a truncated buffer.
    pub dropped_output: usize,
    /// The `http(s)://` links in the captured output, first-seen order — the
    /// one-click links under the buffer. Collected as output lands, because
    /// `view` runs on every redraw and re-splitting the whole buffer there is
    /// a per-frame cost for as long as the row stays open.
    pub urls: Vec<String>,
    /// One-line summary recorded on finish, shown under the title.
    pub result: Option<String>,
    pub started: Instant,
    /// Wall time from start to finish, frozen by [`ActivityLog::finish`].
    pub duration: Option<Duration>,
    pub expanded: bool,
    /// The expanded row's output as one read-only `text_editor` buffer — a
    /// single buffer so a selection can sweep across lines (separate per-line
    /// widgets each hold their own). `Some` exactly while `expanded` with any
    /// `detail`; rebuilt from `detail` by [`Activity::sync_detail_editor`].
    pub detail_editor: Option<String>,
    /// The jj operation a finished mutation committed — arms the row's
    /// one-click "Revert" (reverts exactly this op, not just the latest).
    /// Cleared when fired, so it can't double-revert.
    pub undo_op: Option<String>,
    /// The jj command the operation is equivalent to, shown under the title
    /// (jj repositories only).
    pub command: Option<String>,
}

impl Activity {
    /// `(loaded, total)` and whether it should render as a determinate bar.
    fn progress_snapshot(&self) -> (usize, usize, bool) {
        let (loaded, total) = self.progress.snapshot();
        (loaded, total, self.determinate && total > 0)
    }

    /// (Re)build the output editor to mirror `detail`, or drop it when the
    /// row is collapsed (or has nothing to show). Called on expand/collapse
    /// and whenever output lands on an already-open row, so the buffer never
    /// goes stale.
    fn sync_detail_editor(&mut self) {
        self.detail_editor = (self.expanded && !self.detail.is_empty()).then(|| {
            let mut text = String::new();
            if self.dropped_output > 0 {
                text.push_str(&format!(
                    "… {} earlier lines dropped\n",
                    self.dropped_output
                ));
            }
            text.push_str(&self.detail.join("\n"));
            text
        });
    }

    /// Fold `lines` into the captured output: collect any links they carry,
    /// hold the buffer to [`MAX_OUTPUT_LINES`], and rebuild the editor.
    ///
    /// A link outlives the line that carried it on purpose — the one worth
    /// keeping is the "create a pull request" URL a chatty push buries under
    /// thousands of progress lines.
    fn take_output(&mut self, lines: impl IntoIterator<Item = String>) {
        for line in lines {
            for token in line.split_whitespace() {
                if (token.starts_with("http://") || token.starts_with("https://"))
                    && !self.urls.iter().any(|url| url == token)
                {
                    self.urls.push(token.to_owned());
                }
            }
            self.detail.push(line);
        }
        if self.detail.len() > MAX_OUTPUT_LINES {
            let excess = self.detail.len() - MAX_OUTPUT_LINES;
            self.detail.drain(..excess);
            self.dropped_output += excess;
        }
        self.sync_detail_editor();
    }
}

/// Most output lines one activity keeps. A fetch of a big repository, or a push
/// a server is chatty about, emits tens of thousands; the row shows a few at a
/// time and the whole buffer is re-shaped whenever it is rebuilt, so the oldest
/// go and the row says how many.
const MAX_OUTPUT_LINES: usize = 500;

/// Most finished rows a log keeps; the oldest go first. Running and queued
/// rows always stay.
const MAX_FINISHED: usize = 200;

/// The per-tab activity list; one lives in every tab's [`crate::TabState`].
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
        label: impl Into<Prose>,
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
            dropped_output: 0,
            urls: Vec::new(),
            result: None,
            started: Instant::now(),
            duration: None,
            expanded: false,
            detail_editor: None,
            undo_op: None,
            command: None,
        });
        self.trim();
        progress
    }

    /// Drop the oldest finished rows past [`MAX_FINISHED`].
    fn trim(&mut self) {
        let unfinished = |activity: &Activity| {
            matches!(
                activity.status,
                ActivityStatus::Running | ActivityStatus::Queued
            )
        };
        let mut excess = self
            .activities
            .iter()
            .filter(|activity| !unfinished(activity))
            .count()
            .saturating_sub(MAX_FINISHED);
        self.activities.retain(|activity| {
            if excess == 0 || unfinished(activity) {
                return true;
            }
            excess -= 1;
            false
        });
    }

    /// Record a standalone note — a warning the backend surfaced in passing
    /// (a snapshot that skipped an oversized file) that belongs to no
    /// particular operation. Lands as a finished row so it is visible in the
    /// popover without pretending anything is still running.
    pub fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        // Reuse the newest row when the same note repeats; a watcher tick that
        // re-skips the same file every second would otherwise fill the log.
        if self
            .activities
            .last()
            .is_some_and(|activity| activity.label.to_string() == text)
        {
            return;
        }
        self.activities.push(Activity {
            id: ActivityId(u64::MAX),
            label: text.into(),
            status: ActivityStatus::Done,
            progress: LoadProgress::default(),
            determinate: false,
            detail: Vec::new(),
            dropped_output: 0,
            urls: Vec::new(),
            result: None,
            started: Instant::now(),
            duration: Some(std::time::Duration::ZERO),
            expanded: false,
            detail_editor: None,
            undo_op: None,
            command: None,
        });
        self.trim();
    }

    pub fn append_output(&mut self, id: ActivityId, line: impl Into<String>) {
        if let Some(activity) = self.get_mut(id) {
            activity.take_output([line.into()]);
        }
    }

    /// Append several output lines at once (e.g. the sideband captured during a
    /// push/fetch, delivered together on completion).
    pub fn extend_output(&mut self, id: ActivityId, lines: impl IntoIterator<Item = String>) {
        if let Some(activity) = self.get_mut(id) {
            activity.take_output(lines);
        }
    }

    pub fn finish(&mut self, id: ActivityId, status: ActivityStatus, result: Option<String>) {
        if let Some(activity) = self.get_mut(id) {
            activity.status = status;
            activity.duration = Some(activity.started.elapsed());
            if result.is_some() {
                activity.result = result;
            }
            // A failure's output is what says why: it starts open.
            if status == ActivityStatus::Error && !activity.expanded {
                activity.expanded = true;
                activity.sync_detail_editor();
            }
        }
    }

    /// Flip an entry's status without recording a result — used to move a
    /// mutation between `Queued` and `Running` as the serial queue drains.
    pub fn set_status(&mut self, id: ActivityId, status: ActivityStatus) {
        if let Some(activity) = self.get_mut(id) {
            activity.status = status;
        }
    }

    /// Show the jj command the entry's operation is equivalent to.
    pub fn set_command(&mut self, id: ActivityId, command: Option<String>) {
        if let Some(activity) = self.get_mut(id) {
            activity.command = command;
        }
    }

    /// Arm a finished mutation row's per-op "Revert" with the operation it
    /// committed.
    pub fn set_undo_op(&mut self, id: ActivityId, operation_id: String) {
        if let Some(activity) = self.get_mut(id) {
            activity.undo_op = Some(operation_id);
        }
    }

    /// Disarm the per-op "Revert" (fired, or no longer safe to offer).
    pub fn clear_undo_op(&mut self, id: ActivityId) {
        if let Some(activity) = self.get_mut(id) {
            activity.undo_op = None;
        }
    }

    /// The entry's title — error toasts name what failed with it.
    pub fn label(&self, id: ActivityId) -> Option<&Prose> {
        self.activities
            .iter()
            .find(|activity| activity.id == id)
            .map(|activity| &activity.label)
    }

    pub fn toggle_expand(&mut self, id: ActivityId) {
        if let Some(activity) = self.get_mut(id) {
            activity.expanded = !activity.expanded;
            activity.sync_detail_editor();
        }
    }

    /// Drop finished entries, keeping anything still running or queued (so the
    /// log never hides pending work). Bound to the popover's "Clear".
    pub fn clear_finished(&mut self) {
        self.activities.retain(|activity| {
            matches!(
                activity.status,
                ActivityStatus::Running | ActivityStatus::Queued
            )
        });
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

    pub fn failed_count(&self) -> usize {
        self.activities
            .iter()
            .filter(|a| matches!(a.status, ActivityStatus::Error))
            .count()
    }

    pub fn queued_count(&self) -> usize {
        self.activities
            .iter()
            .filter(|a| matches!(a.status, ActivityStatus::Queued))
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

    /// Like [`first_running`](Self::first_running), but only once the op has run
    /// past [`ACTIVITY_DISPLAY_DELAY`] — so a short op never flashes the toolbar
    /// spinner / progress line. `first_running` is the *oldest* still-running
    /// entry, so if it isn't old enough to show, none are.
    pub fn first_running_visible(&self) -> Option<&Activity> {
        self.first_running()
            .filter(|a| a.started.elapsed() >= ACTIVITY_DISPLAY_DELAY)
    }

    /// Report how far a running op has got, for its determinate bar.
    pub fn set_progress(&mut self, id: ActivityId, loaded: usize, total: usize) {
        if let Some(activity) = self.get_mut(id) {
            activity.progress.set_total(total);
            activity.progress.set_loaded(loaded);
        }
    }

    fn get_mut(&mut self, id: ActivityId) -> Option<&mut Activity> {
        self.activities.iter_mut().find(|a| a.id == id)
    }
}

/// Spinner glyph for an activity that has been running for `elapsed`.
fn spinner_glyph(started: Instant) -> &'static str {
    let frame = (started.elapsed().as_millis() / 80) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[frame]
}

/// Height of [`activity_progress_line`], which the toolbar always reserves.
pub const PROGRESS_LINE_HEIGHT: f32 = 2.0;

/// Side of the fixed square box the status glyph is centered in, so every
/// row's title starts at the same x whether the glyph comes from the mono
/// font (the spinner) or the icon font.
const STATUS_ICON_BOX: f32 = 16.0;
/// Left inset that aligns the result, the command, the progress bar and the
/// output with the title text (status box + the head row's spacing).
const TITLE_INDENT: f32 = STATUS_ICON_BOX + 10.0;

/// Padding inside a row, each side.
const ROW_PAD_X: f32 = 10.0;
const ROW_PAD_Y: f32 = 9.0;
/// Space before each trailing item on the title line.
const TRAILING_GAP: f32 = 8.0;
const CHEVRON: f32 = 13.0;

/// Compact elapsed-time label: "340ms", "1.5s", "42s", "1m 12s".
fn format_duration(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 10_000 {
        format!("{:.1}s", duration.as_secs_f32())
    } else if ms < 60_000 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}m {}s", duration.as_secs() / 60, duration.as_secs() % 60)
    }
}

/// A round count badge on the indicator.
fn round_badge(label: String, fill: Color, ink: Color, font: Font) -> Aligned {
    let font = emphasis_font(font, Weight::SEMIBOLD);
    Aligned::chip(
        typography::line(label, font, text_size::BADGE, ink),
        font,
        text_size::BADGE,
        Padding::from([2.0, 5.0]),
        move |chip| chip.bg(fill).rounded(px(theme::radius::SM)),
    )
}

/// The right-edge toolbar control. While work is in flight: a spinner, the
/// first running task's label, `+N` for the others and how many wait. Idle:
/// "Up to date", or how many failed. A red count of the failures rides along
/// either way. Click → open/close the popover.
pub(crate) fn activity_indicator(
    ui: &Diffui,
    theme: ThemeSpec,
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let mono = ui.config.mono_font;
    let font = ui.config.ui_font;
    let activities = &ui.active().activities;
    let running = activities.running_count();
    let queued = activities.queued_count();
    let failed = activities.failed_count();
    let failed_badge = || {
        round_badge(
            failed.to_string(),
            theme.removed_text,
            theme.panel_background_elevated,
            font,
        )
    };

    // Spinner, label and count badges on one baseline (see `crate::typography`).
    let body = if let Some(active) = activities.first_running_visible() {
        let mut items = vec![
            Aligned::text(
                typography::line(
                    spinner_glyph(active.started),
                    mono,
                    text_size::UI,
                    theme.accent,
                ),
                mono,
                text_size::UI,
            ),
            Aligned::text(
                div()
                    .max_w(px(INDICATOR_LABEL_MAX))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(text_size::UI))
                    .child(active.label.view(font, mono, theme.muted_text, theme)),
                font,
                text_size::UI,
            )
            .shrink(),
        ];
        if running > 1 {
            items.push(round_badge(
                format!("+{}", running - 1),
                crate::palette::active_wash(theme),
                theme.subtle_text,
                font,
            ));
        }
        if queued > 0 {
            items.push(round_badge(
                format!("{queued} queued"),
                crate::palette::active_wash(theme),
                theme.subtle_text,
                font,
            ));
        }
        if failed > 0 {
            items.push(failed_badge());
        }
        typography::baseline_row(items, 6.0)
    } else if failed > 0 {
        typography::baseline_row(
            [
                Aligned::glyph(
                    icons::icon(glyph::X_CIRCLE, ui::ICON_SIZE, theme.removed_text),
                    ui::ICON_SIZE,
                ),
                Aligned::text(
                    typography::line(
                        format!("{failed} failed"),
                        font,
                        text_size::UI,
                        theme.muted_text,
                    ),
                    font,
                    text_size::UI,
                ),
            ],
            6.0,
        )
    } else {
        typography::baseline_row(
            [
                Aligned::glyph(
                    icons::icon(glyph::CHECK_CIRCLE, ui::ICON_SIZE, theme.added_text),
                    ui::ICON_SIZE,
                ),
                Aligned::text(
                    typography::line("Up to date", font, text_size::UI, theme.muted_text),
                    font,
                    text_size::UI,
                ),
            ],
            6.0,
        )
    };

    ui::button(
        "activity-indicator",
        ui::Variant::Ghost,
        ui::Size::Compact,
        theme,
        cx,
    )
    .when(ui.activity_popover_open(), |button| {
        button.resting(crate::palette::active_wash(theme), None)
    })
    .child(body)
    .on_click(Diffui::on_action(Action::ToggleActivityPopover, cx))
    .into_any_element()
}

/// The most the indicator's running label takes before it ellipsises.
const INDICATOR_LABEL_MAX: f32 = 200.0;

/// The thin progress line drawn along the toolbar's bottom edge. Tracks the
/// first running activity (determinate → fractional fill; otherwise a pulsing
/// accent bar). `diff_loading` lights the line for a transient revision-switch
/// diff load that isn't a logged activity. A fixed 2px height keeps the toolbar
/// from shifting when idle.
pub(crate) fn activity_progress_line(
    ui: &Diffui,
    theme: ThemeSpec,
    _window: &mut Window,
    _cx: &mut Cx,
) -> AnyElement {
    let line = div().w_full().flex_none().h(px(PROGRESS_LINE_HEIGHT));
    // Both visuals are held back until the work has run past the display delay,
    // so short ops don't flash. `loading_since` times the (un-logged)
    // revision-switch diff load the same way an activity's `started` does.
    let active = ui.active().activities.first_running_visible();
    let diff_loading = ui
        .active()
        .session
        .loading_since
        .is_some_and(|since| since.elapsed() >= ACTIVITY_DISPLAY_DELAY);

    // Determinate fill: only when the running op reports a real total.
    if let Some(activity) = active {
        let (loaded, total, determinate) = activity.progress_snapshot();
        if determinate {
            let fraction = (loaded as f32 / total as f32).clamp(0.0, 1.0);
            return line
                .child(
                    div()
                        .h(px(PROGRESS_LINE_HEIGHT))
                        .w(relative(fraction))
                        .bg(theme.accent),
                )
                .into_any_element();
        }
    }

    // Indeterminate (running with no total, or a diff load): a pulsing accent
    // bar. Opacity oscillates off elapsed time; the toolbar tick re-renders.
    if active.is_some() || diff_loading {
        let phase = active.map(|a| a.started.elapsed().as_millis()).unwrap_or(0) as f32 / 1000.0;
        // 0.30..0.85 sine pulse.
        let alpha = 0.30 + 0.55 * (0.5 + 0.5 * (phase * std::f32::consts::TAU * 0.8).sin());
        return line
            .bg(Color {
                a: alpha,
                ..theme.accent
            })
            .into_any_element();
    }

    // Idle: an invisible spacer so the toolbar height is stable.
    line.into_any_element()
}

/// The activity popover: a top-right card listing every entry, the work in
/// progress pinned over what has finished. Empty when closed.
pub(crate) fn activity_popover(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if !ui.activity_popover_open() {
        return div().into_any_element();
    }
    let font = ui.config.ui_font;
    let log = &ui.active().activities;
    let title_font = emphasis_font(font, Weight::SEMIBOLD);
    let mut title = vec![Aligned::text(
        typography::line("Activity", title_font, text_size::BODY, theme.text),
        title_font,
        text_size::BODY,
    )];
    for (count, what, tone) in [
        (log.running_count(), "running", Tone::Accent),
        (log.queued_count(), "queued", Tone::Neutral),
        (log.failed_count(), "failed", Tone::Danger),
    ] {
        if count > 0 {
            title.push(Aligned::gap(if title.len() == 1 { 10.0 } else { 4.0 }));
            title.push(count_chip(format!("{count} {what}"), tone, font, theme));
        }
    }
    let header = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .pt(px(10.0))
        .pb(px(theme::space::MD))
        .pl(px(theme::space::XL))
        .pr(px(10.0))
        .child(typography::baseline_row(title, 0.0).flex_1().min_w_0())
        .child(clear_button(ui, theme, cx));

    let (live, past): (Vec<&Activity>, Vec<&Activity>) =
        log.activities.iter().partition(|activity| {
            matches!(
                activity.status,
                ActivityStatus::Running | ActivityStatus::Queued
            )
        });
    let mut list = div().w_full().flex().flex_col();
    if log.is_empty() {
        list = list.child(
            div()
                .w_full()
                .flex()
                .justify_center()
                .py(px(28.0))
                .px(px(theme::space::XL))
                .child(typography::line(
                    "Nothing has run in this tab yet. Fetches, pushes and edits show up here.",
                    font,
                    text_size::UI,
                    theme.subtle_text,
                )),
        );
    }
    let heading = |label: &'static str| {
        crate::overlay_kit::group_heading(label, font, theme).px(px(ROW_PAD_X))
    };
    if !live.is_empty() {
        // In the order they run.
        list = list.child(heading("In progress"));
        for activity in live {
            list = list.child(activity_row(ui, theme, activity, window, cx));
        }
    }
    if !past.is_empty() {
        // Newest first, so the latest result is on top.
        list = list.child(heading("Finished"));
        for activity in past.into_iter().rev() {
            list = list.child(activity_row(ui, theme, activity, window, cx));
        }
    }

    // The card carries no padding of its own so the divider can run edge to
    // edge. The list scrolls out to the card's right edge, where the bar sits
    // as in every scroll view; the rows' own padding keeps their text clear.
    let card = theme::popover(div(), theme)
        .id("activity-popover-card")
        .w(px(POPOVER_WIDTH))
        .flex()
        .flex_col()
        .overflow_hidden()
        .occlude()
        .child(header)
        .child(crate::root::horizontal_divider(theme))
        .child(
            div()
                .w_full()
                .max_h(px(POPOVER_LIST_MAX_HEIGHT))
                .flex()
                .flex_col()
                .child(ui::scrollable(
                    "activity-list",
                    div()
                        .pt(px(2.0))
                        .pb(px(LIST_INSET))
                        .px(px(LIST_INSET))
                        .child(list),
                )),
        );

    // Hang just under the toolbar, its right edge on the toolbar's own edge inset
    // so the card lines up under the controls that opened it. `occlude` keeps the
    // wheel and the cursor from reaching the shell underneath.
    div()
        .id("activity-popover")
        .absolute()
        .inset_0()
        .occlude()
        .on_mouse_down(
            MouseButton::Left,
            Diffui::on_action(Action::ToggleActivityPopover, cx),
        )
        .child(
            div()
                .absolute()
                .top(px(crate::toolbar::chrome_bottom() + theme::space::XS))
                .right(px(theme::space::MD))
                .child(card.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())),
        )
        .into_any_element()
}

/// The floating error-toast stack: bottom-right cards for failed operations,
/// so a failure is visible without the activity popover open. Click a card's ✕
/// to dismiss; a slow tick prunes the rest after [`crate::TOAST_TTL`].
pub(crate) fn toast_layer(
    ui: &Diffui,
    theme: ThemeSpec,
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if ui.toasts.is_empty() {
        return div().into_any_element();
    }
    let title_font = emphasis_font(ui.config.ui_font, Weight::SEMIBOLD);
    // Only the stack occludes: it owns the cursor and wheel over its area, while
    // the rest of the window keeps working.
    let mut cards = div()
        .id("toasts")
        .absolute()
        .right(px(theme::space::LG))
        .bottom(px(theme::space::LG))
        .w(px(380.0))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .occlude();
    for toast in &ui.toasts {
        let id = toast.id;
        // The error color lives in the glyph + title; the card itself keeps the
        // shared quiet popover chrome so the stack doesn't read as four screaming
        // outlines. The title row is the close button's height, so the top and
        // right insets give up the margin its wash keeps around the glyph: the
        // warning glyph and the close glyph both sit 14px in from their card edges.
        let title = typography::baseline_row(
            [
                // A warning triangle, deliberately not an ✕: that glyph reads as
                // a dismiss button, not severity.
                Aligned::glyph(
                    icons::icon(icons::ALERT_TRIANGLE, 15.0, theme.removed_text),
                    15.0,
                ),
                Aligned::text(
                    div()
                        .w_full()
                        .text_size(px(text_size::BODY))
                        .child(toast.title.view(
                            title_font,
                            ui.config.mono_font,
                            theme.removed_text,
                            theme,
                        )),
                    title_font,
                    text_size::BODY,
                )
                .fill(),
                Aligned::glyph(
                    ui::close_button(("toast-close", id as usize), theme.muted_text, theme, cx)
                        .on_click(Diffui::listen(cx, move |this, _, _, _| {
                            this.on_toast_dismiss(id);
                        })),
                    ui::CLOSE_SIDE,
                ),
            ],
            8.0,
        );
        let detail = div()
            .w_full()
            .text_size(px(text_size::UI))
            .child(toast.detail.view(
                ui.config.ui_font,
                ui.config.mono_font,
                theme.muted_text,
                theme,
            ));
        cards = cards.child(
            theme::popover(div(), theme)
                .w_full()
                .flex()
                .flex_col()
                .gap(px(5.0))
                .pt(px(10.0))
                .pr(px(10.0))
                .pb(px(12.0))
                .pl(px(14.0))
                .child(title)
                .child(detail),
        );
    }
    cards.into_any_element()
}

/// A small button on a row: the title line's Revert and Output, the output's
/// links and Copy.
#[allow(clippy::too_many_arguments)]
fn row_button(
    ui: &Diffui,
    theme: ThemeSpec,
    id: (&'static str, u64),
    glyph: Option<&'static str>,
    label: impl Into<gpui_kit::SharedString>,
    variant: ui::Variant,
    on_press: Action,
    cx: &mut Cx,
) -> Div {
    let font = ui.config.ui_font;
    let ink = theme.muted_text;
    let mut line = Vec::new();
    if let Some(glyph) = glyph {
        line.push(Aligned::glyph(icons::icon(glyph, 12.0, ink), 12.0));
        line.push(Aligned::gap(5.0));
    }
    line.push(Aligned::text(
        typography::line(label, font, text_size::CAPTION, theme.text),
        font,
        text_size::CAPTION,
    ));
    press_stays(
        ui::button((id.0, id.1 as usize), variant, ui::Size::Compact, theme, cx)
            .h(px(SMALL_BUTTON))
            .px(px(7.0))
            .corner_radius(theme::radius::XS + 2.0)
            .child(typography::baseline_row(line, 0.0))
            .on_click(Diffui::on_action(on_press, cx)),
    )
}

/// Height of a row's small buttons.
const SMALL_BUTTON: f32 = 22.0;

/// A control inside a row that toggles on press: the press is the control's alone.
fn press_stays(control: impl IntoElement) -> Div {
    div()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(control)
}

/// The output field of a row. Fields are keyed by a static name, and rows come
/// and go, so names are shared out once per activity id.
fn detail_id(id: ActivityId) -> &'static str {
    thread_local! {
        static NAMES: std::cell::RefCell<std::collections::HashMap<u64, &'static str>> =
            Default::default();
    }
    NAMES.with(|names| {
        *names
            .borrow_mut()
            .entry(id.0)
            .or_insert_with(|| Box::leak(format!("activity-detail-{}", id.0).into_boxed_str()))
    })
}

/// What a link's button says: what the output around it says the link is for,
/// else just that it's a link.
fn link_label(activity: &Activity, url: &str) -> &'static str {
    let at = activity.detail.iter().position(|line| line.contains(url));
    let context = at
        .map(|at| {
            activity.detail[at.saturating_sub(1)..=at]
                .join(" ")
                .to_lowercase()
        })
        .unwrap_or_default();
    if url.contains("/pull/new/") || context.contains("create a pull request") {
        "Create pull request"
    } else if url.contains("/merge_requests/new") || context.contains("create a merge request") {
        "Create merge request"
    } else if url.contains("/pull/") {
        "Open pull request"
    } else {
        "Open link"
    }
}

fn activity_row(
    ui: &Diffui,
    theme: ThemeSpec,
    activity: &Activity,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let group: gpui_kit::SharedString = format!("activity-row-{}", activity.id.0).into();
    let error = matches!(activity.status, ActivityStatus::Error);
    let header = activity_header(ui, theme, activity, ROW_WIDTH, &group, window, cx);
    let row = div()
        .id(("activity-row", activity.id.0 as usize))
        .group(group)
        .w_full()
        .flex()
        .flex_col()
        .when(error, |row| {
            theme::squircle(
                row,
                theme::radius::MD,
                Some(Color {
                    a: 0.07,
                    ..theme.removed_text
                }),
                None,
            )
        })
        .when(!error, |row| {
            row.child(crate::squircle::hover_background(
                crate::squircle::Fill::new(crate::squircle::Corners::all(theme::radius::MD))
                    .background(Color {
                        a: 0.045,
                        ..theme.text
                    }),
                0.0,
            ))
        })
        .child(header);

    // Open: the captured output under the header, in one read-only editor
    // holding every line, so drag-selection sweeps across lines and ⌘C copies
    // the sweep. Long lines wrap. The links in it each get a button, and the
    // whole output a copy.
    let Some(detail) = activity
        .detail_editor
        .as_ref()
        .filter(|_| activity.expanded)
    else {
        return row.into_any_element();
    };
    let mono = ui.config.mono_font;
    let inner = ROW_WIDTH - 2.0 * ROW_PAD_X - TITLE_INDENT - 2.0 * WELL_PAD_X - 2.0;
    let line_height = (text_size::CAPTION * 1.6).ceil();
    let lines =
        crate::measure::wrapped_line_count(detail, text_size::CAPTION, mono, line_height, inner)
            .min(OUTPUT_MAX_LINES);
    let field = input::multi_line(
        input::FieldSpec::new(detail_id(activity.id), detail),
        mono,
        window,
        cx,
    )
    .appearance(false)
    .with_size(gpui_kit::component::Size::XSmall)
    .text_size(px(text_size::CAPTION))
    .line_height(px(line_height))
    .text_color(theme.muted_text)
    .h(px(lines as f32 * line_height));
    let well = div()
        .w_full()
        .py(px(theme::space::MD))
        .px(px(WELL_PAD_X))
        .overflow_hidden()
        .map(|well| {
            theme::squircle(
                well,
                theme::radius::SM,
                Some(crate::overlay_kit::inset_fill(theme)),
                Some(theme.border),
            )
        })
        .child(field);
    let mut actions = div().flex().flex_row().flex_wrap().gap(px(6.0));
    for (index, url) in activity.urls.iter().enumerate() {
        actions = actions.child(
            div()
                .id(("activity-link-tip", index))
                .tip(url.clone(), ui.config.ui_font, theme)
                .child(row_button(
                    ui,
                    theme,
                    ("activity-link", activity.id.0 * 64 + index as u64),
                    Some(glyph::EXTERNAL_LINK),
                    link_label(activity, url),
                    ui::Variant::Raised,
                    Action::OpenUrl(url.clone()),
                    cx,
                )),
        );
    }
    actions = actions.child(row_button(
        ui,
        theme,
        ("activity-copy", activity.id.0),
        Some(icons::COPY),
        "Copy output",
        ui::Variant::Ghost,
        Action::Copy(activity.detail.join("\n")),
        cx,
    ));
    row.child(
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(theme::space::MD))
            .pl(px(ROW_PAD_X + TITLE_INDENT))
            .pr(px(ROW_PAD_X))
            .pb(px(ROW_PAD_Y))
            .child(well)
            .child(actions),
    )
    .into_any_element()
}

/// The row's header: the title line, the result, the command and the
/// progress bar, `width` wide. A click on it toggles the row open when it
/// has output, or a line cut short; open, every line shows in full. The
/// output sits outside it, so dragging to select text there doesn't
/// collapse the row.
#[allow(clippy::too_many_arguments)]
fn activity_header(
    ui: &Diffui,
    theme: ThemeSpec,
    activity: &Activity,
    width: f32,
    group: &gpui_kit::SharedString,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let mono = ui.config.mono_font;
    let ui_font = ui.config.ui_font;
    let (loaded, total, determinate) = activity.progress_snapshot();

    // What sits right of the title: a count, the Revert and Output buttons, the
    // timing; each with its width, so the title knows its room.
    let mut trailing: Vec<(Aligned, f32)> = Vec::new();
    let caption = |value: String, font: Font, color: Color| {
        let width = ellipsis::text_width(&value, text_size::CAPTION, font);
        (
            Aligned::text(
                typography::line(value, font, text_size::CAPTION, color),
                font,
                text_size::CAPTION,
            ),
            width,
        )
    };
    let button_width = |label: &str, glyph: bool| {
        ellipsis::text_width(label, text_size::CAPTION, ui_font)
            + 14.0
            + if glyph { 17.0 } else { 0.0 }
    };
    match activity.status {
        ActivityStatus::Running => {
            if !determinate && loaded > 0 {
                // Indeterminate but counting (the jj lazy walk has no known
                // total): surface the running count.
                trailing.push(caption(loaded.to_string(), mono, theme.muted_text));
            }
            trailing.push(caption(
                format_duration(activity.started.elapsed()),
                ui_font,
                theme.subtle_text,
            ));
        }
        ActivityStatus::Queued => {
            trailing.push(caption("queued".to_owned(), ui_font, theme.subtle_text));
        }
        ActivityStatus::Done | ActivityStatus::Cancelled | ActivityStatus::Error => {
            // A finished mutation offers a one-click revert of exactly its op
            // (`jj op revert`), on hover. Labelled apart from the toolbar's
            // Undo, which steps back through the op log instead.
            if matches!(activity.status, ActivityStatus::Done)
                && let Some(operation_id) = activity.undo_op.clone()
            {
                let revert = row_button(
                    ui,
                    theme,
                    ("activity-revert", activity.id.0),
                    Some(icons::UNDO),
                    "Revert",
                    ui::Variant::Ghost,
                    Action::UndoOperation(activity.id, operation_id),
                    cx,
                )
                .opacity(0.0)
                .group_hover(group.clone(), |button| button.opacity(1.0));
                trailing.push((
                    Aligned::glyph(revert, SMALL_BUTTON),
                    button_width("Revert", true),
                ));
            }
            // A jj CLI run reopens its output panel.
            if ui
                .active()
                .cmdline
                .runs
                .iter()
                .any(|run| run.activity == activity.id)
            {
                trailing.push((
                    Aligned::glyph(
                        row_button(
                            ui,
                            theme,
                            ("activity-output", activity.id.0),
                            None,
                            "Output",
                            ui::Variant::Ghost,
                            Action::CommandLine(crate::cmdline::CmdAction::Output(
                                crate::cli_output::OutputAction::Open(activity.id),
                            )),
                            cx,
                        ),
                        SMALL_BUTTON,
                    ),
                    button_width("Output", false),
                ));
            }
            if let Some(duration) = activity.duration {
                trailing.push(caption(
                    format_duration(duration),
                    ui_font,
                    theme.subtle_text,
                ));
            }
        }
    }

    // Collapsed, each line is cut to its room; whether any had to be is what makes
    // the row expandable without output.
    let text_width = width - 2.0 * ROW_PAD_X - TITLE_INDENT;
    let trailing_width: f32 = trailing.iter().map(|(_, width)| TRAILING_GAP + width).sum();
    let error = matches!(activity.status, ActivityStatus::Error);
    let cancelled = matches!(activity.status, ActivityStatus::Cancelled);
    let title_color = if cancelled {
        theme.subtle_text
    } else {
        theme.text
    };
    let title = activity
        .label
        .line(ui_font, mono, title_color, theme, text_size::BODY);
    let result_color = if error {
        theme.removed_text
    } else {
        theme.muted_text
    };
    let result = activity.result.as_deref().map(|result| {
        crate::prose::Prose::from(result).line(ui_font, mono, result_color, theme, text_size::UI)
    });
    let cut_short = !title.fits(text_width - trailing_width)
        || activity.command.as_deref().is_some_and(|command| {
            !crate::plan::command_fits(ui, theme, command, text_size::CAPTION, text_width)
        })
        || result
            .as_ref()
            .is_some_and(|result| !result.fits(text_width));
    let expandable = !activity.detail.is_empty() || cut_short;
    let open = activity.expanded && expandable;
    let chevron_width = if expandable {
        TRAILING_GAP + CHEVRON
    } else {
        0.0
    };

    // The title and the timing share a baseline; the status glyph, the buttons
    // and the chevron centre on the caps (see `crate::typography`).
    let title = if open {
        title.wrapped().w_full()
    } else {
        title.cut(text_width - trailing_width - chevron_width)
    };
    let mut head: Vec<Aligned> = vec![
        Aligned::glyph(status_icon(activity, theme, mono), STATUS_ICON_BOX),
        Aligned::gap(10.0),
        Aligned::text(title, ui_font, text_size::BODY).fill(),
    ];
    for (item, _) in trailing {
        head.push(Aligned::gap(TRAILING_GAP));
        head.push(item);
    }
    if expandable {
        let chevron = if open {
            icons::CHEVRON_UP
        } else {
            icons::CHEVRON_DOWN
        };
        head.push(Aligned::gap(TRAILING_GAP));
        head.push(Aligned::glyph(
            icons::icon(chevron, CHEVRON, theme.subtle_text),
            CHEVRON,
        ));
    }
    let indented = || div().w_full().pl(px(TITLE_INDENT));
    let mut header = div()
        .id(("activity-header", activity.id.0 as usize))
        .w_full()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .py(px(ROW_PAD_Y))
        .px(px(ROW_PAD_X))
        .child(typography::baseline_row(head, 0.0));
    if let Some(result) = result {
        header = header.child(indented().child(if open {
            result.wrapped().w_full()
        } else {
            result.cut(text_width)
        }));
    }
    if let Some(command) = activity.command.as_deref() {
        header = header.child(indented().child(if open {
            crate::plan::wrapped_command_line(ui, theme, command, text_size::CAPTION, window, cx)
        } else {
            crate::plan::caption_command_line(ui, theme, command, window, cx)
        }));
    }
    if matches!(activity.status, ActivityStatus::Running) && determinate {
        let fraction = (loaded as f32 / total as f32).clamp(0.0, 1.0);
        header = header.child(
            indented()
                .pt(px(theme::space::XS))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .h(px(4.0))
                        .rounded(px(2.0))
                        .bg(crate::palette::active_wash(theme))
                        .child(
                            div()
                                .h_full()
                                .w(relative(fraction))
                                .rounded(px(2.0))
                                .bg(theme.accent),
                        ),
                )
                .child(typography::line(
                    format!("{loaded} / {total}"),
                    ui_font,
                    text_size::CAPTION,
                    theme.subtle_text,
                )),
        );
    }
    if expandable {
        let id = activity.id;
        header = header.cursor_pointer().on_mouse_down(
            MouseButton::Left,
            Diffui::listen(cx, move |this, _, _, _| this.on_activity_expand(id)),
        );
    }
    header.into_any_element()
}

/// Status glyph centered in the fixed [`STATUS_ICON_BOX`] square.
fn status_icon(activity: &Activity, theme: ThemeSpec, mono: Font) -> AnyElement {
    let icon = |glyph: &'static str, color: Color| {
        div()
            .size(px(STATUS_ICON_BOX))
            .flex()
            .items_center()
            .justify_center()
            .child(icons::icon(glyph, ui::ICON_SIZE, color))
            .into_any_element()
    };
    match activity.status {
        ActivityStatus::Queued => icon(icons::CLOCK, theme.subtle_text),
        ActivityStatus::Running => mono_glyph(spinner_glyph(activity.started), mono, theme.accent),
        ActivityStatus::Done => icon(glyph::CHECK_CIRCLE, theme.added_text),
        ActivityStatus::Cancelled => icon(glyph::SLASH_CIRCLE, theme.subtle_text),
        ActivityStatus::Error => icon(glyph::X_CIRCLE, theme.removed_text),
    }
}

/// A mono-font glyph boxed like [`icons::icon`] so both glyph sources center
/// identically.
fn mono_glyph(glyph: &'static str, mono: Font, color: Color) -> AnyElement {
    div()
        .size(px(STATUS_ICON_BOX))
        .flex()
        .items_center()
        .justify_center()
        .font(mono.gpui())
        .text_size(px(text_size::BODY))
        .line_height(relative(1.0))
        .text_color(color)
        .child(glyph)
        .into_any_element()
}

/// "Clear finished": drops what's done, keeps what runs or waits.
fn clear_button(ui: &Diffui, theme: ThemeSpec, cx: &mut Cx) -> AnyElement {
    let enabled = ui.active().activities.activities.iter().any(|a| {
        matches!(
            a.status,
            ActivityStatus::Done | ActivityStatus::Cancelled | ActivityStatus::Error
        )
    });
    let color = if enabled {
        theme.muted_text
    } else {
        theme.subtle_text
    };
    let mut button = ui::button(
        "activity-clear",
        ui::Variant::Ghost,
        ui::Size::Compact,
        theme,
        cx,
    )
    .h(px(24.0))
    .px(px(theme::space::MD))
    .child(typography::centered(
        typography::line("Clear finished", ui.config.ui_font, text_size::UI, color),
        ui.config.ui_font,
        text_size::UI,
    ));
    if enabled {
        button = button.on_click(Diffui::on_action(Action::ClearActivities, cx));
    }
    div()
        .id("activity-clear-tip")
        .flex_none()
        .tip(
            "Remove finished entries. Running and queued work stays.",
            ui.config.ui_font,
            theme,
        )
        .child(button)
        .into_any_element()
}

const POPOVER_WIDTH: f32 = 470.0;
const POPOVER_LIST_MAX_HEIGHT: f32 = 520.0;
/// Inset of the list's rows inside the card.
const LIST_INSET: f32 = 6.0;
/// A row's width in the popover: the card less its border and the list's insets.
const ROW_WIDTH: f32 = POPOVER_WIDTH - 2.0 - 2.0 * LIST_INSET;
/// The output well's inner inset.
const WELL_PAD_X: f32 = 10.0;
/// Lines of output an open row shows before its well scrolls.
const OUTPUT_MAX_LINES: usize = 10;

#[cfg(test)]
impl ActivityLog {
    /// The first row with captured output, for scenes that expand one.
    pub(crate) fn first_with_output(&self) -> Option<ActivityId> {
        self.activities
            .iter()
            .find(|activity| !activity.detail.is_empty())
            .map(|activity| activity.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_running_rows_and_the_newest_finished_ones() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(0), "Push", false);
        for id in 1..=MAX_FINISHED as u64 + 5 {
            log.start(ActivityId(id), "Fetch", false);
            log.finish(ActivityId(id), ActivityStatus::Done, None);
        }
        log.start(ActivityId(1000), "Fetch", false);
        assert!(log.label(ActivityId(0)).is_some(), "a running row stays");
        assert!(log.label(ActivityId(5)).is_none(), "the oldest finished go");
        assert!(log.label(ActivityId(6)).is_some());
        assert_eq!(log.activities.len(), MAX_FINISHED + 2);
    }

    #[test]
    fn start_run_finish_lifecycle() {
        let mut log = ActivityLog::default();
        assert!(log.is_empty());

        log.start(ActivityId(1), "Fetch", false);
        assert!(log.any_running());
        assert_eq!(log.running_count(), 1);
        assert_eq!(
            log.first_running().map(|a| a.label.to_string()),
            Some("Fetch".to_owned())
        );

        log.append_output(ActivityId(1), "remote: hi");
        log.finish(ActivityId(1), ActivityStatus::Done, Some("done".to_owned()));
        assert!(!log.any_running());
        assert!(log.first_running().is_none());
    }

    #[test]
    fn output_is_capped_and_its_links_survive_the_cap() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "Push", false);
        log.append_output(ActivityId(1), "remote: https://example.test/pull/new");
        for index in 0..MAX_OUTPUT_LINES + 200 {
            log.append_output(ActivityId(1), format!("progress {index}"));
        }

        let activity = &log.activities[0];
        assert_eq!(activity.detail.len(), MAX_OUTPUT_LINES);
        assert_eq!(activity.dropped_output, 201);
        assert_eq!(
            activity.detail.last().map(String::as_str),
            Some("progress 699"),
            "the newest output is what is kept"
        );
        // The link the push was actually for was on the very first line.
        assert_eq!(activity.urls, vec!["https://example.test/pull/new"]);
    }

    #[test]
    fn expand_builds_output_editor_and_collapse_drops_it() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "Push", false);
        log.append_output(ActivityId(1), "remote: hi");
        // Collapsed: no editor even though output exists.
        assert!(log.activities[0].detail_editor.is_none());

        log.toggle_expand(ActivityId(1));
        let editor = log.activities[0].detail_editor.as_ref().expect("built");
        assert_eq!(editor.trim_end(), "remote: hi");

        // Output landing while the row is open keeps the buffer in sync.
        log.append_output(ActivityId(1), "remote: bye");
        let editor = log.activities[0].detail_editor.as_ref().expect("kept");
        assert_eq!(editor.trim_end(), "remote: hi\nremote: bye");

        log.toggle_expand(ActivityId(1));
        assert!(log.activities[0].detail_editor.is_none());
    }

    #[test]
    fn a_failure_opens_on_its_output() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "Push", false);
        log.append_output(
            ActivityId(1),
            "fatal: Could not read from remote repository.",
        );
        log.finish(
            ActivityId(1),
            ActivityStatus::Error,
            Some("failed".to_owned()),
        );
        assert!(log.activities[0].expanded);
        assert!(log.activities[0].detail_editor.is_some());
        assert_eq!(log.failed_count(), 1);
    }

    #[test]
    fn finish_freezes_duration() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "Fetching all remotes", false);
        assert!(log.activities[0].duration.is_none());
        log.finish(ActivityId(1), ActivityStatus::Done, None);
        assert!(log.activities[0].duration.is_some());
    }

    #[test]
    fn first_running_follows_order() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "a", false);
        log.start(ActivityId(2), "b", false);
        // First *running* is the earliest still-running entry.
        assert_eq!(
            log.first_running().map(|a| a.label.to_string()),
            Some("a".to_owned())
        );
        log.finish(ActivityId(1), ActivityStatus::Done, None);
        assert_eq!(
            log.first_running().map(|a| a.label.to_string()),
            Some("b".to_owned())
        );
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
            log.first_running().map(|a| a.label.to_string()),
            Some("still-running".to_owned())
        );
    }

    #[test]
    fn queued_entry_does_not_run_but_survives_clear() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "running", false);
        log.start(ActivityId(2), "queued", false);
        log.set_status(ActivityId(2), ActivityStatus::Queued);

        // Queued doesn't count as running and never drives the spinner.
        assert_eq!(log.running_count(), 1);
        assert_eq!(
            log.first_running().map(|a| a.label.to_string()),
            Some("running".to_owned())
        );

        // Clearing keeps both the running and the queued entry.
        log.finish(ActivityId(1), ActivityStatus::Done, None);
        log.clear_finished();
        assert_eq!(log.activities.len(), 1);
        assert_eq!(log.activities[0].label.to_string(), "queued");

        // When the queue drains it starts running for real.
        log.set_status(ActivityId(2), ActivityStatus::Running);
        assert_eq!(log.running_count(), 1);
        assert_eq!(
            log.first_running().map(|a| a.label.to_string()),
            Some("queued".to_owned())
        );
    }
}

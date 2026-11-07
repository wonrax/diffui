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

use crate::Cx;
use crate::modes::ModeKind;
use gpui_kit::{AnyElement, Window};
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
            (&text).to_owned()
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
        progress
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
/// font (spinner, queued `…`) or the icon font (check / cross).
const STATUS_ICON_BOX: f32 = 14.0;
/// Left inset that aligns the subtitle / progress bar / code block with the
/// title text (status box + the head row's spacing).
const TITLE_INDENT: f32 = STATUS_ICON_BOX + 8.0;

/// Horizontal padding inside the row's header, each side.
const ROW_PAD_X: f32 = 8.0;
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

/// Stub: the activity view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn activity_indicator(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("activity::activity_indicator")
}

/// Stub: the activity view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn activity_progress_line(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("activity::activity_progress_line")
}

/// Stub: the activity view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn activity_popover(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::overlay(
        "activity::activity_popover",
        ui.mode(ModeKind::ActivityPopover).is_some(),
    )
}

/// Stub: the activity view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn toast_layer(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::overlay("activity::toast_layer", !ui.toasts.is_empty())
}

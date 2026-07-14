//! Per-tab activity / progress log: the single place every long-running
//! operation reports into — cold load, refresh, revset eval, fetch, undo, push.
//!
//! The toolbar shows the *active* tab's log: a right-edge indicator (spinner +
//! the first running task's label + a `+N` chip when several run at once) and a
//! thin progress line along the toolbar's bottom edge. Clicking the indicator
//! opens a popover listing every entry: a status icon, the title, a one-line
//! result summary underneath, and the elapsed time. Rows with captured output
//! expand into a code block showing the remote sideband (e.g. GitHub's
//! "Create a pull request" hint, with clickable URLs).
//!
//! Entries are **persistent**: running and finished alike stay until the user
//! clears them or the app restarts — nothing auto-dismisses.

use std::time::{Duration, Instant};

use iced::{
    Background, Border, Color, Element, Length, Padding, alignment,
    font::Weight,
    widget::{
        Space, button, column, container, mouse_area, opaque, row, scrollable, stack, text,
        text_editor,
    },
};

use crate::chip;
use crate::icons;
use crate::theme::{
    ThemeSpec, chip_background, emphasis_font, ghost_button_style, iced_scrollable_style,
    popover_style, text_size,
};
use crate::{Diffui, Message};
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
    pub detail_editor: Option<text_editor::Content>,
    /// The jj operation a finished mutation committed — arms the row's
    /// one-click "Undo" (reverts exactly this op, not just the latest).
    /// Cleared when fired, so it can't double-revert.
    pub undo_op: Option<String>,
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
        self.detail_editor = (self.expanded && !self.detail.is_empty())
            .then(|| text_editor::Content::with_text(&self.detail.join("\n")));
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
            duration: None,
            expanded: false,
            detail_editor: None,
            undo_op: None,
        });
        progress
    }

    pub fn append_output(&mut self, id: ActivityId, line: impl Into<String>) {
        if let Some(activity) = self.get_mut(id) {
            activity.detail.push(line.into());
            activity.sync_detail_editor();
        }
    }

    /// Append several output lines at once (e.g. the sideband captured during a
    /// push/fetch, delivered together on completion).
    pub fn extend_output(&mut self, id: ActivityId, lines: impl IntoIterator<Item = String>) {
        if let Some(activity) = self.get_mut(id) {
            activity.detail.extend(lines);
            activity.sync_detail_editor();
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

    /// Arm a finished mutation row's per-op "Undo" with the operation it
    /// committed.
    pub fn set_undo_op(&mut self, id: ActivityId, operation_id: String) {
        if let Some(activity) = self.get_mut(id) {
            activity.undo_op = Some(operation_id);
        }
    }

    /// Disarm the per-op "Undo" (fired, or no longer safe to offer).
    pub fn clear_undo_op(&mut self, id: ActivityId) {
        if let Some(activity) = self.get_mut(id) {
            activity.undo_op = None;
        }
    }

    /// The entry's title — error toasts name what failed with it.
    pub fn label(&self, id: ActivityId) -> Option<&str> {
        self.activities
            .iter()
            .find(|activity| activity.id == id)
            .map(|activity| activity.label.as_str())
    }

    pub fn toggle_expand(&mut self, id: ActivityId) {
        if let Some(activity) = self.get_mut(id) {
            activity.expanded = !activity.expanded;
            activity.sync_detail_editor();
        }
    }

    /// Apply a `text_editor` action to an expanded row's output buffer.
    /// Edit actions are dropped here — the buffer is a read-only view of the
    /// captured output; only caret, selection, and scroll go through.
    pub fn perform_detail_action(&mut self, id: ActivityId, action: text_editor::Action) {
        if action.is_edit() {
            return;
        }
        if let Some(activity) = self.get_mut(id)
            && let Some(editor) = activity.detail_editor.as_mut()
        {
            editor.perform(action);
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

/// The right-edge toolbar control: a clickable chip that shows the first
/// running task (spinner + label + `+N`) while work is in flight, or a quiet
/// "Activity" affordance otherwise. Click → open/close the popover.
pub fn activity_indicator(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let mono = ui.config.mono_font;
    let running = ui.activities.running_count();
    let queued = ui.activities.queued_count();

    let body: Element<'_, Message> = if let Some(active) = ui.activities.first_running_visible() {
        let mut chips = row![
            text(spinner_glyph(active.started))
                .size(text_size::UI)
                .font(mono)
                .color(theme.accent),
            text(active.label.as_str())
                .size(text_size::UI)
                .font(ui.config.ui_font)
                .color(theme.text),
        ]
        .spacing(6)
        .align_y(alignment::Vertical::Center);
        if running > 1 {
            chips = chips.push(
                container(
                    text(format!("+{}", running - 1))
                        .size(text_size::BADGE)
                        .font(mono)
                        .color(theme.muted_text),
                )
                .padding(Padding::from([1, 5]))
                .style(move |_| chip::container_style(theme.muted_text)),
            );
        }
        if queued > 0 {
            chips = chips.push(
                container(
                    text(format!("{queued} queued"))
                        .size(text_size::BADGE)
                        .font(mono)
                        .color(theme.subtle_text),
                )
                .padding(Padding::from([1, 5]))
                .style(move |_| chip::container_style(theme.subtle_text)),
            );
        }
        chips.into()
    } else {
        // Idle: a muted glyph that still opens the popover so finished entries
        // remain reachable.
        let glyph = if ui.activities.is_empty() {
            icons::CIRCLE // no activity yet
        } else {
            icons::CHECK // idle — everything finished
        };
        row![
            text(glyph)
                .size(text_size::UI)
                .font(icons::ICON_FONT)
                .color(theme.subtle_text),
            text("Activity")
                .size(text_size::UI)
                .font(ui.config.ui_font)
                .color(theme.subtle_text),
        ]
        .spacing(6)
        .align_y(alignment::Vertical::Center)
        .into()
    };

    button(body)
        .padding(Padding::from([5, 10]))
        .on_press(Message::ActivityToggle)
        .style(move |_, status| ghost_button_style(theme, status))
        .into()
}

/// The thin progress line drawn along the toolbar's bottom edge. Tracks the
/// first running activity (determinate → fractional fill; otherwise a pulsing
/// accent bar). `diff_loading` lights the line for a transient revision-switch
/// diff load that isn't a logged activity. A fixed 2px height keeps the toolbar
/// from shifting when idle.
pub fn activity_progress_line(ui: &Diffui, theme: ThemeSpec) -> Element<'static, Message> {
    const HEIGHT: f32 = 2.0;

    // Both visuals are held back until the work has run past the display delay,
    // so short ops don't flash. `loading_since` times the (un-logged)
    // revision-switch diff load the same way an activity's `started` does.
    let active = ui.activities.first_running_visible();
    let diff_loading = ui
        .session
        .loading_since
        .is_some_and(|since| since.elapsed() >= ACTIVITY_DISPLAY_DELAY);

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
            .size(text_size::BODY_LG)
            .font(emphasis_font(ui.config.ui_font, Weight::Semibold))
            .color(theme.text),
        Space::new().width(Length::Fill),
        clear_button(ui, theme),
    ]
    .align_y(alignment::Vertical::Center)
    .spacing(8);

    let mut list = column![].spacing(2);
    if ui.activities.is_empty() {
        list = list.push(
            container(
                text("No activity yet.")
                    .size(text_size::UI)
                    .font(ui.config.ui_font)
                    .color(theme.subtle_text),
            )
            .padding(Padding::from([6, 8])),
        );
    } else {
        // Newest first so the latest op is on top.
        for activity in ui.activities.activities.iter().rev() {
            list = list.push(activity_row(ui, theme, activity));
        }
    }

    // The card carries no padding of its own so the divider can run edge to
    // edge; the header/list insets are chosen so the rows' status icons (list
    // pad 8 + row pad 8) sit exactly under the header text (pad 16), and the
    // Clear label's right edge (pad 8 + button pad 8) over the durations.
    let body = column![
        container(header).width(Length::Fill).padding(Padding {
            top: 12.0,
            right: 8.0,
            bottom: 10.0,
            left: 16.0,
        }),
        container(Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| solid(theme.border)),
        container(
            scrollable(list)
                .height(Length::Shrink)
                .style(move |_, s| iced_scrollable_style(theme, s)),
        )
        .max_height(520.0)
        .padding(Padding {
            top: 6.0,
            right: 8.0,
            bottom: 8.0,
            left: 8.0,
        }),
    ];

    let card = mouse_area(
        container(body)
            .width(Length::Fixed(420.0))
            .style(move |_| popover_style(theme)),
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

    // `opaque` makes the whole layer report a mouse interaction, which the
    // view's outer `stack!` uses to levitate the cursor away from the shell
    // underneath. Without it the popover was mouse-transparent wherever it
    // didn't handle events itself: wheel scrolling over the card scrolled the
    // diff view behind it, and the diff text's I-beam cursor bled through.
    opaque(stack![scrim, anchored])
}

/// The floating error-toast stack: bottom-right cards for failed operations,
/// so a failure is visible without the activity popover open. Click a card
/// to dismiss; a slow tick prunes the rest after [`crate::TOAST_TTL`].
pub fn toast_layer(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if ui.toasts.is_empty() {
        return Space::new().into();
    }
    let mut cards = column![].spacing(8).width(Length::Fixed(380.0));
    for toast in &ui.toasts {
        // The error color lives in the glyph + title; the card itself keeps
        // the shared quiet popover chrome so the stack doesn't read as four
        // screaming outlines.
        let body = column![
            row![
                // A warning triangle, deliberately not an ✕ — that glyph
                // reads as a dismiss button, not severity.
                icons::icon(icons::ALERT_TRIANGLE, 15.0, theme.removed_text),
                text(toast.title.as_str())
                    .size(text_size::BODY)
                    .font(emphasis_font(ui.config.ui_font, Weight::Semibold))
                    .color(theme.removed_text)
                    .width(Length::Fill),
                button(icons::icon(icons::CLOSE, 13.0, theme.muted_text))
                    .padding(Padding::from([2, 4]))
                    .on_press(Message::ToastDismiss(toast.id))
                    .style(move |_, status| ghost_button_style(theme, status)),
            ]
            .spacing(8)
            .align_y(alignment::Vertical::Center),
            text(toast.detail.as_str())
                .size(text_size::UI)
                .font(ui.config.ui_font)
                .color(theme.muted_text),
        ]
        .spacing(5);
        cards = cards.push(
            container(body)
                .width(Length::Fill)
                .padding(Padding::from([12, 14]))
                .style(move |_| popover_style(theme)),
        );
    }
    // Anchored bottom-right, floating above the status bar. Only the card
    // stack is `opaque` — it must own the cursor and wheel over its area
    // (without it the diff view's I-beam and scrolling bled through, like
    // the activity popover once did) — while the full-size aligning wrapper
    // stays transparent so the rest of the window keeps working.
    container(opaque(cards))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Right)
        .align_y(alignment::Vertical::Bottom)
        .padding(Padding {
            top: 0.0,
            right: 14.0,
            bottom: 46.0,
            left: 0.0,
        })
        .into()
}

/// Side of the fixed square box the status glyph is centered in, so every
/// row's title starts at the same x whether the glyph comes from the mono
/// font (spinner, queued `…`) or the icon font (check / cross).
const STATUS_ICON_BOX: f32 = 14.0;
/// Left inset that aligns the subtitle / progress bar / code block with the
/// title text (status box + the head row's spacing).
const TITLE_INDENT: f32 = STATUS_ICON_BOX + 8.0;

fn activity_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    activity: &'a Activity,
) -> Element<'a, Message> {
    let mono = ui.config.mono_font;
    let (loaded, total, determinate) = activity.progress_snapshot();
    let expandable = !activity.detail.is_empty();

    let mut head = row![
        status_icon(activity, theme, mono),
        text(activity.label.as_str())
            .size(text_size::UI)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium))
            .color(theme.text)
            .wrapping(text::Wrapping::None)
            .ellipsis(text::Ellipsis::End)
            .width(Length::Fill),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);

    if matches!(activity.status, ActivityStatus::Running) {
        if determinate {
            let pct = (loaded * 100 / total.max(1)).min(100);
            head = head.push(
                text(format!("{pct}%"))
                    .size(text_size::CAPTION)
                    .font(mono)
                    .color(theme.muted_text),
            );
        } else if loaded > 0 {
            // Indeterminate but counting (the jj lazy walk has no known total):
            // surface the running count.
            head = head.push(
                text(loaded.to_string())
                    .size(text_size::CAPTION)
                    .font(mono)
                    .color(theme.muted_text),
            );
        }
    } else if let Some(duration) = activity.duration {
        // A finished mutation offers a one-click revert of exactly its op.
        if matches!(activity.status, ActivityStatus::Done)
            && let Some(operation_id) = activity.undo_op.clone()
        {
            head = head.push(
                button(
                    text("Undo")
                        .size(text_size::CAPTION)
                        .font(ui.config.ui_font)
                        .color(theme.accent),
                )
                .padding(Padding::from([1, 7]))
                .on_press(Message::UndoActivityOp(activity.id, operation_id))
                .style(move |_, status| ghost_button_style(theme, status)),
            );
        }
        head = head.push(
            text(format_duration(duration))
                .size(text_size::CAPTION)
                .font(ui.config.ui_font)
                .color(theme.subtle_text),
        );
    }
    if expandable {
        let chevron = if activity.expanded {
            icons::CHEVRON_UP
        } else {
            icons::CHEVRON_DOWN
        };
        head = head.push(icons::icon(chevron, 13.0, theme.subtle_text));
    }

    // Header (title line + result subtitle + the determinate bar). This is the
    // click target that toggles the detail; the detail itself sits *outside*
    // it, so dragging to select text there doesn't collapse the row.
    let mut header = column![head].spacing(3);
    if let Some(result) = activity.result.as_deref() {
        header = header.push(
            container(
                text(result)
                    .size(text_size::UI)
                    .font(ui.config.ui_font)
                    .color(theme.muted_text)
                    .wrapping(text::Wrapping::None)
                    .ellipsis(text::Ellipsis::End),
            )
            .width(Length::Fill)
            .padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: TITLE_INDENT,
            }),
        );
    }
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
        header = header.push(container(bar).width(Length::Fill).padding(Padding {
            top: 1.0,
            right: 0.0,
            bottom: 0.0,
            left: TITLE_INDENT,
        }));
    }

    let id = activity.id;
    let header_area = mouse_area(
        container(header)
            .width(Length::Fill)
            .padding(Padding::from([6, 8])),
    );
    // Only rows with captured output expand on click.
    let header_area: Element<'a, Message> = if expandable {
        header_area.on_press(Message::ActivityExpand(id)).into()
    } else {
        header_area.into()
    };

    if !activity.expanded || !expandable {
        return header_area;
    }

    // Expanded: the captured output in a code block under the header — one
    // read-only text_editor holding every line, so drag-selection sweeps
    // across lines and ⌘C copies the sweep (per-line widgets each trapped
    // the selection). Long lines wrap instead of panning individually. URLs
    // in the output stay plain selectable text; each one also gets a
    // one-click link under the buffer.
    let Some(editor) = activity.detail_editor.as_ref() else {
        return header_area;
    };
    let mut block_body = column![
        text_editor(editor)
            .size(text_size::CAPTION)
            .font(mono)
            .padding(0)
            .wrapping(text::Wrapping::WordOrGlyph)
            .on_action(move |action| Message::ActivityDetailAction(id, action))
            .style(move |_, _| text_editor::Style {
                background: Background::Color(Color::TRANSPARENT),
                border: Border {
                    width: 0.0,
                    color: Color::TRANSPARENT,
                    radius: 0.0.into(),
                },
                placeholder: theme.subtle_text,
                value: theme.muted_text,
                selection: Color {
                    a: 0.25,
                    ..theme.accent
                },
            }),
    ]
    .spacing(6);
    let urls = detail_urls(&activity.detail);
    if !urls.is_empty() {
        let mut links = column![].spacing(2);
        for url in urls {
            links = links.push(
                button(
                    text(url.clone())
                        .size(text_size::CAPTION)
                        .font(mono)
                        .color(theme.info),
                )
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
        }
        block_body = block_body.push(links);
    }
    let block = container(block_body)
        .width(Length::Fill)
        .padding(Padding::from([8, 10]))
        .clip(true)
        .style(move |_| code_block_style(theme));
    column![
        header_area,
        container(block).width(Length::Fill).padding(Padding {
            top: 2.0,
            right: 8.0,
            bottom: 8.0,
            left: 8.0 + TITLE_INDENT,
        }),
    ]
    .into()
}

/// Status glyph centered in the fixed [`STATUS_ICON_BOX`] square.
fn status_icon<'a>(
    activity: &Activity,
    theme: ThemeSpec,
    mono: iced::Font,
) -> Element<'a, Message> {
    match activity.status {
        ActivityStatus::Queued => mono_glyph("\u{2026}", mono, theme.subtle_text), // … waiting
        ActivityStatus::Running => mono_glyph(spinner_glyph(activity.started), mono, theme.accent),
        ActivityStatus::Done => icons::icon(icons::CHECK, STATUS_ICON_BOX, theme.added_text),
        ActivityStatus::Error => icons::icon(icons::CLOSE, STATUS_ICON_BOX, theme.removed_text),
    }
}

/// A mono-font glyph boxed like [`icons::icon`] so both glyph sources center
/// identically.
fn mono_glyph<'a>(glyph: &'a str, mono: iced::Font, color: Color) -> Element<'a, Message> {
    container(
        text(glyph)
            .font(mono)
            .size(text_size::UI)
            .color(color)
            .line_height(text::LineHeight::Relative(1.0)),
    )
    .center_x(Length::Fixed(STATUS_ICON_BOX))
    .center_y(Length::Fixed(STATUS_ICON_BOX))
    .into()
}

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

/// Unique whitespace-delimited `http(s)://…` tokens across the captured
/// output, in first-seen order — each becomes a one-click link under the
/// output editor.
fn detail_urls(detail: &[String]) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for line in detail {
        for token in line.split_whitespace() {
            if (token.starts_with("http://") || token.starts_with("https://"))
                && !urls.iter().any(|url| url == token)
            {
                urls.push(token.to_owned());
            }
        }
    }
    urls
}

fn clear_button(ui: &Diffui, theme: ThemeSpec) -> Element<'static, Message> {
    let enabled = ui
        .activities
        .activities
        .iter()
        .any(|a| matches!(a.status, ActivityStatus::Done | ActivityStatus::Error));
    let label = text("Clear")
        .size(text_size::UI)
        .font(ui.config.ui_font)
        .color(if enabled {
            theme.muted_text
        } else {
            theme.subtle_text
        });
    let mut b = button(label)
        .padding(Padding::from([2, 8]))
        .style(move |_, status| ghost_button_style(theme, status));
    if enabled {
        b = b.on_press(Message::ActivityClear);
    }
    b.into()
}

fn solid(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        ..container::Style::default()
    }
}

/// The expanded row's code block: a soft rounded wash behind the `$ command`
/// and output lines.
fn code_block_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(chip_background(theme.subtle_text))),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: crate::theme::radius::CONTROL.into(),
        },
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
    fn expand_builds_output_editor_and_collapse_drops_it() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "Push", false);
        log.append_output(ActivityId(1), "remote: hi");
        // Collapsed: no editor even though output exists.
        assert!(log.activities[0].detail_editor.is_none());

        log.toggle_expand(ActivityId(1));
        let editor = log.activities[0].detail_editor.as_ref().expect("built");
        assert_eq!(editor.text().trim_end(), "remote: hi");

        // Output landing while the row is open keeps the buffer in sync.
        log.append_output(ActivityId(1), "remote: bye");
        let editor = log.activities[0].detail_editor.as_ref().expect("kept");
        assert_eq!(editor.text().trim_end(), "remote: hi\nremote: bye");

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

    #[test]
    fn queued_entry_does_not_run_but_survives_clear() {
        let mut log = ActivityLog::default();
        log.start(ActivityId(1), "running", false);
        log.start(ActivityId(2), "queued", false);
        log.set_status(ActivityId(2), ActivityStatus::Queued);

        // Queued doesn't count as running and never drives the spinner.
        assert_eq!(log.running_count(), 1);
        assert_eq!(
            log.first_running().map(|a| a.label.as_str()),
            Some("running")
        );

        // Clearing keeps both the running and the queued entry.
        log.finish(ActivityId(1), ActivityStatus::Done, None);
        log.clear_finished();
        assert_eq!(log.activities.len(), 1);
        assert_eq!(log.activities[0].label, "queued");

        // When the queue drains it starts running for real.
        log.set_status(ActivityId(2), ActivityStatus::Running);
        assert_eq!(log.running_count(), 1);
        assert_eq!(
            log.first_running().map(|a| a.label.as_str()),
            Some("queued")
        );
    }
}

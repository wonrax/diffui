//! The GPUI side of the diff: its retained state, the element, and its events.
//!
//! A scroll tick or a hover updates [`State`] and notifies the [`DiffView`] entity,
//! never the app. What the app needs to hear about goes out through the props'
//! callbacks, which run after the entity's update has returned: a callback will
//! usually `act` on the root, and that must not happen with this entity leased.

use std::cell::RefCell;
use std::time::Instant;

use gpui_kit::{
    App, Bounds, Context, CursorStyle, DispatchPhase, Element, ElementId, Entity, GlobalElementId,
    Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollEvent, Style, TouchPhase, Window,
    relative,
};

use super::layout::{
    ActionHit, CheckHit, FileControl, HeightIndex, PREFIX_WIDTH, SelectionUnit, SplitSide,
    TextPosition, View, ordered,
};
use super::line::{GeometryMemo, body_geometry_key};
use super::paint::{CodeCache, Plan};
use super::{Callback, DiffProps, DiffView, HeaderAction, LineAt, LinePress};
use crate::geometry::{Point, Rectangle};
use crate::scroll_resample::{self, Resampler};
use crate::scrollbar;
use crate::theme::Color;

// A pause this long between wheel events ends a scroll gesture, unlocking the axis
// the gesture was pinned to. Touchpads stream events every few ms while a finger
// moves (inertia included), so a real gap means a new intent.
const SCROLL_GESTURE_GAP_MS: u64 = 200;
// Click within this distance of the previous click counts as a multi-click rather
// than a fresh selection anchor.
const MULTI_CLICK_RADIUS: f32 = 4.0;
// Auto-scroll speed (px/sec) when the mouse is dragged just outside the viewport.
// Scaled by how far past the edge the cursor sits.
const AUTO_SCROLL_MAX_SPEED: f32 = 1200.0;
// Width of the "edge zone" outside the viewport over which the auto-scroll speed
// ramps up. The cursor sitting just past the edge scrolls slowly; a cursor far
// outside the viewport scrolls at full speed.
const AUTO_SCROLL_RAMP_PX: f32 = 80.0;

/// Which axis a scroll gesture is locked to (see `State::scroll_axis`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollAxis {
    Horizontal,
    Vertical,
}

/// Everything that outlives a frame.
pub(super) struct State {
    selected_file: usize,
    revision_key: Option<String>,
    pending_file_jump: Option<usize>,
    /// (file, hunk, line, byte_in_line) of a match to scroll into view.
    pending_find_scroll: Option<(usize, usize, usize, usize)>,
    /// Last find scroll token acted on.
    last_find_scroll_token: Option<u64>,
    pub(super) vertical_offset: f32,
    /// `vertical_offset` on a whole device pixel: what a frame paints at, so a
    /// trackpad's fractional offsets don't blur the text.
    pub(super) painted_offset: f32,
    /// Horizontal scroll of the code text in no-wrap mode (px). Forced to zero while
    /// wrapping. The gutter/prefix chrome stays fixed; only text and its highlight
    /// rects shift. Side-by-side scrolls both columns in lockstep.
    pub(super) horizontal_offset: f32,
    /// Wrap flag last seen, to zero `horizontal_offset` when wrap turns on.
    last_wrap: bool,
    /// Axis a two-dimensional scroll gesture locked onto, IDE-style: the first wheel
    /// event of a gesture picks the dominant axis and the other component is discarded
    /// until the gesture ends. Keeps touchpad panning from drifting diagonally in
    /// no-wrap mode. `None` between gestures.
    scroll_axis: Option<ScrollAxis>,
    /// When the last wheel event arrived: a gap ends the gesture.
    last_scroll_at: Option<Instant>,
    /// Shaped code, reused across frames while scrolling.
    pub(super) code: RefCell<CodeCache>,
    /// Anchor (mouse-down position) of the in-progress or completed selection.
    pub(super) selection_anchor: Option<TextPosition>,
    /// Focus (current/release position) of the selection.
    pub(super) selection_focus: Option<TextPosition>,
    /// Side-by-side only: the column the selection lives in, locked at mouse-down.
    /// Drags stay in this column, the highlight draws only there, and copy skips the
    /// other column's lines. `None` for unified mode, header, and full-width-file
    /// selections.
    pub(super) selection_lane: Option<SplitSide>,
    /// The selection's press landed on a header band, not on text.
    selection_from_band: bool,
    /// When the user drags after a double/triple click, the selection grows in word-
    /// or line-sized chunks instead of by character. We remember where the anchor
    /// "click unit" started/ended so the selection always covers full units in both
    /// directions.
    selection_anchor_unit_start: Option<TextPosition>,
    selection_anchor_unit_end: Option<TextPosition>,
    selection_unit: SelectionUnit,
    /// True while the user is mid-drag.
    is_selecting: bool,
    /// Multi-click bookkeeping: timestamp + screen position + count of the last
    /// left-button press.
    last_click_time: Option<Instant>,
    last_click_screen: Option<Point>,
    click_count: u32,
    /// Last cursor position observed while dragging: drives auto-scroll and reprojects
    /// the selection focus after the viewport shifts under the cursor.
    last_drag_cursor: Option<Point>,
    /// When auto-scroll last stepped, so its speed doesn't depend on the frame rate.
    last_auto_scroll: Option<Instant>,
    /// What the bar over the view reads, and where a drag on it asks to go.
    pub(super) bar: scrollbar::Shared,
    /// Vertical wheel motion still to be drawn, spread evenly over frames.
    wheel: Resampler,
    /// Most recent `restore_token` acted on; a change schedules a one-shot jump to the
    /// caller's `restore_offset`. Starts at 0 to match the caller's initial token.
    last_restore_token: u64,
    /// Offset to jump to next, set when `restore_token` changed. Overrides the
    /// `revision_key`-change reset and file jump.
    pending_set_offset: Option<f32>,
    /// Most recent `content_version` acted on. A change drops the per-line caches
    /// (whose keys would otherwise map to stale text).
    last_content_version: u64,
    /// Last revision-header height. Opening/closing the embedded editor changes header
    /// row geometry without changing the revision key; selections use header row
    /// indices, so they must be cleared when this shape changes.
    last_header_height_bits: u32,
    /// Palette identity last seen. Cached text bakes the syntax colours in, so a theme
    /// switch must drop it. `text` alone distinguishes every built-in theme.
    last_palette_text: Option<Color>,
    /// The file header under the cursor, and the control in it the cursor is on.
    pub(super) hovered_file_header: Option<(usize, Option<FileControl>)>,
    /// The hunk under the cursor: its hover-only strip buttons show.
    pub(super) hovered_hunk: Option<(usize, usize)>,
    /// The file whose note button is under the cursor.
    pub(super) hovered_note_button: Option<usize>,
    /// The blame run under the cursor: `start..end` lines.
    pub(super) hovered_annotation: Option<(usize, usize)>,
    /// Whether the cursor is over the description block. Reveals the edit affordance at
    /// its right edge.
    pub(super) hovered_description: bool,
    /// The rich-field link or button under the cursor: (header line, slot), slot 0
    /// being the link and 1.. the buttons.
    pub(super) hovered_header_action: Option<(usize, usize)>,
    /// Line-select mode: the pickable line under the mouse.
    pub(super) hovered_line: Option<LineAt>,
    /// Line-select mode: a press on a line is being dragged: the line it last reached,
    /// and the side-by-side column it is held to.
    line_drag: Option<(LineAt, Option<SplitSide>)>,
    /// Last `LineMode::reveal` acted on.
    last_line_reveal: Option<u64>,
    /// A line to scroll into view next.
    pending_line_reveal: Option<LineAt>,
    /// The hunk- or file-strip button under the cursor.
    pub(super) hovered_hunk_action: Option<ActionHit>,
    /// Last `HunkActions::reveal` token acted on.
    last_hunk_reveal: Option<u64>,
    /// A hunk header to scroll into view next.
    pending_hunk_reveal: Option<(usize, usize)>,
    /// Prefix-sum layout index (see [`HeightIndex`]).
    pub(super) height_index: RefCell<HeightIndex>,
    /// Memoized per-line geometry.
    pub(super) geometry: RefCell<GeometryMemo>,
    /// How many frames this view has planned, and rows on the last one. For the
    /// benchmark's one-paint-per-frame check.
    pub(super) frames: u64,
    /// How long the last height-index rebuild took, for the same.
    pub(super) last_index_build: std::time::Duration,
}

impl Default for State {
    fn default() -> Self {
        Self {
            selected_file: 0,
            revision_key: None,
            pending_file_jump: None,
            pending_find_scroll: None,
            last_find_scroll_token: None,
            vertical_offset: 0.0,
            painted_offset: 0.0,
            horizontal_offset: 0.0,
            last_wrap: true,
            scroll_axis: None,
            last_scroll_at: None,
            code: RefCell::default(),
            selection_anchor: None,
            selection_focus: None,
            selection_lane: None,
            selection_from_band: false,
            selection_anchor_unit_start: None,
            selection_anchor_unit_end: None,
            selection_unit: SelectionUnit::Character,
            is_selecting: false,
            last_click_time: None,
            last_click_screen: None,
            click_count: 0,
            last_drag_cursor: None,
            last_auto_scroll: None,
            bar: scrollbar::Shared::default(),
            wheel: Resampler::default(),
            last_restore_token: 0,
            pending_set_offset: None,
            last_content_version: 0,
            last_header_height_bits: 0,
            last_palette_text: None,
            hovered_file_header: None,
            hovered_hunk: None,
            hovered_note_button: None,
            hovered_annotation: None,
            hovered_description: false,
            hovered_header_action: None,
            hovered_line: None,
            line_drag: None,
            last_line_reveal: None,
            pending_line_reveal: None,
            hovered_hunk_action: None,
            last_hunk_reveal: None,
            pending_hunk_reveal: None,
            height_index: RefCell::default(),
            geometry: RefCell::default(),
            frames: 0,
            last_index_build: std::time::Duration::ZERO,
        }
    }
}

/// Calls to make once the entity's update has returned.
type Outbox = Vec<Box<dyn FnOnce(&mut Window, &mut App)>>;

fn send<T: 'static>(out: &mut Outbox, callback: &Option<Callback<T>>, value: T) {
    if let Some(callback) = callback {
        let callback = callback.clone();
        out.push(Box::new(move |window, cx| callback(&value, window, cx)));
    }
}

fn run(out: Outbox, window: &mut Window, cx: &mut App) {
    for call in out {
        call(window, cx);
    }
}

impl State {
    /// Whether the cursor is on `file`'s header, and on which control.
    pub(super) fn header_hover(&self, file: usize) -> Option<Option<FileControl>> {
        self.hovered_file_header
            .filter(|(hovered, _)| *hovered == file)
            .map(|(_, control)| control)
    }

    fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.selection_focus = None;
        self.selection_lane = None;
        self.selection_anchor_unit_start = None;
        self.selection_anchor_unit_end = None;
        self.selection_unit = SelectionUnit::Character;
    }

    /// New props against what the state last saw. Runs at
    /// prepaint rather than when the props are set, because the document can be read
    /// only then.
    fn sync(&mut self, view: &View<'_>) {
        let props = view.props;
        let header_height_bits = view.header_height().to_bits();
        if self.last_header_height_bits != header_height_bits {
            self.last_header_height_bits = header_height_bits;
            self.selection_anchor = None;
            self.selection_focus = None;
            self.selection_lane = None;
            self.is_selecting = false;
        }

        // Checked before the revision-key reset below (which early-returns), so a tab
        // restore is never skipped. Applied last, so it overrides the reset-to-top + file
        // jump that reset schedules.
        if props.restore_token != self.last_restore_token {
            self.last_restore_token = props.restore_token;
            self.pending_set_offset = Some(props.restore_offset);
        }

        // The content changed under us (a reload, a working-copy edit, or a tab switch).
        // Drop the per-line caches, whose `(file, hunk, line)` keys now map to different
        // text. Done before the revision-key reset so `last_content_version` always
        // advances. Cache-only: a working-copy edit must refresh the rendered text
        // without resetting scroll or selection.
        if props.content_version != self.last_content_version {
            self.last_content_version = props.content_version;
            self.code.borrow_mut().clear();
            self.geometry.borrow_mut().clear();
        }

        // Theme switch: cached text carries the old theme's colours baked in.
        if self.last_palette_text != Some(props.palette.text) {
            self.last_palette_text = Some(props.palette.text);
            self.code.borrow_mut().clear();
        }

        // Wrap-mode flips zero the sideways scroll: wrapped content never overflows, and
        // a stale offset would blank the whole pane.
        if props.wrap != self.last_wrap {
            self.last_wrap = props.wrap;
            self.horizontal_offset = 0.0;
        }

        if self.revision_key.as_deref() != Some(props.revision_key.as_str()) {
            let first_time = self.revision_key.is_none();
            self.revision_key = Some(props.revision_key.clone());
            self.vertical_offset = 0.0;
            self.horizontal_offset = 0.0;
            self.selected_file = props.selected_file;
            self.pending_file_jump = Some(props.selected_file);
            // A revision change means the underlying line indices no longer refer to the
            // same text: drop the selection rather than risk copying stale content.
            self.clear_selection();
            self.is_selecting = false;
            self.last_drag_cursor = None;
            self.click_count = 0;
            // Cache entries are keyed by (file, hunk, line) which now points at different
            // content.
            self.code.borrow_mut().clear();
            self.geometry.borrow_mut().clear();
            if !first_time {
                return;
            }
        }

        if self.selected_file != props.selected_file {
            self.pending_file_jump = Some(props.selected_file);
        }

        match view.line_mode() {
            Some(mode) => {
                if self.last_line_reveal != Some(mode.reveal) {
                    self.last_line_reveal = Some(mode.reveal);
                    self.pending_line_reveal = mode.cursor;
                }
            }
            None => {
                self.hovered_line = None;
                self.line_drag = None;
            }
        }

        match props.hunk_actions.as_ref().and_then(|it| it.reveal) {
            Some((file, hunk, token)) => {
                if self.last_hunk_reveal != Some(token) {
                    self.last_hunk_reveal = Some(token);
                    self.pending_hunk_reveal = Some((file, hunk));
                }
            }
            None => self.hovered_hunk_action = None,
        }

        if let Some(find) = &props.find
            && self.last_find_scroll_token != Some(find.scroll_token)
        {
            self.last_find_scroll_token = Some(find.scroll_token);
            if let Some(m) = find.active.and_then(|active| view.matches.get(active)) {
                self.pending_find_scroll =
                    Some((m.file_index, m.hunk_index, m.line_index, m.byte_start));
            }
        }
    }

    fn publish_file(&mut self, view: &View<'_>, out: &mut Outbox) {
        let selected = view.file_at_offset(&self.height_index.borrow(), self.vertical_offset);
        if selected != self.selected_file {
            self.selected_file = selected;
            send(out, &view.props.on_selected_file_changed, selected);
        }
    }

    /// What's been scheduled, now that bounds are known. Returns whether anything moved.
    fn apply_pending(&mut self, view: &View<'_>, bounds: Rectangle, out: &mut Outbox) {
        let (max_vertical, max_horizontal) = self.limits(view, bounds);
        self.vertical_offset = self.vertical_offset.min(max_vertical);
        self.horizontal_offset = self.horizontal_offset.min(max_horizontal);
        let metrics = view.metrics;

        // `sync` schedules the jump when the caller's selected file moves.
        if let Some(file_index) = self.pending_file_jump.take() {
            // For file 0, scroll to the very top so the revision header stays visible:
            // `file_offset(0)` equals `header_height()`, which would park the file's
            // content header right at the top and hide the revision metadata above it.
            let target = if file_index == 0 {
                0.0
            } else {
                view.file_offset(&self.height_index.borrow(), file_index)
            };
            self.vertical_offset = target.clamp(0.0, max_vertical);
            self.selected_file = file_index;
        }

        let find = self.pending_find_scroll.take().and_then(|at| {
            let index = self.height_index.borrow();
            Some((
                at,
                view.match_target_y(&index, &self.geometry, bounds.width, at)?,
            ))
        });
        if let Some(((file_idx, hunk_idx, line_idx, byte_offset), target)) = find {
            // Center the row in the viewport when there's room; clamp otherwise. Centering
            // keeps the match's surrounding context visible instead of pinning it to the
            // top edge.
            let centered = target - (bounds.height - metrics.row_height) / 2.0;
            self.vertical_offset = centered.clamp(0.0, max_vertical);
            // In no-wrap mode the match may sit past the right edge: scroll sideways so it
            // lands ~1/3 into the text area (some leading context, most of the room for
            // what follows).
            if max_horizontal > 0.0
                && let Some(line) = view.line(file_idx, hunk_idx, line_idx)
            {
                let key = body_geometry_key(
                    file_idx,
                    hunk_idx,
                    line_idx,
                    f32::INFINITY,
                    metrics.char_width,
                );
                let match_x = view
                    .geometry(&self.geometry, key, &line.content, f32::INFINITY)
                    .column_at(0, byte_offset)
                    * metrics.char_width;
                let text_width = view.effective_text_width(bounds.width);
                let off_screen = match_x < self.horizontal_offset
                    || match_x > self.horizontal_offset + text_width - metrics.char_width;
                if off_screen {
                    self.horizontal_offset =
                        (match_x - text_width / 3.0).clamp(0.0, max_horizontal);
                }
            }
            self.publish_file(view, out);
        }

        if let Some((file, hunk)) = self.pending_hunk_reveal.take() {
            let top = {
                let index = self.height_index.borrow();
                index
                    .hunk_ids
                    .binary_search(&(file as u32, hunk as u32))
                    .ok()
                    .map(|slot| index.hunk_tops[slot])
            };
            if let Some(top) = top {
                // Left alone when the strip and a few of its rows show below the sticky
                // file header; else parked right under it.
                let above = top - metrics.file_header_height;
                let below =
                    top + metrics.hunk_header_height + 4.0 * metrics.row_height - bounds.height;
                let target = if above < self.vertical_offset || below > self.vertical_offset {
                    above
                } else {
                    self.vertical_offset
                };
                let target = target.clamp(0.0, max_vertical);
                if (target - self.vertical_offset).abs() > f32::EPSILON {
                    self.vertical_offset = target;
                    self.publish_file(view, out);
                }
            }
        }

        if let Some(at) = self.pending_line_reveal.take() {
            let span = {
                let index = self.height_index.borrow();
                index
                    .row_of_line(at.file, at.hunk, at.line)
                    .map(|row| (index.row_tops[row], index.row_heights[row]))
            };
            if let Some((top, height)) = span {
                // Clear of the sticky file header above, and a row of context either side.
                let above = top - metrics.file_header_height - metrics.row_height;
                let below = top + height + metrics.row_height - bounds.height;
                let target = if above < self.vertical_offset {
                    above
                } else if below > self.vertical_offset {
                    below
                } else {
                    self.vertical_offset
                };
                let target = target.clamp(0.0, max_vertical);
                if (target - self.vertical_offset).abs() > f32::EPSILON {
                    self.vertical_offset = target;
                    self.publish_file(view, out);
                }
            }
        }

        // Tab restore: jump straight to the saved offset, overriding the reset-to-top a
        // revision change scheduled above. Only the offset is restored: `selected_file` was
        // already set by the file-jump block above (to the tab's saved file), and
        // republishing it here would fire `SelectFile` and fight the sidebar's own restore.
        if let Some(offset) = self.pending_set_offset.take() {
            self.vertical_offset = offset.clamp(0.0, max_vertical);
        }
    }

    fn limits(&self, view: &View<'_>, bounds: Rectangle) -> (f32, f32) {
        let index = self.height_index.borrow();
        (
            (index.total_height - bounds.height).max(0.0),
            view.max_horizontal(&index, bounds.width),
        )
    }

    /// Steps the drag's auto-scroll. Returns whether it wants another frame.
    fn step_auto_scroll(&mut self, view: &View<'_>, bounds: Rectangle, out: &mut Outbox) -> bool {
        let (true, Some(cursor)) = (self.is_selecting, self.last_drag_cursor) else {
            self.last_auto_scroll = None;
            return false;
        };
        let speed = auto_scroll_speed(cursor.y, bounds);
        if speed == 0.0 {
            self.last_auto_scroll = None;
            return false;
        }
        let now = Instant::now();
        let elapsed = self
            .last_auto_scroll
            .map_or(1.0 / 60.0, |at| (now - at).as_secs_f32().min(0.1));
        self.last_auto_scroll = Some(now);
        let (max_vertical, _) = self.limits(view, bounds);
        let offset = (self.vertical_offset + speed * elapsed).clamp(0.0, max_vertical);
        if (offset - self.vertical_offset).abs() > f32::EPSILON {
            self.vertical_offset = offset;
            self.advance_drag_selection(view, cursor, bounds);
            self.publish_file(view, out);
        }
        true
    }

    /// Everything hover-driven at `point`. Returns whether any of it changed.
    fn refresh_hover(&mut self, view: &View<'_>, bounds: Rectangle, point: Option<Point>) -> bool {
        let offset = self.vertical_offset;
        let (hunk, note_button) = {
            let index = self.height_index.borrow();
            (
                point.and_then(|p| view.hunk_at(&index, offset, bounds, p)),
                point.and_then(|p| view.note_button_at(&index, offset, bounds, p)),
            )
        };
        let hunk_changed = hunk != self.hovered_hunk || note_button != self.hovered_note_button;
        self.hovered_hunk = hunk;
        self.hovered_note_button = note_button;
        let (file_header, annotation, action, description, header, line) = {
            let index = self.height_index.borrow();
            let index = &*index;
            let at = |f: &dyn Fn(Point) -> bool| point.is_some_and(f);
            (
                point.and_then(|p| view.file_header_at(index, offset, bounds, p)),
                point.and_then(|p| view.annotation_run_at(index, bounds, offset, p)),
                point.and_then(|p| view.strip_action_at(index, bounds, offset, p)),
                at(&|p| view.description_row_contains(bounds, offset, p)),
                point
                    .and_then(|p| view.header_action_at(bounds, offset, p))
                    .map(|(line, slot, _)| (line, slot)),
                point
                    .and_then(|p| view.pickable_line_at(index, offset, bounds, p, None))
                    .map(|(line, _)| line),
            )
        };
        let changed = hunk_changed
            || file_header != self.hovered_file_header
            || annotation != self.hovered_annotation
            || action != self.hovered_hunk_action
            || description != self.hovered_description
            || header != self.hovered_header_action
            || line != self.hovered_line;
        self.hovered_file_header = file_header;
        self.hovered_annotation = annotation;
        self.hovered_hunk_action = action;
        self.hovered_description = description;
        self.hovered_header_action = header;
        self.hovered_line = line;
        changed
    }

    /// Update the selection focus based on the cursor's current screen position. Handles
    /// word/line-mode expansion: in those modes the selection always covers full units in
    /// both directions, so dragging past the next word boundary jumps an entire word at
    /// once.
    fn advance_drag_selection(&mut self, view: &View<'_>, cursor: Point, bounds: Rectangle) {
        let focus = {
            let index = self.height_index.borrow();
            // A press on a band anchors at the document's end, and a focus snapped
            // to the nearest row would select everything from there to it.
            let locate = if self.selection_from_band {
                View::position_at_point
            } else {
                View::drag_position_at_point
            };
            locate(
                view,
                &index,
                &self.geometry,
                cursor,
                bounds,
                self.vertical_offset,
                self.horizontal_offset,
                // The drag stays in the column the selection started in.
                self.selection_lane,
            )
        };
        let Some((focus, _)) = focus else {
            return;
        };
        let (Some(anchor_start), Some(anchor_end)) = (
            self.selection_anchor_unit_start,
            self.selection_anchor_unit_end,
        ) else {
            return;
        };
        let (focus_start, focus_end) = if self.selection_unit == SelectionUnit::Character {
            (focus, focus)
        } else {
            view.expand_to_unit(focus, self.selection_unit)
        };
        let (anchor, focus) = if focus_end <= anchor_start {
            // Drag has crossed to the left of the original click: selection grows from the
            // *end* of the anchor unit backwards to the *start* of the focus unit.
            (anchor_end, focus_start)
        } else if focus_start >= anchor_end {
            (anchor_start, focus_end)
        } else {
            // Still inside the original anchor unit.
            (anchor_start, anchor_end)
        };
        self.selection_anchor = Some(anchor);
        self.selection_focus = Some(focus);
    }

    /// A wheel event at `point`. Returns whether anything moved.
    fn on_wheel(
        &mut self,
        view: &View<'_>,
        bounds: Rectangle,
        event: &ScrollEvent,
        out: &mut Outbox,
    ) -> bool {
        let (max_vertical, max_horizontal) = self.limits(view, bounds);
        let prev_offset = self.vertical_offset;
        let delta = crate::wheel::points(event.delta);
        crate::frame_log::value("wy", f64::from(delta.y));
        // Positive means the content moves down in GPUI; an offset grows going down.
        let mut movement = Point::new(-f32::from(delta.x), -f32::from(delta.y));
        // Shift+wheel pans sideways, matching editors. Only redirect when the event has no
        // native x component: macOS folds shift+wheel into the x delta itself, and trackpads
        // and tilt wheels emit real x deltas.
        if event.modifiers.shift && movement.x == 0.0 {
            movement = Point::new(movement.y, 0.0);
        }

        // IDE-style axis lock while horizontal scrolling exists (no wrap, overflowing
        // lines): the gesture's first event picks the dominant axis and the other component
        // is discarded, so a touchpad pan doesn't drift diagonally. A pause in events ends
        // the gesture and the next one re-picks.
        if max_horizontal > 0.0 {
            let now = Instant::now();
            let gesture_ended = self.last_scroll_at.is_none_or(|at| {
                now.saturating_duration_since(at).as_millis() as u64 >= SCROLL_GESTURE_GAP_MS
            });
            self.last_scroll_at = Some(now);
            if gesture_ended || event.touch_phase == TouchPhase::Started {
                self.scroll_axis = None;
            }
            // A trackpad opens its gesture with a still event: it says nothing about the
            // axis, so the first one that moves picks it.
            if movement.x != 0.0 || movement.y != 0.0 {
                let axis =
                    *self
                        .scroll_axis
                        .get_or_insert(if movement.x.abs() > movement.y.abs() {
                            ScrollAxis::Horizontal
                        } else {
                            ScrollAxis::Vertical
                        });
                match axis {
                    ScrollAxis::Horizontal => movement.y = 0.0,
                    ScrollAxis::Vertical => movement.x = 0.0,
                }
            }
        } else {
            self.scroll_axis = None;
            self.last_scroll_at = None;
        }

        let prev_horizontal = self.horizontal_offset;
        let mut owed = false;
        if movement.y != 0.0 {
            if scroll_resample::delay().is_zero() {
                self.vertical_offset = (self.vertical_offset + movement.y).clamp(0.0, max_vertical);
                self.publish_file(view, out);
            } else {
                // Drawn over the next frames: see `scroll_resample`.
                self.wheel.push(Instant::now(), f64::from(movement.y));
                owed = true;
            }
        }
        // Sideways only lives in no-wrap mode, where content can actually overflow.
        if movement.x != 0.0 && max_horizontal > 0.0 {
            self.horizontal_offset =
                (self.horizontal_offset + movement.x).clamp(0.0, max_horizontal);
        }
        // Headers slid under the (stationary) cursor: refresh the hover so a wash doesn't
        // linger on a moved row.
        let point: Point = event.position.into();
        self.refresh_hover(view, bounds, bounds.contains(point).then_some(point));
        if self.vertical_offset != prev_offset {
            send(out, &view.props.on_scroll, self.vertical_offset);
        }
        owed || self.vertical_offset != prev_offset || self.horizontal_offset != prev_horizontal
    }

    /// A left press at `point`, inside the view.
    fn on_press(
        &mut self,
        view: &View<'_>,
        bounds: Rectangle,
        point: Point,
        modifiers: Modifiers,
        out: &mut Outbox,
    ) {
        let props = view.props;
        let offset = self.vertical_offset;
        // A header field's link or button wins over selection too.
        if let Some((_, _, action)) = view.header_action_at(bounds, offset, point) {
            send::<HeaderAction>(out, &props.on_header_action, action);
            return;
        }
        if view
            .description_edit_bounds(bounds, offset)
            .is_some_and(|button| button.contains(point))
        {
            send(out, &props.on_edit_description, ());
            return;
        }
        let (hit, check, file_control, annotation, expand, pickable) = {
            let index = self.height_index.borrow();
            let index = &*index;
            let expand = view
                .note_button_at(index, offset, bounds, point)
                .or_else(|| view.expand_header_at(index, offset, bounds, point));
            (
                view.strip_action_at(index, bounds, offset, point),
                view.check_at(index, offset, bounds, point),
                view.file_header_at(index, offset, bounds, point)
                    .and_then(|(file, control)| Some((file, control?))),
                view.annotation_line_at(index, bounds, offset, point),
                expand,
                view.pickable_line_at(index, offset, bounds, point, None),
            )
        };
        if let Some(hit) = hit
            && let Some(actions) = &props.hunk_actions
        {
            let (callback, value) = match hit {
                ActionHit::Hunk(row, button) => (actions.on_press.clone(), (row, button)),
                ActionHit::File(strip, button) => (actions.on_file_press.clone(), (strip, button)),
            };
            out.push(Box::new(move |window, cx| callback(&value, window, cx)));
            return;
        }
        if let Some(hit) = check
            && let Some(marks) = &props.marks
        {
            let CheckHit::File(file) = hit;
            send(out, &Some(marks.on_toggle_file.clone()), file);
            return;
        }
        if let Some((file_index, control)) = file_control {
            match control {
                FileControl::Fold => send(out, &props.on_toggle_fold, file_index),
                FileControl::Browse => send(out, &props.on_browse_file, file_index),
                FileControl::More => {
                    let rect = {
                        let index = self.height_index.borrow();
                        view.visible_file_header_positions(&index, bounds, offset)
                            .into_iter()
                            .find(|&(file, _)| file == file_index)
                            .and_then(|(_, y)| view.file_header_layout(file_index, y, bounds))
                            .and_then(|layout| layout.more)
                    };
                    if let Some(rect) = rect {
                        send(out, &props.on_file_menu, (file_index, rect));
                    }
                }
                // Handled with the hunk boxes above.
                FileControl::Check => {}
            }
            return;
        }
        if let (Some(gutter), Some(line)) = (&props.annotations, annotation) {
            send(out, &Some(gutter.on_click.clone()), line);
            return;
        }
        if let Some(file_index) = expand {
            send(out, &props.on_expand_file, file_index);
            return;
        }
        if let (Some(mode), Some((line, lane))) = (view.line_mode(), pickable) {
            send(
                out,
                &Some(mode.on_press.clone()),
                LinePress {
                    line,
                    extend: modifiers.shift,
                    one_side: lane.is_some(),
                },
            );
            self.line_drag = Some((line, lane));
            self.selection_anchor = None;
            self.selection_focus = None;
            self.selection_lane = None;
            return;
        }
        let (position, from_band) = {
            let index = self.height_index.borrow();
            (
                view.position_at_point(
                    &index,
                    &self.geometry,
                    point,
                    bounds,
                    offset,
                    self.horizontal_offset,
                    None,
                ),
                view.band_snap(&index, point.y - bounds.y + offset)
                    .is_some(),
            )
        };
        let Some((position, lane)) = position else {
            return;
        };

        let now = Instant::now();
        let within_window = self
            .last_click_time
            .is_some_and(|t| now.duration_since(t).as_millis() as u64 <= props.multi_click_ms);
        let same_spot = self
            .last_click_screen
            .is_some_and(|p| p.distance(point) <= MULTI_CLICK_RADIUS);
        let next_count = if within_window && same_spot {
            self.click_count + 1
        } else {
            1
        };
        self.click_count = next_count;
        self.last_click_time = Some(now);
        self.last_click_screen = Some(point);

        // Cycle through Char, Word, Line on each successive click.
        let unit = match (next_count - 1) % 3 {
            0 => SelectionUnit::Character,
            1 => SelectionUnit::Word,
            _ => SelectionUnit::Line,
        };
        self.selection_unit = unit;
        let (anchor_start, anchor_end) = view.expand_to_unit(position, unit);
        self.selection_anchor_unit_start = Some(anchor_start);
        self.selection_anchor_unit_end = Some(anchor_end);
        self.selection_anchor = Some(anchor_start);
        self.selection_focus = Some(anchor_end);
        self.selection_lane = lane;
        self.selection_from_band = from_band;
        self.is_selecting = true;
        self.last_drag_cursor = Some(point);
    }

    /// The pointer moved. Returns whether a repaint is needed.
    fn on_move(
        &mut self,
        view: &View<'_>,
        bounds: Rectangle,
        position: Point,
        left_held: bool,
        out: &mut Outbox,
    ) -> bool {
        if let Some((last, lane)) = self.line_drag {
            let reached = {
                let index = self.height_index.borrow();
                view.pickable_line_at(&index, self.vertical_offset, bounds, position, Some(lane))
            };
            if let Some(mode) = view.line_mode()
                && let Some((line, _)) = reached
                && line != last
            {
                send(out, &Some(mode.on_paint.clone()), line);
                self.line_drag = Some((line, lane));
                self.hovered_line = Some(line);
                return true;
            }
            return false;
        }
        if self.is_selecting {
            // A release the view never saw (outside the window, say) ends the drag here.
            if !left_held {
                self.finish_selection();
                return true;
            }
            self.last_drag_cursor = Some(position);
            self.advance_drag_selection(view, position, bounds);
            return true;
        }
        self.refresh_hover(view, bounds, bounds.contains(position).then_some(position))
    }

    fn finish_selection(&mut self) {
        self.is_selecting = false;
        self.last_drag_cursor = None;
        // If the user clicked without dragging in character mode, anchor == focus: clear so
        // we don't leave a phantom zero-width selection. Multi-click selections are kept even
        // without drag, since the user expects them to stick (double-click a word, then ⌘C).
        if self.selection_unit == SelectionUnit::Character
            && self.selection_anchor == self.selection_focus
        {
            self.selection_anchor = None;
            self.selection_focus = None;
            self.selection_lane = None;
        }
    }

    /// The left button came up. Returns whether a repaint is needed.
    fn on_release(&mut self) -> bool {
        if self.line_drag.take().is_some() {
            return false;
        }
        if !self.is_selecting {
            return false;
        }
        self.finish_selection();
        true
    }

    /// The selected text, if any.
    fn selected_text(&self, view: &View<'_>) -> Option<String> {
        let (start, end) = ordered(self.selection_anchor?, self.selection_focus?);
        if start == end {
            return None;
        }
        Some(view.collect_selected_text(start, end, self.selection_lane))
            .filter(|text| !text.is_empty())
    }

    /// Match the convention of every code editor: text cursor over the text area, default
    /// arrow over the gutter and chrome.
    fn cursor_at(&self, view: &View<'_>, bounds: Rectangle, point: Point) -> CursorStyle {
        let index = self.height_index.borrow();
        let index = &*index;
        let offset = self.vertical_offset;
        if scrollbar::hits_container(bounds, point, index.total_height) {
            return CursorStyle::Arrow;
        }
        if view
            .file_header_at(index, offset, bounds, point)
            .is_some_and(|(_, control)| control.is_some())
            || view.check_at(index, offset, bounds, point).is_some()
            || view
                .expand_header_at(index, offset, bounds, point)
                .is_some()
            || view.note_button_at(index, offset, bounds, point).is_some()
            || view
                .pickable_line_at(index, offset, bounds, point, None)
                .is_some()
            || view
                .annotation_run_at(index, bounds, offset, point)
                .is_some()
            || view
                .description_edit_bounds(bounds, offset)
                .is_some_and(|button| button.contains(point))
            || view.header_action_at(bounds, offset, point).is_some()
            || view.strip_action_at(index, bounds, offset, point).is_some()
        {
            return CursorStyle::PointingHand;
        }
        // In the revision-header strip, show the text cursor only over the selectable values
        // (field values + description), and the arrow over labels, bookmark chips, and blank
        // space.
        let target_y = point.y - bounds.y + offset;
        if target_y < view.header_height() {
            let over_value = view.header_line_at_y(target_y).is_some_and(|line_index| {
                view.header_selectable_text(line_index).is_some()
                    && point.x >= view.header_text_origin_x(line_index, bounds)
            });
            return if over_value {
                CursorStyle::IBeam
            } else {
                CursorStyle::Arrow
            };
        }
        if view.props.side_by_side {
            // Full-width (single-sided) files hit-test like unified rows.
            if !index.is_full_width(view.file_at_offset(index, target_y)) {
                let split = view.split_layout(bounds.width);
                let x = point.x - bounds.x;
                let over_text = (x >= split.gutter_width + PREFIX_WIDTH && x < split.divider_x)
                    || x >= split.right_gutter_x + split.gutter_width + PREFIX_WIDTH;
                return if over_text {
                    CursorStyle::IBeam
                } else {
                    CursorStyle::Arrow
                };
            }
        }
        if point.x >= bounds.x + view.metrics.gutter_width + view.metrics.prefix_width {
            CursorStyle::IBeam
        } else {
            CursorStyle::Arrow
        }
    }
}

/// How fast (px/sec, signed) a drag past the top or bottom edge scrolls: zero while the
/// cursor is inside the viewport.
fn auto_scroll_speed(cursor_y: f32, bounds: Rectangle) -> f32 {
    let bottom = bounds.y + bounds.height;
    let speed_for =
        |distance: f32| (distance / AUTO_SCROLL_RAMP_PX).clamp(0.0, 1.0) * AUTO_SCROLL_MAX_SPEED;
    if cursor_y < bounds.y {
        -speed_for(bounds.y - cursor_y)
    } else if cursor_y > bottom {
        speed_for(cursor_y - bottom)
    } else {
        0.0
    }
}

/// The diff's element: the view paints here, and every event lands here.
pub(super) struct DiffElement {
    view: Entity<DiffView>,
}

impl DiffElement {
    pub(super) fn new(view: Entity<DiffView>) -> Self {
        Self { view }
    }
}

impl IntoElement for DiffElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// What prepaint hands paint.
pub(super) struct Frame {
    hitbox: Hitbox,
    plan: Plan,
    cursor: CursorStyle,
    /// The drag's auto-scroll wants another frame.
    animating: bool,
}

impl DiffView {
    /// Builds one frame with the document borrowed from the app.
    fn prepare(
        &mut self,
        bounds: Rectangle,
        hitbox: Hitbox,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Frame {
        window.set_focus_handle(&self.focus, cx);
        let mut plan = Plan::new(window.text_system().clone(), 512);
        let mut out = Outbox::new();
        let scale = window.scale_factor();
        let mouse: Point = window.mouse_position().into();
        let mut cursor = CursorStyle::Arrow;
        let mut animating = false;
        if let Some(props) = &self.props {
            let app: &App = cx;
            let view = View::read(props, app);
            let state = &mut self.state;
            state.frames += 1;
            super::PAINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            state.sync(&view);
            let started = Instant::now();
            let key = state.height_index.borrow().key;
            crate::frame_log::time("diff.index", || {
                view.ensure_height_index(&state.height_index, &state.geometry, bounds.width);
            });
            view.settle_extent(&state.height_index, &state.geometry);
            if state.height_index.borrow().key != key {
                state.last_index_build = started.elapsed();
            }
            let prev_offset = state.vertical_offset;
            state.apply_pending(&view, bounds, &mut out);
            if state.vertical_offset != prev_offset {
                // A jump: the wheel's leftover motion would drag the view off it.
                state.wheel.reset();
            }
            if let Some(offset) = state.bar.take_requested() {
                let (max_vertical, _) = state.limits(&view, bounds);
                state.vertical_offset = offset.clamp(0.0, max_vertical);
                state.wheel.reset();
                state.publish_file(&view, &mut out);
            }
            let step = state.wheel.take(Instant::now()) as f32;
            if step != 0.0 {
                crate::frame_log::value("sy", f64::from(step));
            }
            if step != 0.0 {
                let (max_vertical, _) = state.limits(&view, bounds);
                let before = state.vertical_offset;
                state.vertical_offset = (before + step).clamp(0.0, max_vertical);
                if state.vertical_offset != before {
                    state.publish_file(&view, &mut out);
                    // Rows slid under a still cursor: its hover moves with them.
                    state.refresh_hover(&view, bounds, bounds.contains(mouse).then_some(mouse));
                }
            }
            animating = state.step_auto_scroll(&view, bounds, &mut out) || state.wheel.pending();
            if state.vertical_offset != prev_offset {
                send(&mut out, &props.on_scroll, state.vertical_offset);
            }
            state.bar.publish(
                bounds,
                state.height_index.borrow().total_height,
                state.vertical_offset,
            );
            state.painted_offset = (state.vertical_offset * scale).round() / scale;
            crate::frame_log::time("diff.plan", || view.build_plan(state, bounds, &mut plan));
            cursor = state.cursor_at(&view, bounds, mouse);
        }
        if !out.is_empty() {
            window.defer(cx, move |window, cx| run(out, window, cx));
        }
        Frame {
            hitbox,
            plan,
            cursor,
            animating,
        }
    }

    /// Runs `f` with the view of the document. `f` returns whether to repaint. Callbacks
    /// run after the entity's update has returned.
    fn handle(
        entity: &Entity<Self>,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&View<'_>, &mut State, &mut Outbox) -> bool,
    ) {
        let out = entity.update(cx, |this, cx| {
            let mut out = Outbox::new();
            let Some(props) = &this.props else {
                return out;
            };
            let app: &App = cx;
            let view = View::read(props, app);
            let repaint = f(&view, &mut this.state, &mut out);
            if repaint {
                cx.notify();
            }
            out
        });
        run(out, window, cx);
    }

    /// Selection's text under the view's current state, for the copy shortcut.
    pub(super) fn copy(&self, cx: &App) -> Option<(String, Callback<String>)> {
        let props: &DiffProps = self.props.as_ref()?;
        let callback = props.on_copy.clone()?;
        let view = View::read(props, cx);
        Some((self.state.selected_text(&view)?, callback))
    }
}

impl Element for DiffElement {
    type RequestLayoutState = ();
    type PrepaintState = Frame;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Frame {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let rect: Rectangle = bounds.into();
        crate::frame_log::time("diff.prepare", || {
            self.view
                .update(cx, |view, cx| view.prepare(rect, hitbox, window, cx))
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut Frame,
        window: &mut Window,
        cx: &mut App,
    ) {
        let rect: Rectangle = bounds.into();
        let plan = std::mem::replace(&mut frame.plan, Plan::new(window.text_system().clone(), 0));
        crate::frame_log::time("diff.paint", || {
            window.with_content_mask(Some(gpui_kit::ContentMask { bounds }), |window| {
                plan.flush(window, cx);
            });
        });
        window.set_cursor_style(frame.cursor, &frame.hitbox);
        if frame.animating {
            window.request_animation_frame();
        }

        let hitbox = frame.hitbox.clone();
        let entity = self.view.clone();
        window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox.should_handle_scroll(window) {
                return;
            }
            crate::frame_log::time("wheel", || {
                DiffView::handle(&entity, window, cx, |view, state, out| {
                    state.on_wheel(view, rect, event, out)
                })
            });
            cx.stop_propagation();
        });

        let hitbox = frame.hitbox.clone();
        let entity = self.view.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            if !hitbox.is_hovered(window) {
                // Click outside our bounds: drop any existing selection so it doesn't
                // visually persist after the user moves on.
                DiffView::handle(&entity, window, cx, |_, state, _| {
                    let had = state.selection_anchor.is_some();
                    if had {
                        state.clear_selection();
                    }
                    had
                });
                return;
            }
            let focus = entity.read(cx).focus.clone();
            window.focus(&focus, cx);
            let point: Point = event.position.into();
            let modifiers = event.modifiers;
            DiffView::handle(&entity, window, cx, |view, state, out| {
                state.on_press(view, rect, point, modifiers, out);
                true
            });
            cx.stop_propagation();
        });

        let entity = self.view.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let point: Point = event.position.into();
            let held = event.pressed_button == Some(MouseButton::Left);
            DiffView::handle(&entity, window, cx, |view, state, out| {
                state.on_move(view, rect, point, held, out)
            });
        });

        let entity = self.view.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            DiffView::handle(&entity, window, cx, |_, state, _| state.on_release());
        });
    }
}

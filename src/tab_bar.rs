//! The multi-repo title-bar strip: one tab per open repository plus the
//! controls that manage them.
//!
//! Layout, left → right: a tab for each open repo (VCS badge, `owner/name`
//! with the owner dimmed, an optional uncommitted-changes dot, and a close
//! `×`), a `+` button that opens the path dialog, and — pinned right — a
//! `⌘K` control that opens the command palette (the design's repo-search
//! `⌘P` is folded into our existing palette per the spec). Dragging a tab
//! along the strip reorders it; see [`StripState`].
//!
//! The strip and the path dialog both read straight off `&Diffui`, mirroring
//! `sidebar` / `palette` / `find`, so there is no intermediate view model to
//! keep in sync.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Bounds, Div, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Window,
    canvas, deferred, div, px,
};

use crate::chrome;
use crate::input;
use crate::measure;
use crate::repository::Vcs;
use crate::theme::{
    self, Font, ThemeSpec, Weight, chip_background, emphasis_font, radius, text_size, well_fill,
};
use crate::typography::{self, Aligned};
use crate::{Action, Cx, Diffui, HoverTarget, TabId, icons, ui};

/// Focus target id for the open-repository dialog's path field.
pub const OPEN_REPO_INPUT_ID: &str = "open-repo-input";

const TAB_TEXT_SIZE: f32 = text_size::UI;

/// Fixed tab height: tall enough for the label row plus the hover wash's
/// breathing room, and the unit the strip's other controls center against.
/// Tabs run flush to the strip's bottom edge (browser-style), so this — plus
/// the strip's top padding — decides the whole strip's height.
const TAB_HEIGHT: f32 = 29.0;

/// Gap between the inactive hover wash's bottom edge and the strip floor —
/// what keeps the wash reading as floating rather than connected like the
/// active tab.
const WASH_BOTTOM_GAP: f32 = 3.0;

/// Gap between neighboring tabs, and between the final tab and the add button.
const TAB_GAP: f32 = 3.0;

/// Label padding inside a tab's fill / hover wash.
const LABEL_PAD_X: f32 = 8.0;

/// Narrowest a squeezed tab gets: its badge, a few characters and its `×`.
const TAB_MIN_WIDTH: f32 = 96.0;

/// The close button's square, which also holds the dirty dot.
const CLOSE_SIDE: f32 = 18.0;

/// Horizontal travel before a press on a tab becomes a drag — the revision
/// list's threshold, so a click that wobbles stays a click in both places.
/// Vertical travel never counts: the strip only reorders along its own axis.
const DRAG_THRESHOLD: f32 = 6.0;

/// How far past a neighbor's center the dragged tab's leading edge has to go
/// before the two swap. Without it the swap point going right and the one
/// coming back sit a `TAB_GAP` apart, and a hand resting there flips the pair
/// on every tremor.
const SWAP_HYSTERESIS: f32 = 4.0;

/// Time constant of the slide that carries a displaced (or dropped) tab to
/// its slot: each `SLIDE_TAU` closes ~63% of the remaining distance, so a
/// slide is visually done in about three of them.
const SLIDE_TAU: f32 = 0.05;

/// The open-repo card, sized and inset like a palette column.
const OPEN_DIALOG_WIDTH: f32 = 520.0;
/// Height of the card's input band.
const OPEN_INPUT_BAND: f32 = 52.0;
/// Shared left edge of the input text, the section label and row glyphs.
const OPEN_CONTENT_X: f32 = theme::space::XL;
/// Inset of the recent list's hover washes inside the card.
const OPEN_LIST_INSET: f32 = theme::space::XS;
const OPEN_SECTION_HEIGHT: f32 = 28.0;
const OPEN_SECTION_PILL: f32 = 22.0;
const OPEN_SECTION_PILL_PAD_X: f32 = theme::space::SM;
const OPEN_ROW_HEIGHT: f32 = 34.0;
/// Rows the card's list shows before it scrolls: as many as the folder
/// suggestions can be, so typing a path and clearing it again keeps the card
/// the same size once the recents fill it.
const OPEN_LIST_ROWS: usize = crate::path_browse::MAX_SUGGESTIONS;
const OPEN_GLYPH_BOX: f32 = 16.0;
const OPEN_GLYPH_GAP: f32 = 10.0;
const OPEN_REMOVE_SIDE: f32 = 24.0;

/// The strip's height: the title-bar height where it stands in for one,
/// otherwise its top padding plus the tabs.
pub fn height() -> f32 {
    chrome::title_bar_height().unwrap_or(STRIP_TOP_PAD + TAB_HEIGHT)
}

/// Top padding of the strip where it sizes to its own content.
const STRIP_TOP_PAD: f32 = 6.0;

/// The slot a dragged tab belongs in, given `widths` in strip order, the
/// dragged tab's current `index` with its slot starting at `slot`, and the
/// position `x` its left edge has been dragged to.
///
/// It trades places with a neighbor once its leading edge is
/// [`SWAP_HYSTERESIS`] past that neighbor's center, however wide either tab
/// is — center against center would have a wide tab cover a narrow one
/// completely before they swapped. A fast drag can cross several neighbors
/// between two events, so this walks until the next one holds.
fn drop_index(widths: &[f32], index: usize, slot: f32, x: f32) -> usize {
    let mut widths = widths.to_vec();
    let (mut index, mut slot) = (index, slot);
    // Rightward: its right edge against the next tab's center.
    while let Some(next) = widths.get(index + 1).copied() {
        let center = slot + widths[index] + TAB_GAP + next / 2.0;
        if x + widths[index] <= center + SWAP_HYSTERESIS {
            break;
        }
        slot += next + TAB_GAP;
        widths.swap(index, index + 1);
        index += 1;
    }
    // Leftward: its left edge against the previous tab's center.
    while index > 0 {
        let previous = widths[index - 1];
        let center = slot - TAB_GAP - previous / 2.0;
        if x >= center - SWAP_HYSTERESIS {
            break;
        }
        slot -= previous + TAB_GAP;
        widths.swap(index, index - 1);
        index -= 1;
    }
    index
}

/// The tab's hover label: where it points, and what the ring means when it
/// shows.
fn tab_tooltip(tab: &crate::Tab) -> String {
    let place = match &tab.source {
        crate::TabSource::Repo { root, .. } => crate::contract_user_path(&root.to_string_lossy()),
        crate::TabSource::GitHubPr(_) => format!("{}/{}", tab.owner, tab.name),
    };
    if tab_is_dirty(tab) {
        format!("{place}\nWorking copy (@) has changes")
    } else {
        place
    }
}

/// True when the repo behind `tab` has a non-empty working copy. Best-effort:
/// the working copy's emptiness may still be unresolved (it resolves lazily),
/// in which case we draw no dot rather than guess.
fn tab_is_dirty(tab: &crate::Tab) -> bool {
    // PR tabs have no working copy; their synthetic "All changes" row would
    // otherwise read as permanently dirty.
    if matches!(tab.source, crate::TabSource::GitHubPr(_)) {
        return false;
    }
    tab.state
        .session
        .commits
        .working_copy()
        .and_then(|wc| wc.is_empty())
        == Some(false)
}

/// What the strip remembers between frames. Everything that makes a reorder look
/// like motion lives here; the order itself is the app's, and a drag publishes
/// [`Action::MoveTab`] each time the dragged tab crosses a neighbour, so the strip
/// is re-laid out around it while it is still in the air.
#[derive(Debug, Default)]
pub(crate) struct StripState {
    /// Armed on a tab press, active past [`DRAG_THRESHOLD`].
    drag: Option<TabDrag>,
    /// How far each tab is drawn from its slot, easing to zero. Tabs at rest
    /// have no entry.
    slides: Vec<(TabId, f32)>,
    /// Each tab's slot as of the last paint: what a reorder is measured against
    /// to start the slides above, and where a press takes hold of a tab.
    slots: Vec<Slot>,
    /// The tab a drop left in the air. Drawn above its neighbours until the
    /// slides settle, like the drag it just ended.
    settling: Option<TabId>,
    /// When the slides were last stepped.
    last_frame: Option<Instant>,
    /// A press on the strip's empty area, until it moves (the window drag) or
    /// lifts.
    window_drag_armed: bool,
}

/// Shared with the listeners a frame leaves behind, and with the key path for Esc.
pub(crate) type Strip = Rc<RefCell<StripState>>;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Slot {
    id: TabId,
    x: f32,
    width: f32,
}

#[derive(Debug)]
struct TabDrag {
    id: TabId,
    /// Where the press landed: the activation threshold's anchor.
    origin_x: f32,
    /// How far into the tab the cursor sits, so the tab stays under it the way
    /// it was picked up. Taken when the drag activates, not at the press.
    grab: f32,
    /// The slot the tab was picked up from, where Esc puts it back.
    home: usize,
    /// Whether the threshold was crossed (a plain click never activates).
    active: bool,
    cursor_x: f32,
}

impl StripState {
    fn dragged(&self) -> Option<&TabDrag> {
        self.drag.as_ref().filter(|drag| drag.active)
    }

    fn add_slide(&mut self, id: TabId, offset: f32) {
        match self.slides.iter_mut().find(|(tab, _)| *tab == id) {
            Some((_, current)) => *current += offset,
            None => self.slides.push((id, offset)),
        }
    }

    /// Where an active drag puts its tab's left edge: under the cursor, held
    /// within the run of tabs so it can't be dragged over the `+` or off the
    /// strip.
    fn dragged_x(&self, drag: &TabDrag) -> Option<f32> {
        let slot = self.slots.iter().find(|slot| slot.id == drag.id)?;
        let first = self.slots.first()?;
        let last = self.slots.last()?;
        let max_x = (last.x + last.width - slot.width).max(first.x);
        Some((drag.cursor_x - drag.grab).clamp(first.x, max_x))
    }

    /// How far tab `id` is drawn, and hit-tested, from its slot.
    fn offset(&self, id: TabId) -> f32 {
        if let Some(drag) = self.dragged().filter(|drag| drag.id == id)
            && let (Some(x), Some(slot)) = (
                self.dragged_x(drag),
                self.slots.iter().find(|slot| slot.id == id),
            )
        {
            return x - slot.x;
        }
        // Whole pixels: a tab at rest keeps the exact position the layout gave
        // it, and one in motion doesn't shimmer between them.
        self.slides
            .iter()
            .find(|(tab, _)| *tab == id)
            .map_or(0.0, |(_, offset)| offset.round())
    }

    /// Let go of an active drag: the tab slides into its slot from wherever the
    /// cursor left it.
    fn drop_tab(&mut self, drag: &TabDrag) {
        let Some(x) = self.dragged_x(drag) else {
            return;
        };
        if let Some(slot) = self.slots.iter().find(|slot| slot.id == drag.id).copied() {
            self.add_slide(drag.id, x - slot.x);
            self.settling = Some(drag.id);
        }
    }

    /// Brings the state up to `ids`, the order about to be drawn. A reorder lands
    /// here as tabs whose slots moved: each keeps being drawn where it was and
    /// slides to the new slot from there, all but the dragged tab, which is
    /// wherever the cursor is. Slot widths don't change on a reorder, so the new
    /// slots are known before layout and the first frame already shows the slide.
    fn sync_order(&mut self, ids: &[TabId]) {
        // A tab can close under a drag (⌘W needs no pointer).
        if self
            .drag
            .as_ref()
            .is_some_and(|drag| !ids.contains(&drag.id))
        {
            self.drag = None;
        }
        self.slides.retain(|(id, _)| ids.contains(id));
        let same_tabs = self.slots.len() == ids.len()
            && ids
                .iter()
                .all(|id| self.slots.iter().any(|slot| slot.id == *id));
        let reordered = !self
            .slots
            .iter()
            .map(|slot| slot.id)
            .eq(ids.iter().copied());
        if !same_tabs || !reordered {
            return;
        }
        let Some(start) = self.slots.first().map(|slot| slot.x) else {
            return;
        };
        let dragged = self.dragged().map(|drag| drag.id);
        let animate = dragged.is_some() || self.settling.is_some();
        let mut x = start;
        let mut slots = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(old) = self.slots.iter().find(|slot| slot.id == *id).copied() else {
                return;
            };
            if animate && Some(*id) != dragged && (old.x - x).abs() > 0.5 {
                self.add_slide(*id, old.x - x);
            }
            slots.push(Slot { x, ..old });
            x += old.width + TAB_GAP;
        }
        self.slots = slots;
    }

    /// Eases the slides by the time since the last frame. Whether any are left.
    fn step(&mut self, now: Instant) -> bool {
        if self.slides.is_empty() {
            self.settling = None;
            self.last_frame = None;
            return false;
        }
        let elapsed = self
            .last_frame
            .map_or(0.0, |last| now.duration_since(last).as_secs_f32());
        self.last_frame = Some(now);
        let remaining = (-elapsed / SLIDE_TAU).exp();
        for (_, offset) in &mut self.slides {
            *offset *= remaining;
        }
        self.slides.retain(|(_, offset)| offset.abs() >= 0.5);
        true
    }
}

#[cfg(test)]
impl StripState {
    pub(crate) fn last_slot_end(&self) -> f32 {
        self.slots.last().map_or(0.0, |slot| slot.x + slot.width)
    }

    /// The left and right edges of tab `index`'s slot.
    pub(crate) fn slot_span(&self, index: usize) -> (f32, f32) {
        let slot = self.slots[index];
        (slot.x, slot.x + slot.width)
    }

    pub(crate) fn at_rest(&self) -> bool {
        self.drag.is_none() && self.slides.is_empty()
    }
}

impl Diffui {
    /// A press on a tab selects it and arms a reorder drag.
    fn on_tab_pressed(&mut self, id: TabId, x: f32, cx: &mut Cx) {
        self.perform(Action::SelectTab(id), cx);
        let mut strip = self.tab_strip.borrow_mut();
        // A release that never arrived must not leave a tab stuck to the cursor
        // past the next press. A lone tab has nowhere to go.
        strip.drag = None;
        if self.tabs.len() > 1
            && let Some(home) = self.tabs.iter().position(|tab| tab.id == id)
        {
            strip.drag = Some(TabDrag {
                id,
                origin_x: x,
                grab: 0.0,
                home,
                active: false,
                cursor_x: x,
            });
        }
    }

    fn on_tab_drag_moved(&mut self, x: f32, cx: &mut Cx) {
        let target = {
            let mut strip = self.tab_strip.borrow_mut();
            let Some((id, active, origin_x)) = strip
                .drag
                .as_ref()
                .map(|drag| (drag.id, drag.active, drag.origin_x))
            else {
                return;
            };
            if !active && (x - origin_x).abs() < DRAG_THRESHOLD {
                return;
            }
            let Some(index) = strip.slots.iter().position(|slot| slot.id == id) else {
                return;
            };
            // Take hold of the tab where it is drawn right now, not where the
            // press landed: anchored to the press it would jump the threshold's
            // worth the moment it came loose. From here the cursor places it, so
            // any slide it was in ends.
            if !active {
                let grab = x - (strip.slots[index].x + strip.offset(id));
                strip.slides.retain(|(tab, _)| *tab != id);
                if let Some(drag) = strip.drag.as_mut() {
                    drag.active = true;
                    drag.grab = grab;
                }
            }
            if let Some(drag) = strip.drag.as_mut() {
                drag.cursor_x = x;
            }
            let widths: Vec<f32> = strip.slots.iter().map(|slot| slot.width).collect();
            let slot = strip.slots[index].x;
            let dragged_x = strip
                .drag
                .as_ref()
                .and_then(|drag| strip.dragged_x(drag))
                .unwrap_or(slot);
            let target = drop_index(&widths, index, slot, dragged_x);
            (target != index).then_some((id, target))
        };
        if let Some((id, to)) = target {
            self.perform(Action::MoveTab { id, to }, cx);
        }
        cx.notify();
    }

    fn on_tab_drag_released(&mut self, cx: &mut Cx) {
        let mut strip = self.tab_strip.borrow_mut();
        strip.window_drag_armed = false;
        let Some(drag) = strip.drag.take() else {
            return;
        };
        if drag.active {
            strip.drop_tab(&drag);
        }
        drop(strip);
        cx.notify();
    }

    /// Esc abandons a drag and puts the tab back where it was picked up. The
    /// app's own Esc handling still runs on the same key, as it does for a
    /// revision drag.
    pub(crate) fn cancel_tab_drag(&mut self, cx: &mut Cx) {
        let home = {
            let mut strip = self.tab_strip.borrow_mut();
            let Some(drag) = strip.drag.take() else {
                return;
            };
            if !drag.active {
                return;
            }
            strip.drop_tab(&drag);
            (drag.id, drag.home)
        };
        self.perform(
            Action::MoveTab {
                id: home.0,
                to: home.1,
            },
            cx,
        );
    }
}

/// Build the title-bar tab strip. Empty when no repos are open (the welcome
/// screen owns the window in that case).
pub(crate) fn build_tab_bar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if ui.tabs.is_empty() {
        return div().into_any_element();
    }
    let ids: Vec<TabId> = ui.tabs.iter().map(|tab| tab.id).collect();
    let strip = ui.tab_strip.clone();
    let (dragging, raised) = {
        let mut state = strip.borrow_mut();
        state.sync_order(&ids);
        // Losing the window ends a drag where it is: the release may land elsewhere.
        if !ui.app_focused
            && let Some(drag) = state.drag.take()
            && drag.active
        {
            state.drop_tab(&drag);
        }
        if state.step(Instant::now()) {
            window.request_animation_frame();
        }
        let dragged = state.dragged().map(|drag| drag.id);
        (dragged.is_some(), dragged.or(state.settling))
    };

    let mut tabs = div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(TAB_GAP))
        // Clears the active tab's left join, which the clip would cut.
        .ml(px(-radius::BUTTON))
        .pl(px(radius::BUTTON))
        .h(px(TAB_HEIGHT))
        .min_w_0()
        .flex_shrink(1.0)
        .overflow_hidden();
    for (index, tab) in ui.tabs.iter().enumerate() {
        let offset = strip.borrow().offset(tab.id);
        let lifted = raised == Some(tab.id);
        let body = tab_widget(
            ui,
            theme,
            tab,
            index == ui.active,
            lifted,
            offset,
            dragging,
            cx,
        );
        // The slot holds the tab's place in the row, so its bounds stay the slot's
        // whatever the tab inside is drawn offset by.
        let slot = div()
            .flex_shrink(1.0)
            .min_w(px(TAB_MIN_WIDTH.min(body.1)))
            .h(px(TAB_HEIGHT));
        // The tab in the air paints above the labels it passes over.
        tabs = tabs.child(if lifted {
            slot.child(deferred(body.0).with_priority(1))
        } else {
            slot.child(body.0)
        });
    }
    let recorded = strip.clone();
    let tabs = tabs.on_children_prepainted(move |bounds: Vec<Bounds<Pixels>>, _, _| {
        let mut state = recorded.borrow_mut();
        state.slots = ids
            .iter()
            .zip(bounds)
            .map(|(id, bounds)| Slot {
                id: *id,
                x: f32::from(bounds.origin.x),
                width: f32::from(bounds.size.width),
            })
            .collect();
    });

    // A drag has to keep tracking the cursor after it leaves the tab that started
    // it, so these listen to the whole window. They are registered in every frame
    // rather than only during a drag: a press and release can land between frames.
    let on_move = Diffui::listen(cx, |this, event: &MouseMoveEvent, _, cx| {
        if event.pressed_button == Some(MouseButton::Left) {
            this.on_tab_drag_moved(f32::from(event.position.x), cx);
        } else {
            // A release we never saw: it landed outside the app's event stream.
            this.on_tab_drag_released(cx);
        }
    });
    let on_up = Diffui::listen(cx, |this, _: &MouseUpEvent, _, cx| {
        this.on_tab_drag_released(cx);
    });
    let moving = strip.clone();
    let lifting = strip.clone();
    let listeners = canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx: &mut App| {
                // Dragging only: this runs on every move anywhere in the window.
                if phase.bubble() && moving.borrow().drag.is_some() {
                    on_move(event, window, cx);
                }
            });
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx: &mut App| {
                let watched = {
                    let state = lifting.borrow();
                    state.drag.is_some() || state.window_drag_armed
                };
                if phase.bubble() && event.button == MouseButton::Left && watched {
                    on_up(event, window, cx);
                }
            });
        },
    )
    .absolute()
    .size_0();

    // Top padding only: the tabs run flush to the strip's bottom edge so the
    // active one connects to the toolbar band below. When the strip stands in for
    // the title bar (macOS) it's pinned to a fixed height with the content
    // bottom-aligned instead; the traffic lights stay centered on the full strip
    // (see `chrome::titlebar_options`), like a native browser window.
    let inset = chrome::leading_inset();
    let row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(8.0))
        .h(px(TAB_HEIGHT))
        .w_full()
        .when(inset > 0.0, |row| row.child(div().w(px(inset)).flex_none()))
        // `+` and the palette control stay pinned: only the tabs give way.
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(TAB_GAP))
                .flex_shrink(1.0)
                .min_w_0()
                .child(tabs)
                .child(add_button(theme, cx)),
        )
        .child(div().flex_1().min_w_0())
        .child(palette_hint(theme, ui.config.mono_font, cx));

    // Recessed, darker than the toolbar below it, so the active tab, filled with
    // the toolbar's color, reads as a connected block against it (browser-style).
    // No border: the seam between the active tab and the toolbar must stay
    // invisible.
    let bar = div()
        .id("tab-bar")
        .relative()
        .flex()
        .flex_col()
        .justify_end()
        .w_full()
        .h(px(height()))
        .flex_none()
        .bg(well_fill(theme))
        .child(listeners)
        .child(row);
    let bar = if dragging {
        // An active drag owns the cursor: no hover washes, no `×` presses.
        let size = window.viewport_size();
        bar.child(deferred(
            div()
                .id("tab-drag-cursor")
                .absolute()
                .top_0()
                .left_0()
                .w(size.width)
                .h(size.height)
                .occlude()
                .cursor_grabbing(),
        ))
    } else {
        bar
    };

    // When the native title bar is hidden/replaced (macOS today), the strip's
    // empty area doubles as the window-drag handle: tabs and buttons take their
    // own presses, so only the gaps start a drag. A double-click on that same area
    // runs the system zoom/minimize action, matching a native title bar.
    if chrome::drag_region() {
        let armed = strip.clone();
        let moving = strip;
        bar.on_click(|event, window, _| {
            if event.click_count() == 2 {
                window.titlebar_double_click();
            }
        })
        .on_mouse_down(MouseButton::Left, move |_, _, _| {
            armed.borrow_mut().window_drag_armed = true;
        })
        .on_mouse_move(move |_, window, _| {
            let mut state = moving.borrow_mut();
            if state.window_drag_armed {
                state.window_drag_armed = false;
                window.start_window_move();
            }
        })
        .into_any_element()
    } else {
        bar.into_any_element()
    }
}

/// One repo tab, and its natural width's floor for the slot. The trailing `×`
/// closes it; a press anywhere else selects it and arms a reorder drag.
#[allow(clippy::too_many_arguments)]
fn tab_widget(
    ui: &Diffui,
    theme: ThemeSpec,
    tab: &crate::Tab,
    active: bool,
    lifted: bool,
    offset: f32,
    dragging: bool,
    cx: &mut Cx,
) -> (gpui_kit::Stateful<gpui_kit::Div>, f32) {
    let id = tab.id;
    let element = id.0 as usize;
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let hovered = !dragging && ui.hovered == Some(HoverTarget::Tab(id));

    let badge = tab_badge(&tab.source, active, theme, mono);

    let name_color = if active { theme.text } else { theme.muted_text };
    // owner + name as a tight unit (no gap), matching the design's RepoLabel:
    // "code/diffui" reads as one path. The badge gets the 6px rhythm from the
    // outer row instead. The mono badge and the sans name share a baseline.
    let name_font = emphasis_font(font, Weight::MEDIUM);
    let mut spans = Vec::new();
    if !tab.owner.is_empty() {
        spans.push(
            crate::ellipsis::Span::new(format!("{}/", tab.owner))
                .font(font)
                .color(theme.subtle_text),
        );
    }
    spans.push(crate::ellipsis::Span::new(tab.name.clone()));
    let name = div()
        .text_size(px(TAB_TEXT_SIZE))
        .whitespace_nowrap()
        .text_ellipsis()
        .child(crate::ellipsis::rich_text(&spans, name_font, name_color));
    // The badge's text is two steps under the name's, so its box centres on the
    // tab rather than sitting on the name's baseline.
    let label = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .min_w_0()
        .child(badge)
        .child(typography::baseline_row(
            [Aligned::text(name, name_font, TAB_TEXT_SIZE).shrink()],
            0.0,
        ));
    // The dot gives way to the `×` under the cursor, so it can't carry its own
    // tooltip; the label says what the tab is and what the dot means.
    let label = div()
        .id(("tab-label", element))
        .min_w_0()
        .flex_shrink(1.0)
        .child(label)
        .tooltip(ui::tooltip(tab_tooltip(tab), font, theme));

    // The uncommitted-changes dot lives in the close button's slot and gives way
    // to the `×` under the cursor, as in editor tabs: every tab keeps one trailing
    // slot, so the gap after the name is the same whether or not a repo is dirty,
    // and the tab's width never jumps as that changes.
    let trailing = if tab_is_dirty(tab) && !hovered {
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(CLOSE_SIDE))
            .child(dirty_dot(theme))
            .into_any_element()
    } else {
        ui::button(
            ("tab-close", element),
            ui::Variant::Ghost,
            ui::Size::Compact,
            theme,
            cx,
        )
        .size(px(CLOSE_SIDE))
        .p(px(0.0))
        .flex_none()
        .child(icons::icon(icons::CLOSE, 13.0, theme.subtle_text))
        .on_click(Diffui::on_action(Action::CloseTab(id), cx))
        .into_any_element()
    };

    // Inactive tabs stay quiet; hovering one paints a floating wash over the same
    // box minus `WASH_BOTTOM_GAP`. The label centers in the same top-anchored box
    // in every state, so nothing shifts on hover or selection.
    let wash_fill = (!active && (hovered || lifted)).then(|| chip_background(theme.muted_text));
    let wash = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::XS))
        .w_full()
        .h(px(TAB_HEIGHT - WASH_BOTTOM_GAP))
        .px(px(LABEL_PAD_X))
        .rounded(px(radius::BUTTON))
        .when_some(wash_fill, |wash, fill| wash.bg(fill))
        .child(label)
        .child(div().ml_auto().flex_none().child(trailing));

    let on_hover = cx.listener(move |this: &mut Diffui, entered: &bool, _, cx| {
        if *entered {
            this.hovered = Some(HoverTarget::Tab(id));
            cx.notify();
        // Crossing between neighbours can report the new tab first.
        } else if this.hovered == Some(HoverTarget::Tab(id)) {
            this.hovered = None;
            cx.notify();
        }
    });
    let on_press = Diffui::listen(cx, move |this, event: &gpui_kit::MouseDownEvent, _, cx| {
        this.on_tab_pressed(id, f32::from(event.position.x), cx);
        cx.stop_propagation();
    });

    // Browser-style tab: the active tab is a top-rounded block filled with the
    // toolbar band's own color and running flush to the strip's bottom edge. Its
    // concave joins share the top corners' radius.
    let body = div()
        .id(("tab", element))
        .relative()
        .left(px(offset))
        .w_full()
        .h(px(TAB_HEIGHT))
        .when(active, |body| {
            body.bg(theme.panel_background_elevated)
                .rounded_t(px(radius::BUTTON))
                .child(join(theme, true))
                .child(join(theme, false))
        })
        // Only the active tab has a fill to hide what it passes over; a background
        // one in the air (its activation was refused) gets a stand-in.
        .when(lifted && !active, |body| {
            body.child(
                div()
                    .absolute()
                    .inset_0()
                    .bottom(px(WASH_BOTTOM_GAP))
                    .rounded(px(radius::BUTTON))
                    .bg(well_fill(theme)),
            )
        })
        .child(div().relative().w_full().child(wash))
        .cursor_pointer()
        .on_hover(on_hover)
        .on_mouse_down(MouseButton::Left, on_press);
    (body, tab_natural_width(ui, tab))
}

/// A tab's width with nothing cut, so a short name doesn't get stretched to the
/// squeezed tabs' floor.
fn tab_natural_width(ui: &Diffui, tab: &crate::Tab) -> f32 {
    let name_font = emphasis_font(ui.config.ui_font, Weight::MEDIUM);
    let owner = if tab.owner.is_empty() {
        0.0
    } else {
        measure::line_width(&format!("{}/", tab.owner), TAB_TEXT_SIZE, ui.config.ui_font)
    };
    owner
        + measure::line_width(&tab.name, TAB_TEXT_SIZE, name_font)
        + 2.0 * LABEL_PAD_X
        + theme::space::XS
        + CLOSE_SIDE
}

/// The concave join between one of the active tab's bottom corners and the
/// toolbar band: the tab's fill in the square outside the corner, with the strip's
/// color over it rounded off.
fn join(theme: ThemeSpec, left: bool) -> impl IntoElement {
    let side = px(radius::BUTTON);
    let square = div().absolute().bottom_0().size(side);
    let square = if left {
        square.left(-side)
    } else {
        square.right(-side)
    };
    let cover = div().size_full().bg(well_fill(theme));
    let cover = if left {
        cover.rounded_br(side)
    } else {
        cover.rounded_bl(side)
    };
    square.bg(theme.panel_background_elevated).child(cover)
}

/// A tab's `jj` / `git` / `pr` badge: says what the tab holds. Neutral, and
/// in the accent on the active tab, so colour never competes with the state
/// the panes below show.
fn tab_badge(source: &crate::TabSource, active: bool, theme: ThemeSpec, mono: Font) -> Div {
    let label = match source {
        crate::TabSource::Repo { vcs: Vcs::Jj, .. } => "jj",
        crate::TabSource::Repo { vcs: Vcs::Git, .. } => "git",
        crate::TabSource::GitHubPr(_) => "pr",
    };
    let (text, fill) = if active {
        (theme.accent, chip_background(theme.accent))
    } else {
        (theme.muted_text, chip_background(theme.muted_text))
    };
    let font = emphasis_font(mono, Weight::SEMIBOLD);
    typography::trimmed_padding([1.0, 5.0].into(), font, text_size::BADGE)
        .apply(div())
        .flex_none()
        .bg(fill)
        .rounded(px(radius::XS))
        .child(typography::line(label, font, text_size::BADGE, text))
}

/// `jj` / `git` badge: a soft colored chip carrying the VCS kind, so a mixed set
/// of repos reads at a glance.
fn vcs_badge(vcs: Vcs, theme: ThemeSpec, mono: Font) -> Aligned {
    let (label, color) = match vcs {
        // Violet for jj (the trunk/lane hue), warm amber for git: distinct from
        // the diff add/del greens and reds.
        Vcs::Jj => ("jj", theme.lane_base),
        Vcs::Git => ("git", theme.modified_token),
    };
    Aligned::chip(
        typography::line(label, mono, text_size::BADGE, color),
        mono,
        text_size::BADGE,
        [1.0, 4.0].into(),
        move |chip| chip.bg(chip_background(color)).rounded(px(radius::XS)),
    )
}

/// Small ring signalling the working copy (`@`) has changes. A ring in the
/// "modified" colour rather than a filled accent dot: in an editor a filled dot
/// means unsaved work, and jj has none; it snapshots `@` on its own.
fn dirty_dot(theme: ThemeSpec) -> impl IntoElement {
    div()
        .size(px(7.0))
        .rounded(px(3.5))
        .border(px(1.5))
        .border_color(theme.modified_token)
}

/// `+` button: opens the path dialog to add another repository. A square sized
/// and anchored exactly like an inactive tab's hover wash (same height, same top
/// edge, same clearance from the strip floor), so the strip's controls sit on one
/// visual grid.
fn add_button(theme: ThemeSpec, cx: &mut Cx) -> impl IntoElement {
    let side = TAB_HEIGHT - WASH_BOTTOM_GAP;
    ui::button("tab-add", ui::Variant::Ghost, ui::Size::Compact, theme, cx)
        .size(px(side))
        .p(px(0.0))
        .flex_none()
        .child(icons::icon(icons::PLUS, 15.0, theme.muted_text))
        .on_click(Diffui::on_action(Action::OpenRepoDialog, cx))
}

/// The right-hand `⌘K` control. The design put a repo-search `⌘P` here; per the
/// spec it instead opens our existing command palette. Styled as a keycap
/// (hairline border, mono glyphs) so it reads as "press this key", not as another
/// action button.
fn palette_hint(theme: ThemeSpec, mono: Font, cx: &mut Cx) -> impl IntoElement {
    use gpui_kit::base::TestSupportExt as _;
    div()
        .id("palette-hint")
        .test_support()
        .flex_none()
        .px(px(4.0))
        .py(px(2.0))
        .cursor_pointer()
        .child(
            div()
                .px(px(5.0))
                .py(px(1.0))
                .rounded(px(radius::XS))
                .border_1()
                .border_color(theme.border)
                .bg(chip_background(theme.muted_text))
                .child(typography::line(
                    chrome::cmd_label("K"),
                    mono,
                    text_size::CAPTION,
                    theme.muted_text,
                )),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(Diffui::on_action(Action::OpenPalette, cx))
}

/// Modal confirmation for a guarded mutation (see [`crate::ConfirmDialog`]). Empty
/// when closed. Lives here because it mirrors the open-repository modal's scrim +
/// card construction exactly.
pub(crate) fn build_confirm_dialog(
    ui: &Diffui,
    theme: ThemeSpec,
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Some(dialog) = ui.confirm() else {
        return div().into_any_element();
    };
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    // Red fill: the confirm runs a mutation the jj CLI refuses by default.
    let accept = ui::text_button(
        "confirm-accept",
        None,
        dialog.confirm_label.clone(),
        ui::Variant::Destructive,
        ui::Size::Standard,
        font,
        theme,
        cx,
    )
    .on_click(Diffui::on_action(Action::ConfirmAccept, cx));
    let cancel = ui::cancel_button("confirm-cancel", ui::Size::Standard, font, theme, cx)
        .on_click(Diffui::on_action(Action::ConfirmCancel, cx));

    let mut card = ui::dialog_card(ui::DIALOG_WIDTH, theme).child(ui::dialog_title(
        &dialog.title,
        font,
        mono,
        theme,
    ));
    if !dialog.body.is_empty() {
        card = card.child(ui::dialog_text(&dialog.body, font, mono, theme));
    }
    let card = card.child(ui::button_row(
        None,
        [cancel.into_any_element(), accept.into_any_element()],
    ));
    let dismiss = Diffui::on_action::<()>(Action::ConfirmCancel, cx);
    ui::modal(
        "confirm-dialog",
        card,
        crate::toolbar::modal_top(),
        move |window, cx| dismiss(&(), window, cx),
    )
    .into_any_element()
}

/// Modal overlay for opening a repository from a path. Empty when the dialog is
/// closed, or on the welcome screen, where the card is inline. Click-outside and
/// Esc cancel, so the card has no Cancel button.
pub(crate) fn build_open_repo_dialog(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Some(dialog) = ui.open_repo_dialog() else {
        return div().into_any_element();
    };
    let dismiss = Diffui::on_action::<()>(Action::CloseRepoDialog, cx);
    ui::modal(
        "open-repo-dialog",
        open_repo_card(ui, theme, dialog, window, cx),
        crate::toolbar::modal_top(),
        move |window, cx| dismiss(&(), window, cx),
    )
    .into_any_element()
}

/// The open prompt itself, shared by the modal and the welcome screen: a
/// palette-family card with one input band (path, folder picker, Open), an error
/// line when the last open failed, then the recent repositories.
pub(crate) fn open_repo_card(
    ui: &Diffui,
    theme: ThemeSpec,
    dialog: &crate::OpenRepoDialog,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let on_input = cx.listener(|this: &mut Diffui, path: &String, _, cx| {
        this.act(cx, |this, cx| {
            this.on_open_repo_path_changed(path.clone(), cx)
        });
    });
    let submit = Diffui::on_action::<()>(Action::SubmitRepoDialog, cx);
    let spec = input::FieldSpec::new(OPEN_REPO_INPUT_ID, &dialog.path)
        .placeholder("Path, owner/repo#123, or PR URL")
        .on_input(move |path, window, cx| on_input(&path, window, cx))
        .on_submit(move |window, cx| submit(&(), window, cx));
    // Bare: the card is the field.
    let field = input::single_line(spec, ui.config.mono_font, window, cx)
        .appearance(false)
        // On the card's content edge, not inset from it.
        .px(px(0.0))
        .text_size(px(text_size::BODY_LG));

    let mut trailing = div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(theme::space::XS));
    #[cfg(target_os = "macos")]
    {
        trailing = trailing.child(
            div()
                .id("open-repo-folder-tip")
                .tooltip(ui::tooltip("Choose folder…", ui.config.ui_font, theme))
                .child(
                    ui::icon_button(
                        "open-repo-folder",
                        icons::FOLDER_OPEN,
                        theme.muted_text,
                        ui::Size::Compact,
                        theme,
                        cx,
                    )
                    .on_click(Diffui::on_action(Action::ChooseRepoFolder, cx)),
                ),
        );
    }
    let enabled = !dialog.path.trim().is_empty();
    let mut open = ui::primary_button(
        "open-repo-submit",
        "Open",
        enabled,
        ui::Size::Compact,
        ui.config.ui_font,
        theme,
        cx,
    );
    if enabled {
        open = open.on_click(Diffui::on_action(Action::SubmitRepoDialog, cx));
    }
    let trailing = trailing.child(open);

    // The trailing controls sit as far in from the right edge as from the band's
    // top and bottom.
    let band_inset = (OPEN_INPUT_BAND - ui::Size::Compact.height()) / 2.0;
    let band = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(OPEN_INPUT_BAND))
        .pl(px(OPEN_CONTENT_X))
        .pr(px(band_inset))
        .gap(px(theme::space::MD))
        .text_size(px(text_size::BODY_LG))
        .child(div().flex_1().min_w_0().child(field))
        .child(trailing);

    let mut body = theme::modal_card(div(), theme)
        .w(px(OPEN_DIALOG_WIDTH))
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(band);

    if let Some(error) = &dialog.error {
        // The glyph centres on the first line's caps.
        let line = typography::TextBox::default_line(ui.config.ui_font, text_size::UI);
        body = body.child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(theme::space::MD))
                .pl(px(OPEN_CONTENT_X))
                .pr(px(OPEN_CONTENT_X))
                .pb(px(theme::space::LG))
                .child(
                    div()
                        .flex_none()
                        .pt(px(((line.cap_top() + line.baseline()) / 2.0
                            - ui::ICON_SIZE / 2.0)
                            .max(0.0)))
                        .child(icons::icon(
                            icons::ALERT_TRIANGLE,
                            ui::ICON_SIZE,
                            theme.removed_text,
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font(ui.config.ui_font.gpui())
                        .text_size(px(text_size::UI))
                        .text_color(theme.removed_text)
                        .child(error.clone()),
                ),
        );
    }

    // A path being typed lists the folders it can continue into; anything else
    // shows the recents.
    let section = match dialog.suggestions() {
        Some((query, rows)) => folder_section(ui, theme, dialog, query, rows, cx),
        None => recents_section(ui, theme, cx),
    };
    if let Some((header, list, rows)) = section {
        let list = if rows > OPEN_LIST_ROWS {
            // Past the fixed height the list scrolls, its bar at the card's edge
            // and the rows clear of it.
            div()
                .w_full()
                .pl(px(OPEN_LIST_INSET))
                .pb(px(OPEN_LIST_INSET))
                .child(
                    div()
                        .w_full()
                        .h(px(OPEN_LIST_ROWS as f32 * OPEN_ROW_HEIGHT))
                        .child(ui::scrollable("open-repo-list", ui::gutter().child(list))),
                )
        } else {
            div()
                .w_full()
                .px(px(OPEN_LIST_INSET))
                .pb(px(OPEN_LIST_INSET))
                .child(list)
        };
        body = body
            .child(crate::root::horizontal_divider(theme))
            .child(
                div()
                    .w_full()
                    .h(px(OPEN_SECTION_HEIGHT))
                    .flex()
                    .items_center()
                    .child(header),
            )
            .child(list);
    }
    body.into_any_element()
}

/// The open prompt's recent list: anything in the MRU that isn't already open; one
/// click reopens it. `None` when there is nothing to list.
fn recents_section(
    ui: &Diffui,
    theme: ThemeSpec,
    cx: &mut Cx,
) -> Option<(AnyElement, AnyElement, usize)> {
    let open_roots: Vec<&str> = ui
        .tabs
        .iter()
        .filter_map(|tab| tab.root()?.to_str())
        .collect();
    let recents: Vec<&String> = ui
        .recent_repos
        .iter()
        .filter(|root| !open_roots.contains(&root.as_str()))
        .collect();
    if recents.is_empty() {
        return None;
    }
    let font = ui.config.ui_font;
    let clear = ui::button(
        "open-repo-clear",
        ui::Variant::Ghost,
        ui::Size::Compact,
        theme,
        cx,
    )
    .h(px(OPEN_SECTION_PILL))
    .px(px(OPEN_SECTION_PILL_PAD_X))
    .child(typography::centered(
        typography::line("Clear", font, text_size::CAPTION, theme.subtle_text),
        font,
        text_size::CAPTION,
    ))
    .on_click(Diffui::on_action(Action::ClearRecentRepos, cx));
    let title_font = emphasis_font(font, Weight::MEDIUM);
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .pl(px(OPEN_CONTENT_X))
        // "Clear"'s label, not its wash, ends on the content edge.
        .pr(px(OPEN_CONTENT_X - OPEN_SECTION_PILL_PAD_X))
        .child(typography::centered(
            typography::line("Recent", title_font, text_size::CAPTION, theme.subtle_text),
            title_font,
            text_size::CAPTION,
        ))
        .child(div().flex_1())
        .child(clear);

    let rows = recents.len();
    let mut list = div().flex().flex_col().w_full();
    for (index, root) in recents.into_iter().enumerate() {
        list = list.child(recent_repo_row(ui, theme, root, index, rows, cx));
    }
    Some((header.into_any_element(), list.into_any_element(), rows))
}

/// The folders under the typed path, headed by the folder being browsed. `None`
/// when nothing matches (or the folder is still being read).
fn folder_section(
    ui: &Diffui,
    theme: ThemeSpec,
    dialog: &crate::OpenRepoDialog,
    query: crate::path_browse::Query,
    rows: Vec<&crate::path_browse::Entry>,
    cx: &mut Cx,
) -> Option<(AnyElement, AnyElement, usize)> {
    if rows.is_empty() {
        return None;
    }
    let mono = ui.config.mono_font;
    let width = OPEN_DIALOG_WIDTH - 2.0 * OPEN_CONTENT_X;
    let header = div()
        .w_full()
        .px(px(OPEN_CONTENT_X))
        .child(typography::centered(
            typography::line(
                middle_cut(&query.typed_dir, width, text_size::CAPTION, mono),
                mono,
                text_size::CAPTION,
                theme.subtle_text,
            ),
            mono,
            text_size::CAPTION,
        ));

    let count = rows.len();
    let mut list = div().flex().flex_col().w_full();
    for (index, entry) in rows.into_iter().enumerate() {
        list = list.child(folder_row(
            ui,
            theme,
            entry,
            index,
            dialog.selected == Some(index),
            cx,
        ));
    }
    Some((header.into_any_element(), list.into_any_element(), count))
}

/// The folder glyph that leads a row of the open prompt's list.
fn row_glyph(theme: ThemeSpec) -> Aligned {
    Aligned::glyph(
        div()
            .w(px(OPEN_GLYPH_BOX))
            .flex()
            .justify_center()
            .child(icons::icon(icons::FOLDER, ui::ICON_SIZE, theme.subtle_text)),
        ui::ICON_SIZE,
    )
}

/// A row of the open prompt's list: full width, its wash on hover or `selected`.
fn list_row(
    id: gpui_kit::ElementId,
    content: gpui_kit::Div,
    selected: bool,
    theme: ThemeSpec,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(OPEN_ROW_HEIGHT))
        .px(px(OPEN_CONTENT_X - OPEN_LIST_INSET))
        .rounded(px(radius::CONTROL))
        .cursor_pointer()
        .when(selected, |row| row.bg(theme.selected_file))
        .hover(|row| row.bg(theme.selected_file))
        .child(content)
}

/// One folder suggestion: glyph, name, and a jj/git chip when the folder is a
/// repository (which a click opens rather than browses into).
fn folder_row(
    ui: &Diffui,
    theme: ThemeSpec,
    entry: &crate::path_browse::Entry,
    index: usize,
    selected: bool,
    cx: &mut Cx,
) -> impl IntoElement {
    let font = ui.config.ui_font;
    let mut content = vec![
        row_glyph(theme),
        Aligned::gap(OPEN_GLYPH_GAP),
        Aligned::text(
            typography::line(entry.name.clone(), font, text_size::BODY, theme.text).text_ellipsis(),
            font,
            text_size::BODY,
        )
        .fill(),
    ];
    if let Some(vcs) = entry.vcs {
        content.push(vcs_badge(vcs, theme, ui.config.mono_font));
    }
    list_row(
        ("open-repo-folder-row", index).into(),
        typography::baseline_row(content, 0.0).w_full(),
        selected,
        theme,
    )
    .on_click(Diffui::on_action(Action::OpenRepoPick(index), cx))
}

/// One line of the open prompt's recent list, laid out like a palette row: glyph,
/// `owner/name`, the home-contracted path, with a remove `×` that floats over the
/// row's right end.
fn recent_repo_row(
    ui: &Diffui,
    theme: ThemeSpec,
    root: &str,
    index: usize,
    rows: usize,
    cx: &mut Cx,
) -> impl IntoElement {
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let (owner, name) = crate::repo_label(std::path::Path::new(root));
    // The name (sans) and the path (smaller mono) share a baseline; the glyph
    // centres on the caps.
    let name_font = emphasis_font(font, Weight::MEDIUM);
    let mut label = Vec::new();
    let mut label_width = measure::line_width(&name, text_size::BODY, name_font);
    if !owner.is_empty() {
        let owner = format!("{owner}/");
        label_width += measure::line_width(&owner, text_size::BODY, font);
        label.push(Aligned::text(
            typography::line(owner, font, text_size::BODY, theme.subtle_text),
            font,
            text_size::BODY,
        ));
    }
    label.push(Aligned::text(
        typography::line(name, name_font, text_size::BODY, theme.text),
        name_font,
        text_size::BODY,
    ));
    let gutter = if rows > OPEN_LIST_ROWS {
        crate::scrollbar::GUTTER
    } else {
        OPEN_LIST_INSET
    };
    let path_width = (OPEN_DIALOG_WIDTH
        - OPEN_LIST_INSET
        - gutter
        - 2.0 * (OPEN_CONTENT_X - OPEN_LIST_INSET)
        - OPEN_GLYPH_BOX
        - OPEN_GLYPH_GAP
        - label_width
        - theme::space::MD
        - OPEN_REMOVE_SIDE)
        .max(0.0);
    let content = typography::baseline_row(
        [
            row_glyph(theme),
            Aligned::gap(OPEN_GLYPH_GAP),
            typography::group(label, 0.0),
            Aligned::gap(theme::space::MD),
            // Middle-ellipsized: a path's ends (where it lives, what it's called)
            // say more than its middle.
            Aligned::text(
                typography::line(
                    middle_cut(
                        &crate::contract_user_path(root),
                        path_width,
                        text_size::CAPTION,
                        mono,
                    ),
                    mono,
                    text_size::CAPTION,
                    theme.subtle_text,
                ),
                mono,
                text_size::CAPTION,
            )
            .fill(),
            // Keeps the path clear of the floating `×`.
            Aligned::gap(OPEN_REMOVE_SIDE),
        ],
        0.0,
    )
    .w_full();

    let row = list_row(("open-repo-recent", index).into(), content, false, theme)
        .on_click(Diffui::on_action(Action::OpenRepo(root.to_owned()), cx));
    // Inset from the row's end as far as from its top and bottom.
    let remove_inset = (OPEN_ROW_HEIGHT - OPEN_REMOVE_SIDE) / 2.0;
    let remove = ui::button(
        ("open-repo-remove", index),
        ui::Variant::Ghost,
        ui::Size::Compact,
        theme,
        cx,
    )
    .size(px(OPEN_REMOVE_SIDE))
    .p(px(0.0))
    .rounded(px(radius::CONTROL))
    .child(icons::icon(icons::CLOSE, 13.0, theme.subtle_text))
    .on_click(Diffui::on_action(
        Action::RemoveRecentRepo(root.to_owned()),
        cx,
    ));
    div().relative().w_full().child(row).child(
        div()
            .absolute()
            .top(px(remove_inset))
            .right(px(remove_inset))
            .child(remove),
    )
}

/// `text` cut out of its middle to fit `width` on one line. GPUI can end a line
/// with an ellipsis or start one, not put it in between.
fn middle_cut(text: &str, width: f32, size: f32, font: Font) -> String {
    if measure::line_width(text, size, font) <= width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let cut = |keep: usize| -> String {
        let tail = keep / 2;
        let head = keep - tail;
        chars[..head]
            .iter()
            .chain(std::iter::once(&'\u{2026}'))
            .chain(&chars[chars.len() - tail..])
            .collect()
    };
    let (mut fits, mut over) = (0, chars.len());
    while over - fits > 1 {
        let keep = (fits + over) / 2;
        if measure::line_width(&cut(keep), size, font) <= width {
            fits = keep;
        } else {
            over = keep;
        }
    }
    cut(fits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where tab `index`'s slot starts in a strip that begins at 0.
    fn slot(widths: &[f32], index: usize) -> f32 {
        widths[..index].iter().map(|width| width + TAB_GAP).sum()
    }

    #[test]
    fn a_tab_at_rest_belongs_where_it_is() {
        let widths = [120.0, 40.0, 90.0];
        for index in 0..widths.len() {
            let at_rest = slot(&widths, index);
            assert_eq!(drop_index(&widths, index, at_rest, at_rest), index);
        }
    }

    #[test]
    fn a_tab_swaps_once_its_leading_edge_clears_the_neighbors_center() {
        let widths = [100.0, 60.0, 80.0];
        // The neighbor spans 103..163, so its center is 133; the dragged tab's
        // right edge is `x + 100`.
        let swap_at = 133.0 + SWAP_HYSTERESIS - 100.0;
        assert_eq!(drop_index(&widths, 0, 0.0, swap_at), 0);
        assert_eq!(drop_index(&widths, 0, 0.0, swap_at + 1.0), 1);
    }

    /// The reason for the hysteresis: right after a swap the dragged tab must
    /// not already be on the far side of the way back, or a resting hand flips
    /// the pair on every tremor.
    #[test]
    fn a_swap_holds_until_the_tab_retreats_past_the_way_back() {
        // The strip after the swap above, the dragged tab now second.
        let widths = [60.0, 100.0, 80.0];
        let dragged_slot = slot(&widths, 1);
        let swapped_at = 133.0 + SWAP_HYSTERESIS - 100.0 + 1.0;
        assert_eq!(drop_index(&widths, 1, dragged_slot, swapped_at), 1);

        // The neighbor now spans 0..60: back across its center, less the margin.
        let back_at = 30.0 - SWAP_HYSTERESIS;
        assert_eq!(drop_index(&widths, 1, dragged_slot, back_at), 1);
        assert_eq!(drop_index(&widths, 1, dragged_slot, back_at - 1.0), 0);
        assert!(swapped_at - back_at > 2.0 * SWAP_HYSTERESIS);
    }

    #[test]
    fn a_fast_drag_crosses_several_tabs_in_one_step() {
        let widths = [50.0; 4];
        assert_eq!(drop_index(&widths, 0, 0.0, slot(&widths, 3)), 3);
        assert_eq!(drop_index(&widths, 3, slot(&widths, 3), 0.0), 0);
        assert_eq!(
            drop_index(&widths, 1, slot(&widths, 1), slot(&widths, 2) + 1.0),
            2
        );
    }

    /// The dragged tab is held inside the run of tabs, so both end slots have
    /// to be reachable from inside it — even for a wide tab next to a narrow one.
    #[test]
    fn both_ends_of_the_strip_are_reachable_within_the_clamp() {
        let widths = [200.0, 40.0, 40.0];
        let run: f32 = widths.iter().sum::<f32>() + 2.0 * TAB_GAP;
        assert_eq!(drop_index(&widths, 0, 0.0, run - 200.0), 2);

        let widths = [40.0, 40.0, 200.0];
        assert_eq!(drop_index(&widths, 2, slot(&widths, 2), 0.0), 0);
    }
}

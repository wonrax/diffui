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
use crate::root::PaneSet;
use crate::theme::{
    self, Color, Font, ThemeSpec, Weight, chip_background, chrome_fill, emphasis_font, radius,
    text_size,
};
use crate::typography::{self, Aligned};
use crate::ui::{Tip as _, Tone};
use crate::{Action, Cx, Diffui, HoverTarget, TabId, icons, ui};

/// Focus target id for the open-repository dialog's path field.
pub const OPEN_REPO_INPUT_ID: &str = "open-repo-input";

const TAB_TEXT_SIZE: f32 = text_size::BODY;

/// A tab's height: a floating rounded tab, centred in the strip like the
/// strip's other controls.
const TAB_HEIGHT: f32 = 28.0;

/// Room around the tabs' row inside its clip, so the active tab's drop
/// shadow (1px down, 2px of blur) isn't cut off at the strip's bottom.
const TAB_RING_ROOM: f32 = 4.0;

/// Gap between neighboring tabs, and between the final tab and the add button.
const TAB_GAP: f32 = 2.0;

/// The search field's width when the strip has room for it.
const OMNI_WIDTH: f32 = 320.0;
/// Narrowest the search field gets before the tabs give way instead.
const OMNI_MIN_WIDTH: f32 = 160.0;

/// Label padding inside a tab's fill / hover wash.
const LABEL_PAD_X: f32 = 8.0;

/// Narrowest a squeezed tab gets: its badge, a few characters and its `×`.
const TAB_MIN_WIDTH: f32 = 96.0;

/// The close button's square, which also holds the dirty dot.
const CLOSE_SIDE: f32 = ui::CLOSE_SIDE;

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

/// The open-repo card, sized like a palette column.
const OPEN_DIALOG_WIDTH: f32 = crate::palette::COLUMN_WIDTH;
/// Height of the card's input band.
const OPEN_INPUT_BAND: f32 = crate::palette::INPUT_BAND;
/// The input band's inset: the kind square sits as far in as the rows' leads.
const OPEN_BAND_X: f32 = OPEN_LIST_INSET + OPEN_ROW_PAD_X - (OPEN_KIND_SIDE - OPEN_LEAD_BOX) / 2.0;
/// The status line's text edge.
const OPEN_CONTENT_X: f32 = theme::space::XL;
/// Inset of the list's hover washes inside the card.
const OPEN_LIST_INSET: f32 = theme::list::INSET;
/// A row's inset from its wash to its lead.
const OPEN_ROW_PAD_X: f32 = theme::list::PAD_X;
const OPEN_KIND_SIDE: f32 = 30.0;
const OPEN_STATUS_HEIGHT: f32 = 40.0;
const OPEN_SECTION_PILL: f32 = 22.0;
const OPEN_SECTION_PILL_PAD_X: f32 = theme::space::SM;
const OPEN_ROW_HEIGHT: f32 = theme::list::height::DIALOG;
/// Rows the card's list shows before it scrolls: as many as the folder
/// suggestions can be, so typing a path and clearing it again keeps the card
/// the same size once the recents fill it.
const OPEN_LIST_ROWS: usize = 8;
/// The column a row's badge or folder glyph is centred in.
const OPEN_LEAD_BOX: f32 = 22.0;
const OPEN_GLYPH_GAP: f32 = 10.0;
const OPEN_REMOVE_SIDE: f32 = ui::CLOSE_SIDE;

/// The strip's height: the title-bar height where it stands in for one,
/// otherwise the same height on its own.
pub fn height() -> f32 {
    chrome::title_bar_height().unwrap_or(STRIP_HEIGHT)
}

/// The strip's height where it sizes itself (no title bar to stand in for).
const STRIP_HEIGHT: f32 = 38.0;

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
        .items_center()
        .gap(px(TAB_GAP))
        // Room for the active tab's ring and shadow, which the clip would cut.
        .p(px(TAB_RING_ROOM))
        .m(px(-TAB_RING_ROOM))
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
    let on_move = Diffui::listen_on(
        cx,
        PaneSet::TAB_BAR,
        |this, event: &MouseMoveEvent, _, cx| {
            if event.pressed_button == Some(MouseButton::Left) {
                this.on_tab_drag_moved(f32::from(event.position.x), cx);
            } else {
                // A release we never saw: it landed outside the app's event stream.
                this.on_tab_drag_released(cx);
            }
        },
    );
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

    // Everything centres on the strip, and so do the traffic lights where the
    // strip stands in for the title bar (see `chrome::titlebar_options`).
    let inset = chrome::leading_inset();
    let row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .px(px(8.0))
        .h_full()
        .w_full()
        .when(inset > 0.0, |row| row.child(div().w(px(inset)).flex_none()))
        // `+` stays with the tabs; the search field gives way to its floor, then
        // the tabs squeeze.
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(TAB_GAP))
                .flex_shrink(1.0)
                .min_w_0()
                .child(tabs)
                .child(add_button(theme, cx)),
        )
        .child(div().flex_1().min_w_0())
        .child(palette_hint(ui, theme, cx));

    // The window's chrome: recessed, with tabs floating on it.
    let bar = div()
        .id("tab-bar")
        .relative()
        .flex()
        .flex_col()
        .justify_center()
        .w_full()
        .h(px(height()))
        .flex_none()
        .bg(chrome_fill(theme))
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
        .tip(tab_tooltip(tab), font, theme);

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
        ui::close_button(("tab-close", element), theme.subtle_text, theme, cx)
            .flex_none()
            .on_click(Diffui::on_action(Action::CloseTab(id), cx))
            .into_any_element()
    };

    // Inactive tabs stay quiet; hovering one washes it. The label sits in the
    // same box in every state, so nothing shifts on hover or selection.
    let wash_fill = (!active && (hovered || lifted)).then(|| theme::hover_wash(theme));
    let wash = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::XS))
        .w_full()
        .h_full()
        .pl(px(LABEL_PAD_X))
        .pr(px(LABEL_PAD_X - 3.0))
        .when_some(wash_fill, |wash, fill| {
            wash.child(crate::squircle::background(
                crate::squircle::Fill::new(crate::squircle::Corners::all(radius::CONTROL))
                    .background(fill),
                0.0,
            ))
        })
        .child(label)
        .child(div().ml_auto().flex_none().child(trailing));

    let on_hover = cx.listener(move |this: &mut Diffui, entered: &bool, _, cx| {
        if *entered {
            this.hovered = Some(HoverTarget::Tab(id));
            this.notify_panes(cx, PaneSet::TAB_BAR);
        // Crossing between neighbours can report the new tab first.
        } else if this.hovered == Some(HoverTarget::Tab(id)) {
            this.hovered = None;
            this.notify_panes(cx, PaneSet::TAB_BAR);
        }
    });
    let on_press = Diffui::listen(cx, move |this, event: &gpui_kit::MouseDownEvent, _, cx| {
        this.on_tab_pressed(id, f32::from(event.position.x), cx);
        cx.stop_propagation();
    });

    // A floating rounded tab. The active one is raised: the panes' surface,
    // a hairline ring and a soft shadow.
    let body = div()
        .id(("tab", element))
        .relative()
        .left(px(offset))
        .w_full()
        .h(px(TAB_HEIGHT))
        .when(active, |body| theme::raised(body, radius::CONTROL, theme))
        // Only the active tab has a fill to hide what it passes over; a background
        // one in the air (its activation was refused) gets a stand-in.
        .when(lifted && !active, |body| {
            body.child(crate::squircle::background(
                crate::squircle::Fill::new(crate::squircle::Corners::all(radius::CONTROL))
                    .background(chrome_fill(theme)),
                0.0,
            ))
        })
        .child(div().relative().size_full().child(wash))
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

/// A tab's `jj` / `git` / `pr` badge: says what the tab holds. Neutral, and
/// in the accent on the active tab, so colour never competes with the state
/// the panes below show.
fn tab_badge(source: &crate::TabSource, active: bool, theme: ThemeSpec, mono: Font) -> Div {
    let label = match source {
        crate::TabSource::Repo { vcs: Vcs::Jj, .. } => "jj",
        crate::TabSource::Repo { vcs: Vcs::Git, .. } => "git",
        crate::TabSource::GitHubPr(_) => "pr",
    };
    let ink = if active {
        theme.accent
    } else {
        theme.muted_text
    };
    vcs_badge(label, ink, mono).view()
}

/// `jj`, `git` or `pr` in a small mono tag.
fn vcs_badge(label: &'static str, ink: Color, mono: Font) -> ui::Tag {
    let mono = emphasis_font(mono, Weight::SEMIBOLD);
    ui::tag(label, ink, ui::TagSize::Small, mono).mono(mono)
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

/// `+` button: opens the path dialog to add another repository. A square as
/// tall as a tab, so the strip's controls sit on one line.
fn add_button(theme: ThemeSpec, cx: &mut Cx) -> impl IntoElement {
    let side = TAB_HEIGHT;
    ui::button("tab-add", ui::Variant::Ghost, ui::Size::Compact, theme, cx)
        .size(px(side))
        .p(px(0.0))
        .flex_none()
        .child(icons::icon(icons::PLUS, 15.0, theme.muted_text))
        .on_click(Diffui::on_action(Action::OpenRepoDialog, cx))
}

/// The right-hand search field: opens the command palette, and says what it
/// searches and its key.
fn palette_hint(ui: &Diffui, theme: ThemeSpec, cx: &mut Cx) -> impl IntoElement {
    use gpui_kit::base::TestSupportExt as _;
    let font = ui.config.ui_font;
    let placeholder = div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(typography::line(
            "Search revisions, files, commands",
            font,
            text_size::BODY,
            theme.subtle_text,
        ));
    let keys = ui::chord(
        crate::overlay_kit::chord_keys(&chrome::cmd_label("K")),
        ui.config.ui_font,
        theme,
    );
    div()
        .id("palette-hint")
        .test_support()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::SM))
        .flex_shrink(1.0)
        .w(px(OMNI_WIDTH))
        .min_w(px(OMNI_MIN_WIDTH))
        .h(px(TAB_HEIGHT))
        .pl(px(10.0))
        .pr(px(5.0))
        .map(|field| {
            theme::squircle(
                field,
                radius::CONTROL,
                Some(theme.panel_background_elevated),
                Some(theme.border),
            )
        })
        // Its edge firms up under the pointer.
        .child(crate::squircle::hover_background(
            crate::squircle::Fill::new(crate::squircle::Corners::all(radius::CONTROL))
                .border(1.0, crate::theme::firm_ring(theme)),
            1.0,
        ))
        .cursor_pointer()
        .child(icons::icon(icons::SEARCH, 14.0, theme.subtle_text))
        .child(placeholder)
        .child(keys)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(Diffui::on_action(Action::OpenPalette, cx))
}

/// Modal confirmation for a guarded mutation (see [`crate::ConfirmDialog`]). Empty
/// when closed. Lives here because it mirrors the open-repository modal's scrim +
/// card construction exactly.
///
/// The title and a line of plain words up top; then, for a push, a row per
/// bookmark it moves and any callouts; the buttons in a footer strip.
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
    // Red when the confirm destroys or overrides something (a mutation the jj
    // CLI refuses by default); the plain call to action for a routine one.
    let (variant, glyph) = if dialog.look.routine {
        (
            ui::Variant::Primary,
            Some((icons::UPLOAD, theme.background)),
        )
    } else {
        (ui::Variant::Destructive, None)
    };
    let glyph = glyph.filter(|_| !dialog.look.moves.is_empty());
    let accept = ui::text_button(
        "confirm-accept",
        glyph,
        dialog.confirm_label.clone(),
        variant,
        ui::Size::Standard,
        font,
        theme,
        cx,
    )
    .on_click(Diffui::on_action(Action::ConfirmAccept, cx));
    let cancel = ui::cancel_button("confirm-cancel", ui::Size::Standard, font, theme, cx)
        .on_click(Diffui::on_action(Action::ConfirmCancel, cx));

    let mut card =
        crate::overlay_kit::Dialog::new(&dialog.title, font, mono, theme).lede(&dialog.body, mono);
    for (index, bookmark) in dialog.look.moves.iter().enumerate() {
        card = card.child(push_row(ui, theme, index, bookmark));
    }
    for (lead, detail) in &dialog.look.callouts {
        card = card.child(crate::overlay_kit::callout(
            lead,
            detail.as_ref(),
            font,
            mono,
            theme,
        ));
    }
    let card = card
        .hint(&["esc"], "Cancel")
        .buttons([cancel.into_any_element(), accept.into_any_element()])
        .build();
    let dismiss = Diffui::on_action::<()>(Action::ConfirmCancel, cx);
    ui::modal(
        "confirm-dialog",
        card,
        crate::toolbar::modal_top(),
        move |window, cx| dismiss(&(), window, cx),
    )
    .into_any_element()
}

/// One bookmark a push moves: its name, where the remote has it and where it
/// goes, and whether that's a fast-forward and which revisions it publishes.
fn push_row(
    ui: &Diffui,
    theme: ThemeSpec,
    index: usize,
    bookmark: &diffui_core::BookmarkMove,
) -> impl IntoElement {
    use diffui_core::MoveKind;
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let name_font = emphasis_font(font, Weight::SEMIBOLD);
    let hash = |text: &str, color: Color| {
        Aligned::text(
            typography::line(text.to_owned(), mono, text_size::BODY, color),
            mono,
            text_size::BODY,
        )
    };
    let none = "\u{2014}";
    let route = typography::baseline_row(
        [
            hash(bookmark.from.as_deref().unwrap_or(none), theme.subtle_text),
            Aligned::gap(theme::space::MD),
            Aligned::text(
                typography::line("\u{2192}", font, text_size::BODY, theme.subtle_text),
                font,
                text_size::BODY,
            ),
            Aligned::gap(theme::space::MD),
            hash(bookmark.to.as_deref().unwrap_or(none), theme.text),
        ],
        0.0,
    );
    let kind = match bookmark.kind {
        MoveKind::Add => "New on the remote",
        MoveKind::Forward => "Fast-forward",
        MoveKind::Backward => "Moves backward",
        MoveKind::Sideways => "Not a fast-forward",
        MoveKind::Delete => "Deleted on the remote",
    };
    let mut why = crate::prose::Prose::from(kind);
    if bookmark.revision_count > 0 {
        why = why.text(match bookmark.revision_count {
            1 => " \u{b7} 1 revision: ".to_owned(),
            n => format!(" \u{b7} {n} revisions: "),
        });
        for (at, id) in bookmark.revisions.iter().enumerate() {
            if at > 0 {
                why = why.text(", ");
            }
            why = why.rev(id.clone());
        }
        let more = bookmark
            .revision_count
            .saturating_sub(bookmark.revisions.len());
        if more > 0 {
            why = why.text(format!(" and {more} more"));
        }
    }
    let ink = if bookmark.kind.is_fast_forward() {
        theme.subtle_text
    } else {
        theme.note_text
    };
    div()
        .id(("confirm-push-row", index))
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(theme::space::LG))
        .p(px(theme::space::LG))
        .map(|row| theme::squircle(row, radius::MD, None, Some(theme.border)))
        .child(div().pt(px(1.0)).child(icons::icon(
            crate::overlay_kit::glyph::BOOKMARK,
            ui::ICON_SIZE,
            theme.lane_base,
        )))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(theme::space::XS))
                .child(
                    typography::line(
                        bookmark.bookmark.clone(),
                        name_font,
                        text_size::BODY,
                        theme.text,
                    )
                    .overflow_hidden()
                    .text_ellipsis(),
                )
                .child(route)
                .child(
                    div()
                        .w_full()
                        .text_size(px(text_size::BODY))
                        .child(why.view(font, mono, ink, theme)),
                ),
        )
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
/// palette-family card with one input band (what kind of thing is typed, the
/// path, a folder picker), a status line saying what Enter will do with it,
/// then the recents, the folders under the path, or the pull request it names.
pub(crate) fn open_repo_card(
    ui: &Diffui,
    theme: ThemeSpec,
    dialog: &crate::OpenRepoDialog,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let on_input = cx.listener(|this: &mut Diffui, path: &String, _, cx| {
        this.act(cx, |this, cx| this.open_repo_typed(path.clone(), cx));
    });
    let submit = Diffui::on_action::<()>(Action::SubmitRepoDialog, cx);
    let spec = input::FieldSpec::new(OPEN_REPO_INPUT_ID, &dialog.path)
        .placeholder("Path, owner/repo#123, or PR URL")
        .on_input(move |path, window, cx| on_input(&path, window, cx))
        .on_submit(move |window, cx| submit(&(), window, cx));
    // Bare: the card is the field.
    let field = input::single_line(spec, mono, window, cx)
        .appearance(false)
        // On the band's text edge, not inset from it.
        .px(px(0.0))
        .text_size(px(text_size::BODY_LG));
    let status = ui.open_repo_status();

    let mut band = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(OPEN_INPUT_BAND))
        .px(px(OPEN_BAND_X))
        .gap(px(OPEN_GLYPH_GAP))
        .text_size(px(text_size::BODY_LG))
        .child(kind_box(status.as_ref(), theme))
        .child(div().flex_1().min_w_0().child(field));
    #[cfg(target_os = "macos")]
    {
        band = band.child(
            ui::text_button(
                "open-repo-folder",
                None,
                "Browse\u{2026}",
                ui::Variant::Raised,
                ui::Size::Compact,
                font,
                theme,
                cx,
            )
            .on_click(Diffui::on_action(Action::ChooseRepoFolder, cx))
            .tip("Choose a folder in Finder", font, theme),
        );
    }

    let mut body = theme::surface(div(), theme::Elevation::Modal, theme)
        .w(px(OPEN_DIALOG_WIDTH))
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(band)
        .child(crate::root::horizontal_divider(theme));
    // A plain folder needs no telling: browsing through folders is what the
    // list under the field is for.
    let told = status
        .as_ref()
        .filter(|status| !matches!(status, crate::path_browse::Status::NotRepository { .. }));
    if let Some(status) = told {
        body = body
            .child(status_line(ui, theme, status))
            .child(crate::root::horizontal_divider(theme));
    }

    let path = dialog.path.trim();
    let list: Option<AnyElement> = match (&status, dialog.suggestions()) {
        (Some(crate::path_browse::Status::PullRequest(spec)), _) => {
            Some(pull_request_card(ui, theme, spec).into_any_element())
        }
        (_, Some((query, rows))) => {
            Some(folder_section(ui, theme, dialog, query, rows, window, cx))
        }
        _ if path.is_empty() => recents_section(ui, theme, dialog, window, cx),
        _ => None,
    };
    if let Some(list) = list {
        body = body.child(
            div()
                .w_full()
                .px(px(OPEN_LIST_INSET))
                .pb(px(OPEN_LIST_INSET))
                .child(list),
        );
    }
    body.child(open_repo_footer(ui, theme, dialog, status.as_ref()))
        .into_any_element()
}

/// A stretch of a status line.
enum Run {
    Plain(String),
    /// A name: semibold, full ink.
    Strong(String),
    /// A path or a spelling to type: mono.
    Code(String),
}

fn plain(text: impl Into<String>) -> Run {
    Run::Plain(text.into())
}

fn strong(text: impl Into<String>) -> Run {
    Run::Strong(text.into())
}

fn code(text: impl Into<String>) -> Run {
    Run::Code(text.into())
}

/// The tone, glyph, words and Enter hint of a status.
fn status_parts(
    status: &crate::path_browse::Status,
) -> (Tone, &'static str, Vec<Run>, Option<&'static str>) {
    use crate::overlay_kit::glyph;
    use crate::path_browse::Status;
    let vcs_name = |vcs: Vcs| match vcs {
        Vcs::Jj => "jj",
        Vcs::Git => "git",
    };
    match status {
        Status::PullRequest(spec) => {
            let mut runs = vec![
                plain("Pull request "),
                strong(format!("#{}", spec.number)),
                plain(" in "),
                strong(format!("{}/{}", spec.owner, spec.repo)),
            ];
            if let Some(host) = &spec.host {
                runs.extend([plain(" on "), code(host.clone())]);
            }
            (
                Tone::Accent,
                glyph::PULL_REQUEST,
                runs,
                Some("Open in a new tab"),
            )
        }
        Status::Relative => (
            Tone::Warn,
            icons::ALERT_TRIANGLE,
            vec![
                plain("Start with "),
                code("/"),
                plain(" or "),
                code("~/"),
                plain(" for a folder, or type "),
                code("owner/repo#123"),
                plain(" for a pull request."),
            ],
            None,
        ),
        Status::Repository {
            name, open: true, ..
        } => (
            Tone::Accent,
            glyph::CHECK_CIRCLE,
            vec![strong(name.clone()), plain(" is already open.")],
            Some("Switch to its tab"),
        ),
        Status::Repository {
            name,
            vcs,
            at_root: true,
            ..
        } => (
            Tone::Ok,
            glyph::CHECK_CIRCLE,
            vec![
                strong(name.clone()),
                plain(format!(" is a {} repository.", vcs_name(*vcs))),
            ],
            Some("Open"),
        ),
        Status::Repository { name, vcs, .. } => (
            Tone::Ok,
            glyph::CHECK_CIRCLE,
            vec![
                plain("Inside "),
                strong(name.clone()),
                plain(format!(" ({}). Opens the repository root.", vcs_name(*vcs))),
            ],
            Some("Open"),
        ),
        Status::NotRepository { folder } => (
            Tone::Muted,
            icons::FOLDER,
            vec![
                code(folder.clone()),
                plain(" isn't a repository. Pick a folder marked "),
                strong("jj"),
                plain(" or "),
                strong("git"),
                plain("."),
            ],
            None,
        ),
        Status::NoMatch { folder, partial } => (
            Tone::Warn,
            icons::ALERT_TRIANGLE,
            vec![
                plain("No folder in "),
                code(folder.clone()),
                plain(format!(" starts with \u{201c}{partial}\u{201d}.")),
            ],
            None,
        ),
        Status::Missing { folder } => (
            Tone::Warn,
            icons::ALERT_TRIANGLE,
            vec![code(folder.clone()), plain(" doesn't exist.")],
            None,
        ),
        Status::Failed(error) => (
            Tone::Danger,
            icons::ALERT_TRIANGLE,
            vec![plain(error.clone())],
            None,
        ),
    }
}

/// The square at the band's start: what kind of thing the input names. Green
/// once the path resolves to a repository, the accent for a pull request.
fn kind_box(status: Option<&crate::path_browse::Status>, theme: ThemeSpec) -> impl IntoElement {
    use crate::path_browse::Status;
    let (glyph, fill, ink) = match status {
        Some(Status::PullRequest(_)) => (
            crate::overlay_kit::glyph::PULL_REQUEST,
            chip_background(theme.accent),
            theme.accent,
        ),
        Some(Status::Repository { .. }) => (
            icons::FOLDER,
            chip_background(theme.added_text),
            theme.added_text,
        ),
        _ => (icons::FOLDER, theme::well_wash(theme), theme.muted_text),
    };
    div()
        .flex_none()
        .size(px(OPEN_KIND_SIDE))
        .flex()
        .items_center()
        .justify_center()
        .map(|badge| theme::squircle(badge, radius::SM, Some(fill), None))
        .child(icons::icon(glyph, ui::ICON_SIZE, ink))
}

/// The line under the input saying what Enter will do with it, updated as it's
/// typed. An open that failed for a reason only the open found says why here.
fn status_line(ui: &Diffui, theme: ThemeSpec, status: &crate::path_browse::Status) -> Div {
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let (tone, glyph, runs, enter) = status_parts(status);
    let ink = tone.ink(theme);
    let line = div()
        .w_full()
        .min_h(px(OPEN_STATUS_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::MD))
        .px(px(OPEN_CONTENT_X))
        .py(px(theme::space::SM))
        .when(tone == Tone::Warn, |line| line.bg(theme.note_background))
        .when(tone == Tone::Danger, |line| {
            line.bg(chip_background(theme.removed_text))
        });
    if let crate::path_browse::Status::Failed(error) = status {
        // jj's and the OS's reasons run long: wrap them.
        return line
            .items_start()
            .child(icons::icon(glyph, ui::ICON_SIZE, ink))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font(font.gpui())
                    .text_size(px(text_size::BODY))
                    .text_color(theme.removed_text)
                    .child(error.clone()),
            );
    }
    let strong_font = emphasis_font(font, Weight::SEMIBOLD);
    let mut words: Vec<Aligned> = vec![
        Aligned::glyph(icons::icon(glyph, ui::ICON_SIZE, ink), ui::ICON_SIZE),
        Aligned::gap(theme::space::MD),
    ];
    for run in runs {
        let code = matches!(run, Run::Code(_));
        let (text, run_font, size, color) = match run {
            Run::Plain(text) => (text, font, text_size::BODY, theme.muted_text),
            Run::Strong(text) => (text, strong_font, text_size::BODY, theme.text),
            Run::Code(text) => (text, mono, text_size::CAPTION, theme.text),
        };
        let item = Aligned::text(
            typography::line(text, run_font, size, color)
                .overflow_hidden()
                .text_ellipsis(),
            run_font,
            size,
        );
        // A long path gives way when the card runs out of room; the words
        // around it stay whole.
        words.push(if code { item.shrink() } else { item });
    }
    let mut line = line.child(typography::baseline_row(words, 0.0).flex_1().min_w_0());
    if let Some(enter) = enter {
        line = line.child(crate::overlay_kit::key_hint(
            &["\u{21b5}"],
            enter,
            font,
            theme,
        ));
    }
    line
}

/// The pull request a reference names, and what opening it does.
fn pull_request_card(ui: &Diffui, theme: ThemeSpec, spec: &diffui_core::github::PrSpec) -> Div {
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let title_font = emphasis_font(font, Weight::SEMIBOLD);
    let mut words = crate::prose::Prose::from(
        "Opens as its own tab, diffing the pull request's head against its base.",
    );
    if spec.host.is_none() {
        words = words
            .text(" A bare ")
            .code("owner/repo#123")
            .text(" takes its host from the current tab's remote, so GitHub Enterprise works too.");
    }
    let title = typography::baseline_row(
        [
            Aligned::text(
                typography::line(
                    format!("{}/{}", spec.owner, spec.repo),
                    title_font,
                    text_size::BODY_LG,
                    theme.text,
                ),
                title_font,
                text_size::BODY_LG,
            ),
            Aligned::gap(theme::space::SM),
            Aligned::text(
                typography::line(
                    format!("#{}", spec.number),
                    emphasis_font(font, Weight::MEDIUM),
                    text_size::BODY_LG,
                    theme.subtle_text,
                ),
                emphasis_font(font, Weight::MEDIUM),
                text_size::BODY_LG,
            ),
        ],
        0.0,
    );
    div()
        .mt(px(OPEN_LIST_INSET))
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(theme::space::LG))
        .px(px(theme::space::XL))
        .py(px(14.0))
        .map(|card| {
            theme::squircle(
                card,
                radius::MD,
                Some(Color {
                    a: 0.07,
                    ..theme.accent
                }),
                Some(Color {
                    a: 0.3,
                    ..theme.accent
                }),
            )
        })
        .child(div().pt(px(1.0)).child(icons::icon(
            crate::overlay_kit::glyph::PULL_REQUEST,
            18.0,
            theme.accent,
        )))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(theme::space::XS))
                .child(title)
                .child(
                    div()
                        .w_full()
                        .text_size(px(text_size::BODY))
                        .line_height(px(text_size::BODY * 1.5))
                        .child(words.view(font, mono, theme.muted_text, theme)),
                ),
        )
}

/// The prompt's footer: the keys that work in the state it's in.
fn open_repo_footer(
    ui: &Diffui,
    theme: ThemeSpec,
    dialog: &crate::OpenRepoDialog,
    status: Option<&crate::path_browse::Status>,
) -> Div {
    use crate::overlay_kit::{Density, MODAL_FOOT, footer, footer_note, key_hint};
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let path = dialog.path.trim();
    let end = || {
        div()
            .ml_auto()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(theme::space::SM))
    };
    let strip = footer(Density::Strip, MODAL_FOOT, theme);
    let bound = |commands: &[crate::commands::CommandId], label: &'static str| {
        crate::overlay_kit::bound_hint(
            ui,
            crate::commands::Context::OpenRepo,
            commands,
            label,
            theme,
        )
    };
    if path.is_empty() {
        let remove = ui
            .keymap
            .chord_for(crate::commands::Context::OpenRepo, "repo.remove-recent")
            .map(|chord| crate::overlay_kit::chord_keys(&chord.display()))
            .unwrap_or_default();
        let remove: Vec<&str> = remove.iter().map(String::as_str).collect();
        return strip
            .children(bound(
                &["repo.previous-folder", "repo.next-folder"],
                "Select",
            ))
            .child(key_hint(&["\u{21b5}"], "Open", font, theme))
            .when(!remove.is_empty(), |strip| {
                strip.child(key_hint(&remove, "Remove", font, theme))
            })
            .child(
                end()
                    .child(footer_note("Type", font, theme))
                    .child(ui::keycap("/", font, theme).view())
                    .child(footer_note("or", font, theme))
                    .child(ui::keycap("~", font, theme).view())
                    .child(footer_note("to browse, or", font, theme))
                    .child(footer_note("owner/repo#123", mono, theme)),
            );
    }
    let browsing = crate::path_browse::Query::parse(path).is_some()
        && !matches!(status, Some(crate::path_browse::Status::PullRequest(_)));
    if browsing {
        return strip
            .children(bound(
                &["repo.previous-folder", "repo.next-folder"],
                "Select",
            ))
            .children(bound(&["repo.complete-folder"], "Complete"))
            .child(key_hint(&["\u{21b5}"], "Open", font, theme))
            .child(key_hint(&["\u{232b}"], "Up a folder", font, theme))
            .child(
                end()
                    .child(footer_note("Hidden folders: type", font, theme))
                    .child(ui::keycap(".", font, theme).view()),
            );
    }
    strip
        .child(key_hint(&["\u{21b5}"], "Open", font, theme))
        .child(end().children(bound(&["repo.close-dialog"], "Close")))
}

/// A list's group heading with an action at its far end, both on the rows'
/// text edges.
fn section_head(label: &'static str, font: Font, theme: ThemeSpec) -> Div {
    crate::overlay_kit::group_heading(label, font, theme).px(px(OPEN_ROW_PAD_X))
}

/// The open prompt's recent list: the repositories opened before, the open
/// ones marked (a pick switches to their tab). `None` when there is nothing to
/// list.
fn recents_section(
    ui: &Diffui,
    theme: ThemeSpec,
    dialog: &crate::OpenRepoDialog,
    window: &mut Window,
    cx: &mut Cx,
) -> Option<AnyElement> {
    let recents = ui.open_repo_recents();
    let font = ui.config.ui_font;
    if recents.is_empty() {
        return Some(
            div()
                .w_full()
                .pl(px(OPEN_ROW_PAD_X + OPEN_LEAD_BOX + OPEN_GLYPH_GAP))
                .pr(px(OPEN_ROW_PAD_X))
                .py(px(theme::space::XL))
                .child(typography::line(
                    "Nothing opened yet. Type a path to browse folders.",
                    font,
                    text_size::BODY,
                    theme.subtle_text,
                ))
                .into_any_element(),
        );
    }
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
    let header = div()
        .w_full()
        .flex()
        .flex_row()
        .items_end()
        .child(section_head("Recent", font, theme).flex_1())
        // "Clear"'s label, not its wash, ends on the rows' end.
        .child(
            div()
                .pb(px(theme::space::XXS))
                .pr(px(OPEN_ROW_PAD_X - OPEN_SECTION_PILL_PAD_X))
                .child(clear),
        );

    let rows = recents.len();
    let mut list = div().flex().flex_col().w_full();
    for (index, recent) in recents.iter().enumerate() {
        list = list.child(recent_repo_row(
            ui,
            theme,
            recent,
            index,
            dialog.selected == Some(index),
            cx,
        ));
    }
    Some(
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(header)
            .child(scrolling(
                list,
                rows,
                "open-repo-list",
                dialog.selected,
                window,
                cx,
            ))
            .into_any_element(),
    )
}

/// `list`, scrolling past [`OPEN_LIST_ROWS`] rows so the card keeps its size,
/// and moved to the row the arrow keys land on. Scrolling, it reaches out
/// through the dialog's list inset and takes it back inside, so the bar runs
/// along the card's edge, clear of the rows' text.
fn scrolling(
    list: Div,
    rows: usize,
    id: &'static str,
    selected: Option<usize>,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if rows <= OPEN_LIST_ROWS {
        return list.into_any_element();
    }
    let handle = ui::scroll_handle(id, window, cx).read(cx).clone();
    ui::reveal_on_change(
        gpui_kit::ElementId::Name(format!("{id}-revealed").into()),
        &handle,
        selected,
        |row| {
            let top = row as f32 * OPEN_ROW_HEIGHT;
            (top, top + OPEN_ROW_HEIGHT)
        },
        window,
        cx,
    );
    let inset = theme::Padding {
        top: 0.0,
        right: OPEN_LIST_INSET,
        bottom: OPEN_LIST_INSET,
        left: OPEN_LIST_INSET,
    };
    div()
        .h(px(OPEN_LIST_ROWS as f32 * OPEN_ROW_HEIGHT + OPEN_LIST_INSET))
        .mx(px(-OPEN_LIST_INSET))
        .mb(px(-OPEN_LIST_INSET))
        .child(ui::scroll_list(
            id,
            &handle,
            ui::ScrollList {
                inset,
                ..Default::default()
            },
            list,
        ))
        .into_any_element()
}

/// The folders under the typed path: the folder as breadcrumbs (a segment
/// lists it), how many folders and repositories match, then the matches.
fn folder_section(
    ui: &Diffui,
    theme: ThemeSpec,
    dialog: &crate::OpenRepoDialog,
    query: crate::path_browse::Query,
    rows: Vec<&crate::path_browse::Entry>,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let listing = dialog.listing.as_ref();
    let (folders, repos) = listing.map_or((0, 0), |listing| query.count(listing));

    let mut crumbs = div().flex().flex_row().items_center().min_w_0();
    for (index, (segment, target)) in query.crumbs().into_iter().enumerate() {
        let root = segment == "/";
        crumbs = crumbs.child(
            div()
                .id(("open-repo-crumb", index))
                .flex_none()
                .px(px(3.0))
                .rounded(px(radius::XS))
                .cursor_pointer()
                .hover(|crumb| crumb.bg(theme::hover_wash(theme)))
                .child(typography::line(
                    segment,
                    mono,
                    text_size::CAPTION,
                    theme.muted_text,
                ))
                .on_click(Diffui::listen(cx, move |this, _, _, cx| {
                    this.browse_open_repo_path(target.clone(), cx);
                })),
        );
        if !root {
            crumbs = crumbs.child(typography::line(
                "/",
                mono,
                text_size::CAPTION,
                theme.subtle_text,
            ));
        }
    }
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut count = plural(folders, "folder", "folders");
    if repos > 0 {
        count.push_str(" \u{b7} ");
        count.push_str(&plural(repos, "repo", "repos"));
    }
    let header = div()
        .w_full()
        .h(px(crate::overlay_kit::GROUP_HEIGHT))
        .flex()
        .flex_row()
        .items_end()
        .pb(px(theme::space::XS))
        .pl(px(OPEN_ROW_PAD_X - 3.0))
        .pr(px(OPEN_ROW_PAD_X))
        .gap(px(theme::space::MD))
        .child(crumbs.flex_1())
        .child(typography::line(
            count,
            font,
            text_size::CAPTION,
            theme.subtle_text,
        ));

    let shown = rows.len();
    let mut list = div().flex().flex_col().w_full();
    for (index, entry) in rows.into_iter().enumerate() {
        list = list.child(folder_row(
            ui,
            theme,
            &query,
            entry,
            index,
            dialog.selected == Some(index),
            cx,
        ));
    }
    let note = |text: String| {
        div()
            .w_full()
            .pl(px(OPEN_ROW_PAD_X + OPEN_LEAD_BOX + OPEN_GLYPH_GAP))
            .pr(px(OPEN_ROW_PAD_X))
            .py(px(theme::space::SM))
            .child(typography::line(
                text,
                font,
                text_size::BODY,
                theme.subtle_text,
            ))
    };
    let list = scrolling(
        list,
        shown,
        "open-repo-folder-list",
        dialog.selected,
        window,
        cx,
    );
    let mut section = div().w_full().flex().flex_col().child(header).child(list);
    if listing.is_some_and(|listing| listing.entries.is_empty()) {
        section = section.child(note("This folder has no subfolders.".to_owned()));
    }
    section.into_any_element()
}

/// A row's lead: the jj/git badge of a repository, or a folder.
fn row_lead(vcs: Option<Vcs>, theme: ThemeSpec, mono: Font) -> Aligned {
    match vcs {
        Some(vcs) => {
            let badge = match vcs {
                Vcs::Jj => vcs_badge("jj", theme.accent, mono),
                Vcs::Git => vcs_badge("git", theme.subtle_text, mono).fill(theme::well_wash(theme)),
            };
            badge.aligned().map(|badge| {
                div()
                    .w(px(OPEN_LEAD_BOX))
                    .flex()
                    .justify_center()
                    .child(badge)
                    .into_any_element()
            })
        }
        None => Aligned::glyph(
            div()
                .w(px(OPEN_LEAD_BOX))
                .flex()
                .justify_center()
                .child(icons::icon(icons::FOLDER, ui::ICON_SIZE, theme.subtle_text)),
            ui::ICON_SIZE,
        ),
    }
}

/// A row of the open prompt's list: full width, its wash on hover or `selected`.
fn list_row(
    id: gpui_kit::ElementId,
    content: impl IntoElement,
    selected: bool,
    theme: ThemeSpec,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    ui::list_row(
        ui::ListRow::new(OPEN_ROW_HEIGHT, OPEN_ROW_PAD_X),
        selected,
        theme,
    )
    .id(id)
    .gap(px(theme::space::MD))
    .child(content)
}

/// The "Open" chip of a repository a tab has open.
fn open_chip(font: Font, theme: ThemeSpec) -> impl IntoElement {
    let font = emphasis_font(font, Weight::MEDIUM);
    ui::tag("Open", theme.accent, ui::TagSize::Regular, font).view()
}

/// What a row's pick does, said on the highlighted row.
fn row_hint(hint: &'static str, font: Font, theme: ThemeSpec) -> impl IntoElement {
    crate::overlay_kit::footer_note(hint, font, theme)
}

/// One folder suggestion: badge or folder glyph, the name with what's typed
/// of it picked out, and on the highlighted row what a pick does (a
/// repository opens, a folder is browsed into).
fn folder_row(
    ui: &Diffui,
    theme: ThemeSpec,
    query: &crate::path_browse::Query,
    entry: &crate::path_browse::Entry,
    index: usize,
    selected: bool,
    cx: &mut Cx,
) -> impl IntoElement {
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let repo = entry.vcs.is_some();
    let open = entry
        .canonical
        .as_deref()
        .is_some_and(|path| ui.tabs.iter().any(|tab| tab.root() == Some(path)));
    let (name_font, ink) = if repo {
        (emphasis_font(font, Weight::MEDIUM), theme.text)
    } else {
        (font, theme.muted_text)
    };
    // What's typed of the name is a prefix of it.
    let typed = query
        .partial
        .chars()
        .count()
        .min(entry.name.chars().count());
    let split = entry
        .name
        .char_indices()
        .nth(typed)
        .map_or(entry.name.len(), |(at, _)| at);
    let (head, tail) = entry.name.split_at(split);
    let spans = [
        crate::ellipsis::Span::new(head)
            .font(emphasis_font(font, Weight::SEMIBOLD))
            .color(theme.accent),
        crate::ellipsis::Span::new(tail),
    ];
    let name = Aligned::text(
        crate::overlay_kit::rich_line(name_font, text_size::BODY, ink)
            .overflow_hidden()
            .text_ellipsis()
            .child(crate::ellipsis::rich_text(&spans, name_font, ink)),
        name_font,
        text_size::BODY,
    )
    .fill();
    let text = typography::baseline_row(
        [
            row_lead(entry.vcs, theme, mono),
            Aligned::gap(OPEN_GLYPH_GAP),
            name,
        ],
        0.0,
    )
    .flex_1()
    .min_w_0();
    let (hint, key) = match (repo, open) {
        (true, true) => ("switch to tab", "\u{21b5}"),
        (true, false) => ("open", "\u{21b5}"),
        (false, _) => ("go in", "\u{21e5}"),
    };
    let end = div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(theme::space::MD))
        .when(open, |end| end.child(open_chip(font, theme)))
        .when(selected, |end| {
            end.child(row_hint(hint, font, theme))
                .child(ui::keycap(key, font, theme).view())
        });
    list_row(
        ("open-repo-folder-row", index).into(),
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(theme::space::MD))
            .child(text)
            .child(end),
        selected,
        theme,
    )
    .on_click(Diffui::on_action(Action::OpenRepoPick(index), cx))
}

/// One line of the open prompt's recent list: badge, name, the folder it's in,
/// whether a tab has it open, and a remove ✕ on the hovered or highlighted row.
fn recent_repo_row(
    ui: &Diffui,
    theme: ThemeSpec,
    recent: &crate::path_browse::Recent,
    index: usize,
    selected: bool,
    cx: &mut Cx,
) -> impl IntoElement {
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let root = std::path::Path::new(&recent.root);
    let name = root.file_name().map_or_else(
        || recent.root.clone(),
        |name| name.to_string_lossy().into_owned(),
    );
    let parent = root
        .parent()
        .map(|parent| crate::contract_user_path(&parent.to_string_lossy()))
        .unwrap_or_default();
    let name_font = emphasis_font(font, Weight::MEDIUM);
    let name_width = measure::line_width(&name, text_size::BODY, name_font);
    let end_width = if recent.open { 44.0 } else { 0.0 }
        + if selected { 100.0 } else { 0.0 }
        + OPEN_REMOVE_SIDE;
    let where_width = (OPEN_DIALOG_WIDTH
        - 2.0 * OPEN_LIST_INSET
        - 2.0 * OPEN_ROW_PAD_X
        - OPEN_LEAD_BOX
        - OPEN_GLYPH_GAP
        - name_width
        - theme::space::MD
        - end_width)
        .max(0.0);
    let text = typography::baseline_row(
        [
            row_lead(recent.vcs, theme, mono),
            Aligned::gap(OPEN_GLYPH_GAP),
            // A name too long for the row gives up the path first, then
            // ellipsises.
            Aligned::text(
                typography::line(name, name_font, text_size::BODY, theme.text).text_ellipsis(),
                name_font,
                text_size::BODY,
            )
            .shrink(),
            Aligned::gap(theme::space::MD),
            // Middle-ellipsized: a path's ends (where it lives, what it's called)
            // say more than its middle.
            Aligned::text(
                typography::line(
                    crate::ellipsis::cut_middle(&parent, where_width, text_size::CAPTION, mono)
                        .into_owned(),
                    mono,
                    text_size::CAPTION,
                    theme.subtle_text,
                ),
                mono,
                text_size::CAPTION,
            )
            .fill(),
        ],
        0.0,
    )
    .flex_1()
    .min_w_0();
    let group: gpui_kit::SharedString = format!("open-repo-recent-{index}").into();
    let remove =
        ui::close_button(("open-repo-remove", index), theme.subtle_text, theme, cx).on_click(
            Diffui::on_action(Action::RemoveRecentRepo(recent.root.clone()), cx),
        );
    let end = div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(theme::space::MD))
        .when(recent.open, |end| end.child(open_chip(font, theme)))
        .when(selected, |end| {
            end.child(row_hint(
                if recent.open { "switch to tab" } else { "open" },
                font,
                theme,
            ))
        })
        .child(
            div()
                .when(!selected, |slot| {
                    slot.opacity(0.0)
                        .group_hover(group.clone(), |slot| slot.opacity(1.0))
                })
                .child(remove),
        );
    list_row(
        ("open-repo-recent", index).into(),
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap(px(theme::space::MD))
            .child(text)
            .child(end),
        selected,
        theme,
    )
    .group(group)
    .on_click(Diffui::on_action(Action::OpenRepo(recent.root.clone()), cx))
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
        // The neighbor starts a gap after the dragged tab, so its center is
        // `100 + TAB_GAP + 30`; the dragged tab's right edge is `x + 100`.
        let swap_at = 100.0 + TAB_GAP + 30.0 + SWAP_HYSTERESIS - 100.0;
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
        let swapped_at = 100.0 + TAB_GAP + 30.0 + SWAP_HYSTERESIS - 100.0 + 1.0;
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

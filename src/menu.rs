//! Cross-platform popup-menu overlay: the rendering + interaction for the
//! toolbar dropdowns (fetch branches / revset presets) and the revision
//! right-click context menu. macOS uses a real `NSMenu` (see [`crate::macos_native`]);
//! every other platform — and any future one — uses this.
//!
//! A menu is a tree of [`MenuEntry`]. The open menu lives on [`crate::Diffui`]
//! as `menu: Option<OverlayMenu>`, holding both the tree and the live
//! interaction state (which flyout is open, what's highlighted, the
//! anchor/glow rects). The renderer walks the open submenu path, placing one
//! card per level at absolute window coordinates so flyouts sit beside their
//! parent row.
//!
//! Interaction is click-to-open, click-to-pick: the press that opens the menu
//! keeps it open, and a *later* press-release on a row picks it.
//! A mouse-up listener fires on any mouse-up over a row regardless of where the
//! press began, so the opening press's own release is swallowed
//! ([`OverlayMenu::opening_release`]) — without that, a right-click near the
//! window's bottom edge would open the menu and run the row it landed on in the
//! same gesture. Drag-over-and-release from the opening press therefore does not
//! pick; every release after it does.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, BoxShadow, ElementId, Hsla, MouseButton, MouseMoveEvent, MouseUpEvent, ScrollEvent,
    ScrollHandle, SharedString, Window, canvas, div, point, px,
};

use crate::commands::{CommandArg, CommandId};
use crate::geometry::{Point, Rectangle, Size};
use crate::measure;
use crate::theme::{self, Color, ThemeSpec, emphasis_font, text_size};
use crate::typography::{self, Aligned};
use crate::{Cx, Diffui, icons};

// ── Card geometry ───────────────────────────────────────────────────────────
const MENU_MIN_WIDTH: f32 = 200.0;
const MENU_MAX_WIDTH: f32 = 460.0;
const MENU_CARD_PAD: f32 = 4.0;
const MENU_ITEM_PAD_X: f32 = 8.0;
/// Row text runs one step above the chrome default so menus read as content,
/// not controls. Width measurement ([`card_width`]) keys off the same sizes.
const MENU_LABEL_SIZE: f32 = text_size::BODY;
const MENU_DETAIL_SIZE: f32 = text_size::UI;
/// Sub-pixel cushion so a row sized to exactly its measured width doesn't
/// ellipsize from float rounding.
const MENU_TEXT_SLACK: f32 = 2.0;
/// Minimum gap between an item's label and its right-aligned detail/chevron.
const MENU_ROW_GAP: f32 = 16.0;
/// Fixed per-row heights — fixed so flyout positions (which key off a parent
/// row's offset down its card) are exact rather than guessed.
const MENU_ROW_HEIGHT: f32 = 28.0;
const MENU_SEP_HEIGHT: f32 = 9.0;
/// Gap between a trigger's bottom edge and the menu it drops (toolbar carets).
const MENU_ANCHOR_GAP: f32 = 4.0;
/// How far a flyout tucks under its parent card's edge, AppKit-style, so the
/// two read as one connected surface rather than two cards touching.
const MENU_FLYOUT_OVERLAP: f32 = 4.0;
/// Hover highlight rounding. Strict concentricity with the card's
/// [`theme::radius::SURFACE`] would be `SURFACE - border - MENU_CARD_PAD`,
/// but that small a radius reads sharper than the card's; a step up looks
/// matched.
const MENU_ROW_RADIUS: f32 = theme::radius::BUTTON;
/// Radians/second for the right-click glow pulse (~1.2 s period).
const GLOW_PULSE_SPEED: f32 = 5.0;

// ── Model ────────────────────────────────────────────────────────────────────

/// One node of a popup menu.
#[derive(Debug, Clone)]
pub(crate) enum MenuEntry {
    /// A selectable leaf naming a registry command and the argument to run it
    /// with. `detail` is right-aligned mono (a revset expression, a push
    /// remote, the command's chord); `emphasized` bumps the label weight (the
    /// fetch menu's header row).
    Item {
        label: String,
        detail: Option<String>,
        emphasized: bool,
        command: CommandId,
        arg: CommandArg,
    },
    /// A parent row whose children open in a flyout beside it.
    Submenu {
        label: String,
        items: Vec<MenuEntry>,
    },
    /// A greyed, non-interactive row (e.g. an otherwise-empty submenu), and
    /// why, when there is something to say.
    Disabled {
        label: String,
        reason: Option<String>,
    },
    Separator,
}

impl MenuEntry {
    fn height(&self) -> f32 {
        match self {
            MenuEntry::Separator => MENU_SEP_HEIGHT,
            _ => MENU_ROW_HEIGHT,
        }
    }
}

/// Where a menu opens from. Only constructed by the non-macOS overlay menu
/// (macOS uses a native `NSMenu`); the enum and the `anchor_origin` match are
/// still compiled on both platforms, so suppress the macOS "never constructed"
/// lint rather than cfg-gate the whole overlay subsystem out.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) enum AnchorSpec {
    /// Drop edge-to-edge below this trigger rect, left-aligned (toolbar carets).
    Below(Rectangle),
    /// Open at this point (a right-click cursor).
    At(Point),
}

/// The open popup menu: its tree plus live interaction state.
#[derive(Debug, Clone)]
pub(crate) struct OverlayMenu {
    pub root: Vec<MenuEntry>,
    pub anchor: AnchorSpec,
    /// Indices of the currently-open submenu chain (one per open flyout level).
    pub open_path: Vec<usize>,
    /// Path of the currently highlighted row, if any.
    pub highlight: Option<Vec<usize>>,
    /// The button whose press opened the menu, until that press's release has
    /// been seen — see [`Self::opening_release`]. `None` once it has, or when
    /// the menu was opened by something other than a mouse press.
    pub opened_by: Option<gpui_kit::MouseButton>,
    /// Whether the cursor has entered the menu at least once — lets a
    /// drag-out-and-release dismiss while a click-in-place stays open.
    pub entered: bool,
    /// Latest cursor position (window coords), tracked while the menu is open —
    /// drawn by the debug overlay (the guard itself tests the move position).
    pub cursor: Option<Point>,
    /// Apex of the trajectory triangle: the cursor time-smoothed by [`ease_apex`]
    /// on the menu tick. It trails the pointer along the open branch (so it sits
    /// where the sweep is coming *from*, left-edge starts included), catches up
    /// when the cursor idles, and freezes the moment a crossed row goes pending —
    /// keeping the apex upstream so the wedge has room during a sweep, instead of
    /// racing the cursor and dropping it the moment it edges past.
    pub flyout_origin: Option<Point>,
    /// An off-branch row the cursor is over but hasn't switched to yet, because
    /// the cursor is sweeping toward the open flyout. Switched to the moment the
    /// pointer steers out of the triangle (veers off the flyout), or once the
    /// sweep stalls (see [`OverlayMenu::sweep_stalled`]).
    pub pending_row: Option<Vec<usize>>,
    /// Cursor speed in px/s, exponentially smoothed over move events. What the
    /// guard consults: a *slow* pointer entering an off-branch row is aiming at
    /// that row, not the flyout, so the sweep hold doesn't arm at all.
    pub speed: f32,
    /// When the last cursor move arrived — anchors the speed math's Δt.
    pub last_move_at: Option<Instant>,
    /// Last instant the smoothed speed was at or above [`GUARD_SLOW_SPEED`].
    /// While a row is pending, this falling [`GUARD_STALL_MS`] behind means the
    /// sweep has stalled — the pointer is idle or crawling — and the row under
    /// it wins. Only moves refresh it, so a fully idle cursor stalls too.
    pub last_fast_at: Option<Instant>,
    /// Row to pulse-highlight while open (the right-clicked revision).
    pub glow: Option<Rectangle>,
    /// When the menu opened — drives the glow pulse phase.
    pub opened_at: Instant,
    /// Scroll offset of each open card, indexed by depth (root = 0). Fed by
    /// the cards' `on_scroll`; read by [`card_rects`] so a flyout anchors to
    /// its parent row *where it currently sits*, not where the unscrolled
    /// layout would put it. Entries past a branch switch are invalidated in
    /// [`Self::hover`] — the replacement card starts unscrolled.
    pub scrolls: Vec<f32>,
}

impl OverlayMenu {
    // Constructed only for the non-macOS overlay menu; macOS uses a native
    // `NSMenu` and never builds an `OverlayMenu`.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn new(
        root: Vec<MenuEntry>,
        anchor: AnchorSpec,
        opened_by: Option<gpui_kit::MouseButton>,
    ) -> Self {
        OverlayMenu {
            root,
            anchor,
            open_path: Vec::new(),
            highlight: None,
            opened_by,
            entered: false,
            cursor: None,
            flyout_origin: None,
            pending_row: None,
            speed: 0.0,
            last_move_at: None,
            last_fast_at: None,
            glow: None,
            opened_at: Instant::now(),
            scrolls: Vec::new(),
        }
    }

    /// Whether `button`'s release is the mouse-up of the very press that opened
    /// the menu, and consumes it — every later release answers `false`. That
    /// first release belongs to the opening gesture, not to whatever the menu
    /// drew under the cursor, so callers must let it pass without picking a row
    /// or dismissing: a row hears a release on any button-up over it without
    /// knowing where the press began, so a right-click low in the
    /// window would otherwise open the context menu and run the row that landed
    /// under the cursor — Abandon, Edit — on the way back up.
    pub(crate) fn opening_release(&mut self, button: gpui_kit::MouseButton) -> bool {
        if self.opened_by == Some(button) {
            self.opened_by = None;
            true
        } else {
            false
        }
    }

    /// Recorded scroll of the card at `depth` (0 until it reports one).
    fn card_scroll(&self, depth: usize) -> f32 {
        self.scrolls.get(depth).copied().unwrap_or(0.0)
    }

    /// Fold one cursor move into the smoothed speed. Δt is capped so the first
    /// move after an idle spell reads as the flick it is (distance over one
    /// frame-ish interval) instead of being averaged across the whole pause
    /// into a false crawl.
    pub(crate) fn note_cursor_move(&mut self, pos: Point) {
        let now = Instant::now();
        if let (Some(prev), Some(prev_at)) = (self.cursor, self.last_move_at) {
            let dt = now
                .saturating_duration_since(prev_at)
                .as_secs_f32()
                .clamp(1e-4, GUARD_SPEED_MAX_DT);
            let inst = prev.distance(pos) / dt;
            self.speed += (inst - self.speed) * (dt / GUARD_SPEED_TAU).min(1.0);
        }
        self.cursor = Some(pos);
        self.last_move_at = Some(now);
        if self.speed >= GUARD_SLOW_SPEED {
            self.last_fast_at = Some(now);
        }
    }

    /// Whether the pointer is aiming at the row under it rather than sweeping:
    /// its smoothed speed is under [`GUARD_SLOW_SPEED`]. Consulted when an
    /// off-branch hover arrives — a slow entry commits at once, no hold.
    pub(crate) fn moving_slowly(&self) -> bool {
        self.speed < GUARD_SLOW_SPEED
    }

    /// Whether a held sweep has stalled: the cursor hasn't moved fast since
    /// [`GUARD_STALL_MS`] ago (idle counts — only moves refresh the clock).
    /// The menu tick commits the pending row when this trips.
    pub(crate) fn sweep_stalled(&self) -> bool {
        self.last_fast_at
            .is_none_or(|at| at.elapsed().as_millis() as u64 >= GUARD_STALL_MS)
    }

    /// Commit a hover to `path`: highlight/open it and clear any pending sweep.
    /// The trajectory apex is tracked separately as the cursor moves.
    pub(crate) fn activate(&mut self, path: Vec<usize>) {
        self.pending_row = None;
        self.hover(path);
    }

    /// Whether `path` lies on the currently-open submenu chain (it's an
    /// ancestor of, equal to, or a descendant of the open flyout) — i.e.
    /// hovering it should *not* collapse the flyout.
    pub(crate) fn on_open_branch(&self, path: &[usize]) -> bool {
        let n = path.len().min(self.open_path.len());
        path[..n] == self.open_path[..n]
    }

    /// The entry at `path` (indices descend through submenus).
    pub(crate) fn entry_at(&self, path: &[usize]) -> Option<&MenuEntry> {
        let mut entries = &self.root;
        for (depth, &idx) in path.iter().enumerate() {
            match entries.get(idx)? {
                MenuEntry::Submenu { items, .. } if depth + 1 < path.len() => entries = items,
                entry if depth + 1 == path.len() => return Some(entry),
                _ => return None,
            }
        }
        None
    }

    /// Hovering a row: highlight it, and open its flyout if it's a submenu while
    /// keeping its ancestors open and collapsing sibling branches.
    pub(crate) fn hover(&mut self, path: Vec<usize>) {
        self.entered = true;
        let open_path = match self.entry_at(&path) {
            Some(MenuEntry::Submenu { .. }) => path.clone(),
            // Keep the ancestor chain open; drop anything deeper.
            _ => path[..path.len().saturating_sub(1)].to_vec(),
        };
        // Cards deeper than where the open chain diverges are replaced (or
        // gone); their recorded scrolls would misplace the fresh flyouts.
        let unchanged = self
            .open_path
            .iter()
            .zip(&open_path)
            .take_while(|(before, after)| before == after)
            .count();
        self.scrolls.truncate(unchanged + 1);
        self.open_path = open_path;
        self.highlight = Some(path);
    }
}

/// Whether the row at `path` should render highlighted: it's the hovered row,
/// or an open submenu parent on the active branch.
fn row_active(menu: &OverlayMenu, path: &[usize]) -> bool {
    if menu.highlight.as_deref() == Some(path) {
        return true;
    }
    // An open submenu parent: `open_path` starts with this row's path.
    path.len() <= menu.open_path.len() && menu.open_path[..path.len()] == *path
}

/// The entries of the submenu reached by `prefix` (each index steps into a
/// `Submenu`). `prefix == []` is the root.
fn entries_at<'a>(root: &'a [MenuEntry], prefix: &[usize]) -> Option<&'a [MenuEntry]> {
    let mut entries = root;
    for &idx in prefix {
        match entries.get(idx)? {
            MenuEntry::Submenu { items, .. } => entries = items,
            _ => return None,
        }
    }
    Some(entries)
}

/// Tallest a menu card may grow: the full window. Only content that doesn't
/// fit even then scrolls.
fn menu_max_height(ui: &Diffui) -> f32 {
    ui.window_size.height
}

/// On-screen rect of each open card (root + one per open flyout level), in the
/// order root → deepest. Shared by the renderer (to place cards) and the
/// hit-test / trajectory guard (to read their geometry), so the two never drift.
fn card_rects(ui: &Diffui, menu: &OverlayMenu) -> Vec<(Vec<usize>, Rectangle)> {
    let win = ui.window_size;
    let max_h = menu_max_height(ui);
    let mut out: Vec<(Vec<usize>, Rectangle)> = Vec::new();
    let mut prefix: Vec<usize> = Vec::new();
    // A right-click anchors on the bare cursor point; a toolbar caret on its
    // trigger rect, held off by [`MENU_ANCHOR_GAP`] on whichever side the card
    // ends up.
    let (mut anchor, mut gap) = match menu.anchor {
        AnchorSpec::Below(r) => (r, MENU_ANCHOR_GAP),
        AnchorSpec::At(p) => (Rectangle::new(p, Size::ZERO), 0.0),
    };

    while let Some(entries) = entries_at(&menu.root, &prefix) {
        let size = Size::new(card_width(ui, entries), card_outer_height(entries, max_h));
        let rect = card_rect(win, anchor, gap, size);
        out.push((prefix.clone(), rect));

        let level = prefix.len();
        let Some(&idx) = menu.open_path.get(level) else {
            break;
        };
        let Some(MenuEntry::Submenu { items, .. }) = entries.get(idx) else {
            break;
        };
        let flyout_w = card_width(ui, items);
        // Open to the right, tucked under the parent's edge; flip left when
        // there's no room.
        let x = if rect.x + rect.width - MENU_FLYOUT_OVERLAP + flyout_w <= win.width {
            rect.x + rect.width - MENU_FLYOUT_OVERLAP
        } else {
            (rect.x - flyout_w + MENU_FLYOUT_OVERLAP).max(0.0)
        };
        // The parent row *as drawn* — a scrolled parent card has slid its rows
        // up by its scroll offset — as a zero-height anchor, so the flyout's
        // first row lines up with it, or its last row does when it has to flip.
        let row_y = rect.y + row_top_offset(entries, idx) - menu.card_scroll(level);
        anchor = Rectangle::new(Point::new(x, row_y), Size::ZERO);
        gap = 0.0;
        prefix.push(idx);
    }
    out
}

/// Where a card of `size` lands in a `win` window: left-aligned with `anchor`
/// and dropped `gap` below it, or — when that would run off the bottom — flipped
/// so its bottom edge sits `gap` above the anchor's top. Sliding it up to fit
/// instead would drag the card over the anchor, which for a right-click means
/// putting a row under the cursor that the same click's release then runs.
fn card_rect(win: Size, anchor: Rectangle, gap: f32, size: Size) -> Rectangle {
    let below = anchor.y + anchor.height + gap;
    let above = anchor.y - gap - size.height;
    let y = if below + size.height <= win.height {
        below
    } else if above >= 0.0 {
        above
    } else {
        // Too tall for either side (a full-window menu, or an anchor with no
        // room above or below): sit on the bottom edge and let the card scroll.
        (win.height - size.height).max(0.0)
    };
    Rectangle {
        x: anchor.x.min((win.width - size.width).max(0.0)).max(0.0),
        y,
        width: size.width,
        height: size.height,
    }
}

/// The deepest open flyout's rect, if a submenu is open.
pub(crate) fn flyout_rect(ui: &Diffui, menu: &OverlayMenu) -> Option<Rectangle> {
    if menu.open_path.is_empty() {
        return None;
    }
    let rects = card_rects(ui, menu);
    (rects.len() > 1).then(|| rects.last().unwrap().1)
}

/// Fraction of the apex→cursor gap closed per tick (~16 ms) while on the open
/// branch. This is exponential smoothing on a fixed clock, not distance: during
/// a sweep the apex lags the cursor by roughly `velocity / APEX_EASE` (so a fast
/// diagonal gets a wide wedge, a slow one a tight one), and the moment the
/// cursor stops the gap keeps shrinking each tick until the apex catches up —
/// unlike a distance leash, which would hang a fixed gap behind forever. Lower =
/// more lag / wider wedge / slower catch-up.
const APEX_EASE: f32 = 0.2;

/// Ease the trajectory apex one tick toward `pos`. `None` (just opened) snaps to
/// the cursor. Driven only by the fixed-rate menu tick — never by raw moves — so
/// the lag is purely time-based and independent of how fast the mouse is moved
/// or how many move events the platform emits.
pub(crate) fn ease_apex(apex: Option<Point>, pos: Point) -> Point {
    let Some(apex) = apex else {
        return pos;
    };
    Point::new(
        apex.x + (pos.x - apex.x) * APEX_EASE,
        apex.y + (pos.y - apex.y) * APEX_EASE,
    )
}

/// The trajectory triangle: apex at `origin` (the leashed cursor), base on
/// the flyout's near edge. The base is pushed [`TRAJECTORY_EDGE_BUFFER`] *into*
/// the flyout so the wedge bridges the card's padding/border (otherwise the
/// cursor lands in the dead strip between the wedge and the first row, and the
/// flyout drops), and extended [`TRAJECTORY_BASE_BUFFER`] past the top and bottom
/// so the corner rows aren't on a razor-thin edge.
pub(crate) fn flyout_triangle(origin: Point, flyout: Rectangle) -> (Point, Point, Point) {
    let near_x = if origin.x <= flyout.x {
        flyout.x + TRAJECTORY_EDGE_BUFFER
    } else {
        flyout.x + flyout.width - TRAJECTORY_EDGE_BUFFER
    };
    (
        origin,
        Point::new(near_x, flyout.y - TRAJECTORY_BASE_BUFFER),
        Point::new(near_x, flyout.y + flyout.height + TRAJECTORY_BASE_BUFFER),
    )
}

/// Whether `cur` is sweeping into the open flyout — inside the triangle from
/// `origin` to the flyout's near edge. While true, a hover onto an off-branch
/// row is ignored so the diagonal path to the submenu isn't hijacked by the rows
/// it crosses.
pub(crate) fn heading_to_flyout(origin: Point, cur: Point, flyout: Rectangle) -> bool {
    let (a, b, c) = flyout_triangle(origin, flyout);
    point_in_triangle(cur, a, b, c)
}

const TRAJECTORY_BASE_BUFFER: f32 = 32.0;
/// How far the triangle's base reaches into the flyout (past its near edge), to
/// cover the card padding + border the cursor crosses before reaching a row.
const TRAJECTORY_EDGE_BUFFER: f32 = 12.0;

/// Smoothed cursor speed (px/s) separating a deliberate crawl from a sweep.
/// Under it, an off-branch hover commits immediately — the guard never arms —
/// and while a row is pending, time spent under it counts toward the stall.
/// Sweeping toward a flyout runs well into the hundreds of px/s; picking a
/// row by feel sits well under this.
const GUARD_SLOW_SPEED: f32 = 90.0;
/// How long the pointer must go without fast motion (idle included) before a
/// pending off-branch row commits anyway: the sweep didn't materialize, so the
/// row under the cursor is what the user wants.
const GUARD_STALL_MS: u64 = 260;
/// EMA time constant for the smoothed speed. Small enough that a flick out of
/// rest reads fast within a couple of move events, large enough that one
/// jittery sample doesn't flip the slow/fast verdict.
const GUARD_SPEED_TAU: f32 = 0.08;
/// Δt ceiling per move sample. Move events pause entirely while the cursor
/// rests, so the gap to the *next* event says nothing about how fast that next
/// motion is — cap it near two frames and judge the flick by its distance.
const GUARD_SPEED_MAX_DT: f32 = 0.032;

fn point_in_triangle(p: Point, a: Point, b: Point, c: Point) -> bool {
    let sign = |p1: Point, p2: Point, p3: Point| {
        (p1.x - p3.x) * (p2.y - p3.y) - (p2.x - p3.x) * (p1.y - p3.y)
    };
    let d1 = sign(p, a, b);
    let d2 = sign(p, b, c);
    let d3 = sign(p, c, a);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

fn card_outer_height(entries: &[MenuEntry], max_height: f32) -> f32 {
    let content: f32 = entries.iter().map(MenuEntry::height).sum();
    (content + MENU_CARD_PAD * 2.0).min(max_height)
}

fn row_top_offset(entries: &[MenuEntry], idx: usize) -> f32 {
    entries.iter().take(idx).map(MenuEntry::height).sum()
}

/// Width that fits the widest row of `entries` on one line, clamped to the
/// card min/max (with scrollbar reserve when the card will scroll).
fn card_width(ui: &Diffui, entries: &[MenuEntry]) -> f32 {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let mut content: f32 = 0.0;
    for entry in entries {
        let w = match entry {
            MenuEntry::Item {
                label,
                detail,
                emphasized,
                ..
            } => {
                let label_font = if *emphasized {
                    emphasis_font(font, crate::theme::Weight::MEDIUM)
                } else {
                    font
                };
                let mut w = measure::line_width(label, MENU_LABEL_SIZE, label_font);
                if let Some(detail) = detail {
                    w += MENU_ROW_GAP + measure::line_width(detail, MENU_DETAIL_SIZE, mono);
                }
                w
            }
            MenuEntry::Submenu { label, .. } => {
                measure::line_width(label, MENU_LABEL_SIZE, font) + MENU_ROW_GAP + CHEVRON_WIDTH
            }
            MenuEntry::Disabled { label, reason } => {
                let mut w = measure::line_width(label, MENU_LABEL_SIZE, font);
                if let Some(reason) = reason {
                    w += MENU_ROW_GAP + measure::line_width(reason, MENU_DETAIL_SIZE, font);
                }
                w
            }
            MenuEntry::Separator => 0.0,
        };
        content = content.max(w);
    }

    // A scrolling card's bar floats over the rows' right inset, which is as
    // wide as the bar's gutter, so it needs no width of its own.
    let raw = MENU_CARD_PAD * 2.0 + MENU_ITEM_PAD_X * 2.0 + content + MENU_TEXT_SLACK;
    raw.clamp(MENU_MIN_WIDTH, MENU_MAX_WIDTH).ceil()
}

/// Footprint of the submenu chevron. The icon box is square, so this value is
/// both the chevron's rendered size and the width the menu-width calc reserves.
const CHEVRON_WIDTH: f32 = 13.0;

/// An element that records its own bounds as it lays out, and the cell it records
/// them in: how a dropdown anchors to its whole trigger. Make it an absolute child
/// of the (`relative`) trigger; a listener built in the same frame reads the cell.
pub(crate) fn anchor_tracker() -> (Rc<Cell<Rectangle>>, impl IntoElement) {
    let anchor = Rc::new(Cell::new(Rectangle::new(Point::ORIGIN, Size::ZERO)));
    let recorded = anchor.clone();
    let tracker = canvas(
        move |bounds, _, _| recorded.set(bounds.into()),
        |_, _, _, _| {},
    )
    .absolute()
    .size_full();
    (anchor, tracker)
}

/// The whole popup overlay (scrim + glow + cards), or nothing when no menu is
/// open. Stacked over the app shell.
///
/// The backdrop is the event sink: it dismisses on a press outside the cards,
/// arms/closes on release, and hears the cursor move everywhere, over the cards
/// too, so the trajectory guard always has the latest position. It occludes, so
/// the app underneath stays inert (no hover, tooltip or cursor bleed-through); the
/// cards are its children and don't occlude it.
pub(crate) fn build_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Some(menu) = ui.menu() else {
        return div().into_any_element();
    };

    fn release(
        button: MouseButton,
        cx: &mut Cx,
    ) -> impl Fn(&MouseUpEvent, &mut Window, &mut gpui_kit::App) + 'static {
        Diffui::listen(cx, move |this, _, _, _| this.menu_on_scrim_release(button))
    }
    let mut layer = div()
        .id("menu-overlay")
        .absolute()
        .inset_0()
        .occlude()
        .on_any_mouse_down(Diffui::listen(cx, |this, _, _, _| this.menu_on_dismiss()))
        .on_mouse_up(MouseButton::Left, release(MouseButton::Left, cx))
        .on_mouse_up(MouseButton::Right, release(MouseButton::Right, cx))
        .on_mouse_move(Diffui::listen(cx, |this, event: &MouseMoveEvent, _, _| {
            this.menu_on_mouse_moved(event.position.into())
        }))
        .on_scroll(|_, _, cx| cx.stop_propagation());

    if let Some(rect) = menu.glow {
        layer = layer.child(glow_card(theme, rect, menu.opened_at));
    }
    for (prefix, rect) in card_rects(ui, menu) {
        let entries = entries_at(&menu.root, &prefix).unwrap_or(&[]);
        layer = layer.child(build_card(
            ui, theme, menu, entries, &prefix, rect, window, cx,
        ));
    }
    push_trajectory_debug(layer, ui, menu).into_any_element()
}

/// Draws the submenu trajectory triangle over the open menu: the apex (blue), its
/// wedge to the flyout's near edge, and the live cursor (green inside the triangle,
/// red outside). Compiled in only under the `trajectory-debug` feature; a no-op
/// otherwise.
#[cfg(feature = "trajectory-debug")]
fn push_trajectory_debug(
    layer: gpui_kit::Stateful<gpui_kit::Div>,
    ui: &Diffui,
    menu: &OverlayMenu,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let (Some(origin), Some(cursor), Some(fly)) =
        (menu.flyout_origin, menu.cursor, flyout_rect(ui, menu))
    else {
        return layer;
    };
    let (apex, base_top, base_bottom) = flyout_triangle(origin, fly);
    let inside = point_in_triangle(cursor, apex, base_top, base_bottom);
    let vertex = |at: Point| point(px(at.x), px(at.y));
    let dot = |at: Point, color: u32| {
        div()
            .absolute()
            .left(px(at.x - 3.5))
            .top(px(at.y - 3.5))
            .size(px(7.0))
            .rounded(px(3.5))
            .bg(gpui_kit::rgb(color))
    };
    layer
        .child(
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let mut wedge = gpui_kit::PathBuilder::fill();
                    wedge.move_to(vertex(apex));
                    wedge.line_to(vertex(base_top));
                    wedge.line_to(vertex(base_bottom));
                    wedge.close();
                    if let Ok(path) = wedge.build() {
                        window.paint_path(path, gpui_kit::rgba(0xf58c3830));
                    }
                },
            )
            .absolute()
            .size_full(),
        )
        .child(dot(apex, 0x66b3ff))
        .child(dot(cursor, if inside { 0x4de673 } else { 0xff4d4d }))
}

#[cfg(not(feature = "trajectory-debug"))]
fn push_trajectory_debug(
    layer: gpui_kit::Stateful<gpui_kit::Div>,
    _ui: &Diffui,
    _menu: &OverlayMenu,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    layer
}

#[allow(clippy::too_many_arguments)]
fn build_card(
    ui: &Diffui,
    theme: ThemeSpec,
    menu: &OverlayMenu,
    entries: &[MenuEntry],
    prefix: &[usize],
    rect: Rectangle,
    window: &mut Window,
    cx: &mut Cx,
) -> impl IntoElement {
    let mut list = div().flex().flex_col().w_full();
    for (idx, entry) in entries.iter().enumerate() {
        let mut path = prefix.to_vec();
        path.push(idx);
        let active = row_active(menu, &path);
        list = list.child(build_row(ui, theme, entry, path, active, cx));
    }

    let content_h: f32 = entries.iter().map(MenuEntry::height).sum();
    let scrolls = content_h + MENU_CARD_PAD * 2.0 > menu_max_height(ui);
    // The card swallows presses (either button) so a click inside it never reaches
    // the dismiss backdrop; rows carry their own hover/release handlers.
    let card = theme::popover(div(), theme)
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
        .overflow_hidden()
        .pt(px(MENU_CARD_PAD))
        .pb(px(MENU_CARD_PAD))
        .pl(px(MENU_CARD_PAD))
        .on_any_mouse_down(|_, _, cx| cx.stop_propagation());
    if !scrolls {
        return card.pr(px(MENU_CARD_PAD)).child(list).into_any_element();
    }

    // The id is per *branch*, not per depth: switching between sibling submenus at
    // the same depth must not inherit the old one's scroll state. The offsets it
    // records feed [`card_rects`]'s flyout anchoring.
    let depth = prefix.len();
    let id_path: Vec<String> = prefix.iter().map(usize::to_string).collect();
    let id = format!("menu-card-{}", id_path.join("-"));
    let handle = window
        .use_keyed_state(
            ElementId::Name(SharedString::from(id.clone())),
            cx,
            |_, _| ScrollHandle::new(),
        )
        .read(cx)
        .clone();
    let scrolled = handle.clone();
    // Out to the card's right edge, like every scroll view's bar; the card's right
    // inset moves inside the list.
    card.on_scroll(Diffui::listen(cx, move |this, _: &ScrollEvent, _, _| {
        this.menu_on_card_scrolled(depth, -f32::from(scrolled.offset().y))
    }))
    .child(
        div()
            .id(ElementId::Name(SharedString::from(id)))
            .size_full()
            .track_scroll(&handle)
            .overflow_y_scroll()
            .vertical_scrollbar(&handle)
            .child(div().pr(px(MENU_CARD_PAD)).child(list)),
    )
    .into_any_element()
}

fn build_row(
    ui: &Diffui,
    theme: ThemeSpec,
    entry: &MenuEntry,
    path: Vec<usize>,
    active: bool,
    cx: &mut Cx,
) -> AnyElement {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;

    let label_line = |label: &str, label_font, color: Color| {
        typography::line(label.to_owned(), label_font, MENU_LABEL_SIZE, color)
    };
    // Labels centre their caps in the row; a detail or chevron joins on the
    // label's baseline.
    let detail_line = |detail: &str, detail_font| {
        Aligned::text(
            div().w_full().flex().justify_end().child(
                typography::line(
                    detail.to_owned(),
                    detail_font,
                    MENU_DETAIL_SIZE,
                    theme.subtle_text,
                )
                .min_w_0()
                .text_ellipsis(),
            ),
            detail_font,
            MENU_DETAIL_SIZE,
        )
        .fill()
    };
    let inner = match entry {
        MenuEntry::Separator => {
            return div()
                .flex()
                .items_center()
                .h(px(MENU_SEP_HEIGHT))
                .px(px(MENU_ITEM_PAD_X))
                .child(div().w_full().h(px(1.0)).bg(theme.border))
                .into_any_element();
        }
        MenuEntry::Disabled { label, reason } => {
            let label = Aligned::text(
                label_line(label, font, theme.subtle_text),
                font,
                MENU_LABEL_SIZE,
            );
            match reason {
                // Why it's greyed, where an enabled row shows its chord.
                Some(reason) => {
                    typography::baseline_row([label, detail_line(reason, font)], MENU_ROW_GAP)
                }
                None => typography::baseline_row([label], 0.0),
            }
        }
        MenuEntry::Item {
            label,
            detail,
            emphasized,
            ..
        } => {
            let label_font = if *emphasized {
                emphasis_font(font, crate::theme::Weight::MEDIUM)
            } else {
                font
            };
            let label = Aligned::text(
                label_line(label, label_font, theme.text),
                label_font,
                MENU_LABEL_SIZE,
            );
            match detail {
                Some(detail) => {
                    typography::baseline_row([label, detail_line(detail, mono)], MENU_ROW_GAP)
                }
                None => typography::baseline_row([label], 0.0),
            }
        }
        MenuEntry::Submenu { label, .. } => typography::baseline_row(
            [
                Aligned::text(
                    label_line(label, font, theme.text).text_ellipsis(),
                    font,
                    MENU_LABEL_SIZE,
                )
                .fill(),
                Aligned::glyph(
                    icons::icon(icons::CHEVRON_RIGHT, CHEVRON_WIDTH, theme.muted_text),
                    CHEVRON_WIDTH,
                ),
            ],
            MENU_ROW_GAP,
        ),
    };

    let id: Vec<String> = path.iter().map(usize::to_string).collect();
    let hover_path = path.clone();
    fn pick(
        path: Vec<usize>,
        button: MouseButton,
        cx: &mut Cx,
    ) -> impl Fn(&MouseUpEvent, &mut Window, &mut gpui_kit::App) + 'static {
        Diffui::listen(cx, move |this, _, _, cx| {
            this.menu_on_select(path.clone(), button, cx);
            cx.stop_propagation();
        })
    }
    // Hover highlights / opens; a release of *either* button selects, so the
    // right-click menu is picked from with either hand. Which button released
    // is carried along: the opening gesture's own release is dropped by the update
    // side. No press listener: presses fall through to the card's capture so they
    // don't reach the dismiss backdrop.
    div()
        .id(ElementId::Name(SharedString::from(format!(
            "menu-row-{}",
            id.join("-")
        ))))
        .flex()
        .items_center()
        .w_full()
        .h(px(MENU_ROW_HEIGHT))
        .px(px(MENU_ITEM_PAD_X))
        .rounded(px(MENU_ROW_RADIUS))
        .cursor_pointer()
        // Same solid selection fill the palette's rows use, so "the row you're on"
        // looks identical across every list overlay.
        .when(active, |row| row.bg(theme.selected_file))
        .child(inner)
        .on_hover(Diffui::listen(cx, move |this, entered: &bool, _, _| {
            if *entered {
                this.menu_on_hover(hover_path.clone());
            }
        }))
        .on_mouse_up(MouseButton::Left, pick(path.clone(), MouseButton::Left, cx))
        .on_mouse_up(MouseButton::Right, pick(path, MouseButton::Right, cx))
        .into_any_element()
}

fn glow_card(theme: ThemeSpec, rect: Rectangle, opened_at: Instant) -> impl IntoElement {
    let elapsed = Instant::now()
        .saturating_duration_since(opened_at)
        .as_secs_f32();
    let pulse = 0.5 + 0.5 * (elapsed * GLOW_PULSE_SPEED).sin();
    let accent = theme.accent;
    div()
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
        .bg(Color {
            a: 0.10 + 0.14 * pulse,
            ..accent
        })
        .border(px(1.5))
        .border_color(Color {
            a: 0.55 + 0.45 * pulse,
            ..accent
        })
        // Traces the row card it sits on.
        .rounded(px(crate::revision_list::ROW_CARD_RADIUS))
        // A soft accent halo: the "glow" half of the effect, pulsing with the
        // border.
        .shadow(vec![BoxShadow {
            color: Hsla::from(Color {
                a: 0.5 + 0.4 * pulse,
                ..accent
            }),
            offset: point(px(0.0), px(0.0)),
            blur_radius: px(8.0),
            spread_radius: px(0.0),
            inset: false,
        }])
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: Size = Size {
        width: 1200.0,
        height: 900.0,
    };

    /// A zero-height anchor — a right-click cursor, or a flyout's parent row.
    fn at(x: f32, y: f32) -> Rectangle {
        Rectangle::new(Point::new(x, y), Size::ZERO)
    }

    fn card(width: f32, height: f32) -> Size {
        Size::new(width, height)
    }

    fn opened_by(button: gpui_kit::MouseButton) -> OverlayMenu {
        OverlayMenu::new(Vec::new(), AnchorSpec::At(Point::ORIGIN), Some(button))
    }

    #[test]
    fn card_drops_below_an_anchor_it_fits_under() {
        let rect = card_rect(WIN, at(100.0, 200.0), 0.0, card(220.0, 300.0));
        assert_eq!((rect.x, rect.y), (100.0, 200.0));
    }

    #[test]
    fn card_flips_above_a_cursor_it_would_overflow_under() {
        // The right-click bug: 900px window, cursor at y=700, 300px of menu.
        // The card belongs above the cursor — sliding it up to fit would park a
        // row under the pointer for the same click's release to run.
        let rect = card_rect(WIN, at(100.0, 700.0), 0.0, card(220.0, 300.0));
        assert_eq!(rect.y, 400.0);
        assert!(rect.y + rect.height <= 700.0);
    }

    #[test]
    fn flipped_dropdown_keeps_its_gap_off_the_trigger() {
        let trigger = Rectangle {
            x: 40.0,
            y: 820.0,
            width: 90.0,
            height: 24.0,
        };
        let below = card_rect(WIN, trigger, MENU_ANCHOR_GAP, card(220.0, 30.0));
        assert_eq!(below.y, 820.0 + 24.0 + MENU_ANCHOR_GAP);
        // Too tall to drop: the same gap now sits between the card's bottom and
        // the trigger's top.
        let above = card_rect(WIN, trigger, MENU_ANCHOR_GAP, card(220.0, 300.0));
        assert_eq!(above.y + above.height, 820.0 - MENU_ANCHOR_GAP);
    }

    #[test]
    fn card_clamps_only_when_neither_side_fits() {
        // Window-tall card: no room either way, so it fills the window and
        // scrolls.
        let rect = card_rect(WIN, at(0.0, 500.0), 0.0, card(220.0, 900.0));
        assert_eq!(rect.y, 0.0);
        // An anchor tall enough to leave room on neither side: bottom-aligned.
        let tall = Rectangle {
            x: 0.0,
            y: 40.0,
            width: 10.0,
            height: 800.0,
        };
        let rect = card_rect(WIN, tall, 0.0, card(220.0, 300.0));
        assert_eq!(rect.y, 600.0);
    }

    #[test]
    fn card_x_stays_inside_the_window() {
        let rect = card_rect(WIN, at(1100.0, 10.0), 0.0, card(220.0, 100.0));
        assert_eq!(rect.x, 980.0);
        // Wider than the window itself: pinned to the left edge, not negative.
        let rect = card_rect(WIN, at(1100.0, 10.0), 0.0, card(1400.0, 100.0));
        assert_eq!(rect.x, 0.0);
    }

    #[test]
    fn the_opening_release_is_swallowed_exactly_once() {
        let mut menu = opened_by(gpui_kit::MouseButton::Right);
        assert!(menu.opening_release(gpui_kit::MouseButton::Right));
        // Spent: the next right-release is a click of its own and picks.
        assert!(!menu.opening_release(gpui_kit::MouseButton::Right));
    }

    #[test]
    fn the_other_button_picks_without_waiting() {
        // A left-click on a row of a right-click menu is a whole press-release
        // of its own, so it selects at once...
        let mut menu = opened_by(gpui_kit::MouseButton::Right);
        assert!(!menu.opening_release(gpui_kit::MouseButton::Left));
        // ...and leaves the right button still owing its opening release.
        assert!(menu.opening_release(gpui_kit::MouseButton::Right));
    }

    #[test]
    fn a_menu_with_no_opening_press_picks_on_the_first_release() {
        let mut menu = OverlayMenu::new(Vec::new(), AnchorSpec::At(Point::ORIGIN), None);
        assert!(!menu.opening_release(gpui_kit::MouseButton::Left));
    }
}

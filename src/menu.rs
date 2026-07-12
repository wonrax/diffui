//! Cross-platform popup-menu overlay: the iced rendering + interaction for the
//! toolbar dropdowns (fetch branches / revset presets) and the revision
//! right-click context menu. macOS uses a real `NSMenu` (see [`crate::macos_native`]);
//! every other platform — and any future one — uses this.
//!
//! A menu is a tree of [`MenuEntry`]. The open menu lives on [`crate::Diffui`]
//! as `menu: Option<OverlayMenu>`, holding both the tree and the live
//! interaction state (which flyout is open, what's highlighted, the
//! anchor/glow rects). The renderer walks the open submenu path, placing one
//! card per level with `pin` at absolute window coordinates so flyouts sit
//! beside their parent row.
//!
//! Interaction mirrors AppKit/web menus: press the trigger, drag over items,
//! release to pick (`mouse_area::on_release` fires on any mouse-up over a row,
//! regardless of where the press began). A plain click also works — the menu
//! stays open after the opening click's release and a later click selects.

use std::time::Instant;

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree},
};
use iced::{
    Background, Border, Color, Element, Event, Length, Padding, Point, Rectangle, Size, Theme,
    alignment,
    widget::{
        Space, column, container, mouse_area, opaque, pin, row, scrollable,
        scrollable::{Direction, Scrollbar},
        stack, text,
    },
};

use crate::icons;
use crate::measure;
use crate::theme::{self, ThemeSpec, chip_background, emphasis_font, popover_style, text_size};
use crate::{Diffui, MenuAction, Message};
use diffui_core::RevisionSelection;

/// Messages from an open popup menu, nested under [`Message::Menu`].
#[derive(Debug, Clone)]
pub enum MenuMessage {
    /// Hovered a row (path of indices through submenus): highlight it and open
    /// its flyout if it's a submenu.
    Hover(Vec<usize>),
    /// Cursor moved while a menu is open (window coords) — drives the submenu
    /// trajectory guard and keeps the app underneath inert.
    MouseMoved(iced::Point),
    /// Released over a row: pick it (if a leaf).
    Select(Vec<usize>),
    /// A press inside the card — swallowed so it doesn't dismiss.
    CapturePress,
    /// Dismiss the open menu (press outside, or Esc).
    Dismiss,
    /// A release on the dismiss scrim — arms/dismisses outside-dismiss.
    ScrimRelease,
    /// No-op tick that keeps `view` re-running so the right-click glow pulses.
    Tick,
}

// ── Card geometry ───────────────────────────────────────────────────────────
const MENU_MIN_WIDTH: f32 = 200.0;
const MENU_MAX_WIDTH: f32 = 460.0;
const MENU_MAX_HEIGHT: f32 = 420.0;
const MENU_CARD_PAD: f32 = 6.0;
const MENU_ITEM_PAD_X: f32 = 8.0;
/// `Scrollbar::spacing` for a scrolling card — embedded rail (reserves its own
/// width) so long rows don't run under it.
const MENU_SCROLLBAR_SPACING: f32 = 6.0;
/// Rail flush against the card's right padding so its margin to the edge
/// matches the top/bottom padding.
const MENU_SCROLLBAR_MARGIN: f32 = 0.0;
/// Sub-pixel cushion so a row sized to exactly its measured width doesn't
/// ellipsize from float rounding.
const MENU_TEXT_SLACK: f32 = 2.0;
/// Minimum gap between an item's label and its right-aligned detail/chevron.
const MENU_ROW_GAP: f32 = 16.0;
/// Fixed per-row heights — fixed so flyout positions (which key off a parent
/// row's offset down its card) are exact rather than guessed.
const MENU_ROW_HEIGHT: f32 = 26.0;
const MENU_SEP_HEIGHT: f32 = 9.0;
/// Gap between a trigger's bottom edge and the menu it drops (toolbar carets).
const MENU_ANCHOR_GAP: f32 = 4.0;
/// Radians/second for the right-click glow pulse (~1.2 s period).
const GLOW_PULSE_SPEED: f32 = 5.0;

// ── Model ────────────────────────────────────────────────────────────────────

/// One node of a popup menu.
#[derive(Debug, Clone)]
pub(crate) enum MenuEntry {
    /// A selectable leaf. `detail` is right-aligned mono (the revset expression);
    /// `emphasized` bumps the label weight (the fetch menu's header row).
    Item {
        label: String,
        detail: Option<String>,
        emphasized: bool,
        action: MenuAction,
    },
    /// A parent row whose children open in a flyout beside it.
    Submenu {
        label: String,
        items: Vec<MenuEntry>,
    },
    /// A greyed, non-interactive row (e.g. an otherwise-empty submenu).
    Disabled {
        label: String,
    },
    Separator,
}

impl MenuEntry {
    pub(crate) fn item(label: impl Into<String>, action: MenuAction) -> Self {
        MenuEntry::Item {
            label: label.into(),
            detail: None,
            emphasized: false,
            action,
        }
    }

    fn height(&self) -> f32 {
        match self {
            MenuEntry::Separator => MENU_SEP_HEIGHT,
            _ => MENU_ROW_HEIGHT,
        }
    }
}

/// Where a menu opens from. Only constructed by the non-macOS iced overlay menu
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
    /// Whether an outside press/release dismisses. Starts `false` for a
    /// left-click trigger (so the opening click's release is swallowed instead
    /// of closing) and `true` for a right-click (no opening left-click to eat).
    pub armed: bool,
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
    /// pointer steers out of the triangle (veers off the flyout).
    pub pending_row: Option<Vec<usize>>,
    /// Revision selection backing the menu, for actions read on demand
    /// (author/committer/description copies).
    pub selection: Option<RevisionSelection>,
    /// Row to pulse-highlight while open (the right-clicked revision).
    pub glow: Option<Rectangle>,
    /// When the menu opened — drives the glow pulse phase.
    pub opened_at: Instant,
}

impl OverlayMenu {
    // Constructed only for the non-macOS iced overlay menu; macOS uses a native
    // `NSMenu` and never builds an `OverlayMenu`.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn new(root: Vec<MenuEntry>, anchor: AnchorSpec, armed: bool) -> Self {
        OverlayMenu {
            root,
            anchor,
            open_path: Vec::new(),
            highlight: None,
            armed,
            entered: false,
            cursor: None,
            flyout_origin: None,
            pending_row: None,
            selection: None,
            glow: None,
            opened_at: Instant::now(),
        }
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
        self.open_path = match self.entry_at(&path) {
            Some(MenuEntry::Submenu { .. }) => path.clone(),
            // Keep the ancestor chain open; drop anything deeper.
            _ => path[..path.len().saturating_sub(1)].to_vec(),
        };
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

// ── Renderer ──────────────────────────────────────────────────────────────────

/// The whole popup overlay (scrim + glow + cards), or an empty `Space` when no
/// menu is open. Stacked over the app shell in `view`.
pub(crate) fn build_overlay(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(menu) = ui.menu.as_ref() else {
        return Space::new().into();
    };

    // Full-screen event sink behind the cards: dismisses on a press outside the
    // cards, arms/closes on release, and — crucially — captures cursor moves and
    // scrolls so the app underneath stays inert (no hover/tooltip bleed-through)
    // and the latest cursor reaches the trajectory guard. Cards sit above it, so
    // they see events first; only what they ignore reaches here.
    let mut layers: Vec<Element<'_, Message>> = vec![
        Backdrop {
            on_press: Message::Menu(MenuMessage::Dismiss),
            on_release: Message::Menu(MenuMessage::ScrimRelease),
            on_move: |p| Message::Menu(MenuMessage::MouseMoved(p)),
        }
        .into(),
    ];

    if let Some(rect) = menu.glow {
        layers.push(
            pin(glow_card(theme, rect.width, rect.height, menu.opened_at))
                .x(rect.x)
                .y(rect.y)
                .into(),
        );
    }

    for (prefix, rect) in card_rects(ui, menu) {
        let entries = entries_at(&menu.root, &prefix).unwrap_or(&[]);
        layers.push(
            pin(build_card(ui, theme, menu, entries, &prefix, rect.width))
                .x(rect.x)
                .y(rect.y)
                .into(),
        );
    }

    push_trajectory_debug(&mut layers, ui, menu);

    // The Backdrop swallows *events*, but `mouse_interaction` is a separate
    // per-layer query that ignores event capture — without `opaque`, the app
    // underneath still painted its own cursor (the diff view's I-beam)
    // through the menu scrim.
    opaque(stack(layers).width(Length::Fill).height(Length::Fill))
}

/// Draws the submenu trajectory triangle over the open menu: the apex (blue),
/// its wedge to the flyout's near edge, and the live cursor (green inside the
/// triangle, red outside). Compiled in only under the `trajectory-debug`
/// feature; a no-op otherwise.
#[cfg(feature = "trajectory-debug")]
fn push_trajectory_debug<'a>(
    layers: &mut Vec<Element<'a, Message>>,
    ui: &Diffui,
    menu: &OverlayMenu,
) {
    use iced::widget::canvas;

    let (Some(origin), Some(cur), Some(fly)) =
        (menu.flyout_origin, menu.cursor, flyout_rect(ui, menu))
    else {
        return;
    };
    let (apex, base_top, base_bottom) = flyout_triangle(origin, fly);

    struct Triangle {
        apex: Point,
        base_top: Point,
        base_bottom: Point,
        cursor: Point,
        inside: bool,
    }

    impl canvas::Program<Message> for Triangle {
        type State = ();

        fn draw(
            &self,
            _state: &(),
            renderer: &iced::Renderer,
            _theme: &Theme,
            bounds: Rectangle,
            _cursor: mouse::Cursor,
        ) -> Vec<canvas::Geometry> {
            let mut frame = canvas::Frame::new(renderer, bounds.size());
            let accent = Color::from_rgb(0.96, 0.55, 0.22);
            let triangle = canvas::Path::new(|p| {
                p.move_to(self.apex);
                p.line_to(self.base_top);
                p.line_to(self.base_bottom);
                p.close();
            });
            frame.fill(&triangle, Color { a: 0.18, ..accent });
            frame.stroke(
                &triangle,
                canvas::Stroke::default().with_color(accent).with_width(1.0),
            );
            frame.fill(
                &canvas::Path::circle(self.apex, 3.5),
                Color::from_rgb(0.40, 0.70, 1.0),
            );
            let cursor_color = if self.inside {
                Color::from_rgb(0.30, 0.90, 0.45)
            } else {
                Color::from_rgb(1.0, 0.30, 0.30)
            };
            frame.fill(&canvas::Path::circle(self.cursor, 3.5), cursor_color);
            vec![frame.into_geometry()]
        }
    }

    layers.push(
        canvas(Triangle {
            apex,
            base_top,
            base_bottom,
            cursor: cur,
            inside: point_in_triangle(cur, apex, base_top, base_bottom),
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
    );
}

#[cfg(not(feature = "trajectory-debug"))]
fn push_trajectory_debug(
    _layers: &mut Vec<Element<'_, Message>>,
    _ui: &Diffui,
    _menu: &OverlayMenu,
) {
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

/// On-screen rect of each open card (root + one per open flyout level), in the
/// order root → deepest. Shared by the renderer (to place cards) and the
/// hit-test / trajectory guard (to read their geometry), so the two never drift.
fn card_rects(ui: &Diffui, menu: &OverlayMenu) -> Vec<(Vec<usize>, Rectangle)> {
    let win = ui.window_size;
    let mut out: Vec<(Vec<usize>, Rectangle)> = Vec::new();
    let mut prefix: Vec<usize> = Vec::new();
    let (mut x, mut y) = match menu.anchor {
        AnchorSpec::Below(r) => (r.x, r.y + r.height + MENU_ANCHOR_GAP),
        AnchorSpec::At(p) => (p.x, p.y),
    };

    loop {
        let Some(entries) = entries_at(&menu.root, &prefix) else {
            break;
        };
        let width = card_width(ui, entries);
        let height = card_outer_height(entries);
        let px = x.min((win.width - width).max(0.0)).max(0.0);
        let py = y.min((win.height - height).max(0.0)).max(0.0);
        out.push((
            prefix.clone(),
            Rectangle {
                x: px,
                y: py,
                width,
                height,
            },
        ));

        let level = prefix.len();
        let Some(&idx) = menu.open_path.get(level) else {
            break;
        };
        let Some(MenuEntry::Submenu { items, .. }) = entries.get(idx) else {
            break;
        };
        let flyout_w = card_width(ui, items);
        // Open to the right; flip left when there's no room.
        x = if px + width + flyout_w <= win.width {
            px + width
        } else {
            (px - flyout_w).max(0.0)
        };
        // Align the flyout's first row with its parent row.
        y = py + MENU_CARD_PAD + row_top_offset(entries, idx) - MENU_CARD_PAD;
        prefix.push(idx);
    }
    out
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

fn card_outer_height(entries: &[MenuEntry]) -> f32 {
    let content: f32 = entries.iter().map(MenuEntry::height).sum();
    (content + MENU_CARD_PAD * 2.0).min(MENU_MAX_HEIGHT)
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
                    emphasis_font(font, iced::font::Weight::Medium)
                } else {
                    font
                };
                let mut w = measure::line_width(label, text_size::UI, label_font);
                if let Some(detail) = detail {
                    w += MENU_ROW_GAP + measure::line_width(detail, text_size::CAPTION, mono);
                }
                w
            }
            MenuEntry::Submenu { label, .. } => {
                measure::line_width(label, text_size::UI, font) + MENU_ROW_GAP + CHEVRON_WIDTH
            }
            MenuEntry::Disabled { label } => measure::line_width(label, text_size::UI, font),
            MenuEntry::Separator => 0.0,
        };
        content = content.max(w);
    }

    let content_h: f32 = entries.iter().map(MenuEntry::height).sum();
    let reserve = content_h + MENU_CARD_PAD * 2.0 > MENU_MAX_HEIGHT;
    let scrollbar = if reserve {
        theme::SCROLLBAR_WIDTH + MENU_SCROLLBAR_MARGIN * 2.0 + MENU_SCROLLBAR_SPACING
    } else {
        0.0
    };
    let raw = MENU_CARD_PAD * 2.0 + MENU_ITEM_PAD_X * 2.0 + content + scrollbar + MENU_TEXT_SLACK;
    raw.clamp(MENU_MIN_WIDTH, MENU_MAX_WIDTH).ceil()
}

/// Footprint of the submenu chevron. The icon box is square, so this value is
/// both the chevron's rendered size and the width the menu-width calc reserves.
const CHEVRON_WIDTH: f32 = 13.0;

fn build_card<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    menu: &'a OverlayMenu,
    entries: &'a [MenuEntry],
    prefix: &[usize],
    width: f32,
) -> Element<'a, Message> {
    let rows = entries.iter().enumerate().map(|(idx, entry)| {
        let mut path = prefix.to_vec();
        path.push(idx);
        let active = row_active(menu, &path);
        build_row(ui, theme, entry, path, active)
    });
    let list = column(rows.collect::<Vec<_>>()).spacing(0);

    let content_h: f32 = entries.iter().map(MenuEntry::height).sum();
    let body: Element<'a, Message> = if content_h + MENU_CARD_PAD * 2.0 > MENU_MAX_HEIGHT {
        scrollable(list)
            .width(Length::Fill)
            .height(Length::Shrink)
            .direction(Direction::Vertical(
                Scrollbar::default()
                    .width(theme::SCROLLBAR_WIDTH)
                    .scroller_width(theme::SCROLLBAR_WIDTH)
                    .margin(MENU_SCROLLBAR_MARGIN)
                    .spacing(MENU_SCROLLBAR_SPACING),
            ))
            .style(move |_, status| theme::iced_scrollable_style(theme, status))
            .into()
    } else {
        list.into()
    };

    // The card swallows presses (either button) so a click inside it never
    // reaches the dismiss backdrop; rows carry their own hover/release handlers.
    mouse_area(
        container(body)
            .width(Length::Fixed(width))
            .max_height(MENU_MAX_HEIGHT)
            .padding(Padding::from([MENU_CARD_PAD, MENU_CARD_PAD]))
            .style(move |_| popover_style(theme)),
    )
    .on_press(Message::Menu(MenuMessage::CapturePress))
    .on_right_press(Message::Menu(MenuMessage::CapturePress))
    .into()
}

fn build_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    entry: &'a MenuEntry,
    path: Vec<usize>,
    active: bool,
) -> Element<'a, Message> {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;

    let inner: Element<'a, Message> = match entry {
        MenuEntry::Separator => {
            return container(
                container(Space::new())
                    .width(Length::Fill)
                    .height(Length::Fixed(1.0))
                    .style(move |_| container::Style {
                        background: Some(Background::Color(theme.border)),
                        ..container::Style::default()
                    }),
            )
            .height(Length::Fixed(MENU_SEP_HEIGHT))
            .padding(Padding::from([0.0, MENU_ITEM_PAD_X]))
            .align_y(alignment::Vertical::Center)
            .into();
        }
        MenuEntry::Disabled { label } => text(label)
            .size(text_size::UI)
            .color(theme.subtle_text)
            .font(font)
            .width(Length::Fill)
            .wrapping(iced::advanced::text::Wrapping::None)
            .ellipsis(iced::advanced::text::Ellipsis::End)
            .into(),
        MenuEntry::Item {
            label,
            detail,
            emphasized,
            ..
        } => {
            let label_font = if *emphasized {
                emphasis_font(font, iced::font::Weight::Medium)
            } else {
                font
            };
            let label_widget = text(label)
                .size(text_size::UI)
                .color(theme.text)
                .font(label_font)
                .wrapping(iced::advanced::text::Wrapping::None);
            match detail {
                Some(detail) => row![
                    label_widget,
                    text(detail)
                        .size(text_size::CAPTION)
                        .color(theme.subtle_text)
                        .font(mono)
                        .width(Length::Fill)
                        .align_x(iced::advanced::text::Alignment::Right)
                        .wrapping(iced::advanced::text::Wrapping::None)
                        .ellipsis(iced::advanced::text::Ellipsis::End),
                ]
                .spacing(MENU_ROW_GAP)
                .align_y(alignment::Vertical::Center)
                .into(),
                None => label_widget.width(Length::Fill).into(),
            }
        }
        MenuEntry::Submenu { label, .. } => row![
            text(label)
                .size(text_size::UI)
                .color(theme.text)
                .font(font)
                .width(Length::Fill)
                .wrapping(iced::advanced::text::Wrapping::None)
                .ellipsis(iced::advanced::text::Ellipsis::End),
            icons::icon(icons::CHEVRON_RIGHT, CHEVRON_WIDTH, theme.muted_text),
        ]
        .spacing(MENU_ROW_GAP)
        .align_y(alignment::Vertical::Center)
        .into(),
    };

    let bg = if active {
        Some(Background::Color(chip_background(theme.muted_text)))
    } else {
        None
    };
    let styled = container(inner)
        .width(Length::Fill)
        .height(Length::Fixed(MENU_ROW_HEIGHT))
        .padding(Padding::from([0.0, MENU_ITEM_PAD_X]))
        .align_y(alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: bg,
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });

    // Hover highlights / opens; a release of *either* button selects (so a
    // left-click works and a right-press-drag-release off the right-click that
    // opened the menu does too). No `on_press` — presses fall through to the
    // card's capture so they don't reach the dismiss backdrop, while a
    // press-started-on-the-trigger drag still releases onto the row.
    mouse_area(styled)
        .on_enter(Message::Menu(MenuMessage::Hover(path.clone())))
        .on_release(Message::Menu(MenuMessage::Select(path.clone())))
        .on_right_release(Message::Menu(MenuMessage::Select(path)))
        .interaction(mouse::Interaction::Pointer)
        .into()
}

fn glow_card(
    theme: ThemeSpec,
    width: f32,
    height: f32,
    opened_at: Instant,
) -> Element<'static, Message> {
    let elapsed = Instant::now()
        .saturating_duration_since(opened_at)
        .as_secs_f32();
    let pulse = 0.5 + 0.5 * (elapsed * GLOW_PULSE_SPEED).sin();
    let accent = theme.accent;
    let fill = Color {
        a: 0.10 + 0.14 * pulse,
        ..accent
    };
    let border = Color {
        a: 0.55 + 0.45 * pulse,
        ..accent
    };
    container(Space::new())
        .width(Length::Fixed(width))
        .height(Length::Fixed(height))
        .style(move |_| container::Style {
            background: Some(Background::Color(fill)),
            border: Border {
                width: 1.5,
                color: border,
                radius: 6.0.into(),
            },
            // A soft accent halo — the "glow" half of the effect, pulsing with
            // the border.
            shadow: iced::Shadow {
                color: Color {
                    a: 0.5 + 0.4 * pulse,
                    ..accent
                },
                offset: iced::Vector::new(0.0, 0.0),
                blur_radius: 8.0,
            },
            ..container::Style::default()
        })
        .into()
}

// ── AnchorArea: report a trigger's bounds on press ───────────────────────────

/// A transparent wrapper that reports its own layout rectangle when pressed —
/// used to anchor a dropdown edge-to-edge under its trigger. Events reach the
/// content first, so a real `button`/`text_input` inside captures its own
/// press; only a press the content ignores (the inert caret) fires `on_press`,
/// carrying the *whole wrapper's* bounds.
pub(crate) struct AnchorArea<'a, Message> {
    content: Element<'a, Message, Theme>,
    on_press: Box<dyn Fn(Rectangle) -> Message + 'a>,
}

pub(crate) fn anchor_area<'a, Message>(
    content: impl Into<Element<'a, Message, Theme>>,
    on_press: impl Fn(Rectangle) -> Message + 'a,
) -> AnchorArea<'a, Message> {
    AnchorArea {
        content: content.into(),
        on_press: Box::new(on_press),
    }
}

impl<Message> Widget<Message, Theme, iced::Renderer> for AnchorArea<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            shell,
            viewport,
        );

        if shell.is_event_captured() {
            return;
        }

        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event
            && cursor.is_over(layout.bounds())
        {
            shell.publish((self.on_press)(layout.bounds()));
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: iced::Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<AnchorArea<'a, Message>> for Element<'a, Message, Theme> {
    fn from(area: AnchorArea<'a, Message>) -> Self {
        Element::new(area)
    }
}

// ── Backdrop: the modal event sink behind the cards ──────────────────────────

/// A transparent full-screen leaf that swallows every mouse event the cards
/// above it don't handle. Presses dismiss, releases arm/close, and moves +
/// scrolls are captured (publishing the cursor for the trajectory guard) so the
/// app underneath the open menu never hovers, tooltips, or scrolls.
struct Backdrop {
    on_press: Message,
    on_release: Message,
    on_move: fn(Point) -> Message,
}

impl Widget<Message, Theme, iced::Renderer> for Backdrop {
    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(Length::Fill, Length::Fill, Size::ZERO))
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        // Capture unconditionally — no `cursor.is_over` gate. The cards above
        // capture their own presses, so whatever reaches this full-screen sink
        // belongs to it. The gate would wrongly bail when the cursor is over a
        // card: the stack "levitates" the cursor for layers beneath an
        // interactive one, so `is_over` reads false there — exactly when we must
        // still swallow the move so it can't bleed to the app's hover/tooltips.
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(_)) => {
                shell.publish(self.on_press.clone());
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(_)) => {
                shell.publish(self.on_release.clone());
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                shell.publish((self.on_move)(*position));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::WheelScrolled { .. }) => {
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        _renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
    }
}

impl<'a> From<Backdrop> for Element<'a, Message, Theme> {
    fn from(backdrop: Backdrop) -> Self {
        Element::new(backdrop)
    }
}

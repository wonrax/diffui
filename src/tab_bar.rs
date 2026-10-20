//! The multi-repo title-bar strip: one tab per open repository plus the
//! controls that manage them.
//!
//! Layout, left → right: a tab for each open repo (VCS badge, `owner/name`
//! with the owner dimmed, an optional uncommitted-changes dot, and a close
//! `×`), a `+` button that opens the path dialog, and — pinned right — a
//! `⌘K` control that opens the command palette (the design's repo-search
//! `⌘P` is folded into our existing palette per the spec). Dragging a tab
//! along the strip reorders it — see [`TabStrip`].
//!
//! The strip and the path dialog both read straight off `&Diffui`, mirroring
//! `sidebar` / `palette` / `find`, so there is no intermediate view model to
//! keep in sync.

use iced::{
    Alignment, Background, Border, Color, Element, Event, Length, Padding, Point, Rectangle,
    Shadow, Size, Theme, Vector,
    advanced::{
        Layout, Renderer as _, Shell, Widget, layout, mouse as advanced_mouse, overlay, renderer,
        widget::{Operation, Tree, tree},
    },
    alignment,
    font::Weight,
    keyboard, mouse,
    time::Instant,
    widget::{Space, button, column, container, mouse_area, row, stack, text},
    window,
};

use crate::chrome;
use crate::icons;
use crate::repository::Vcs;
use crate::theme::{
    self, ThemeSpec, centered_control_content_fill, chip_background, emphasis_font,
    ghost_button_style, input_style, modal_style, radius, text_size, well_fill,
};
use crate::typography::{self, Aligned};
use crate::ui;
use crate::{Action, Diffui, HoverTarget, Message, TabId, UiEvent, WindowEvent};

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

/// Build the title-bar tab strip. Returns an empty `Space` when no repos are
/// open (the empty-state view owns the window in that case).
pub fn build_tab_bar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if ui.tabs.is_empty() {
        return Space::new().into();
    }

    let mut children = Vec::with_capacity(ui.tabs.len() + 1);
    for (index, tab) in ui.tabs.iter().enumerate() {
        let active = index == ui.active;
        children.push(tab_widget(ui, theme, tab, active));
    }
    children.push(add_button(theme));
    let tabs_row = TabStrip {
        children,
        tab_ids: ui.tabs.iter().map(|tab| tab.id).collect(),
        active: ui.active,
        theme,
    };

    // Top padding only: the tabs run flush to the strip's bottom edge so the
    // active one connects to the toolbar band below (see `tab_widget`). When
    // the strip stands in for the title bar (macOS) it's pinned to a fixed
    // height with the content bottom-aligned instead; the traffic lights stay
    // centered on the full strip (see `chrome::position_window_controls`),
    // like a native browser window.
    let titlebar_height = chrome::title_bar_height();
    let v_pad = if titlebar_height.is_some() {
        0.0
    } else {
        STRIP_TOP_PAD
    };
    let mut strip = row![]
        .align_y(alignment::Vertical::Center)
        .spacing(6)
        .padding(Padding {
            top: v_pad,
            right: 8.0,
            bottom: 0.0,
            left: 8.0,
        });
    // Reserve space at the leading edge for OS window controls that overlap our
    // content (macOS traffic lights); zero elsewhere.
    let inset = chrome::leading_inset();
    if inset > 0.0 {
        strip = strip.push(Space::new().width(Length::Fixed(inset)));
    }
    let strip = strip
        .push(tabs_row)
        .push(Space::new().width(Length::Fill))
        .push(palette_hint(theme, ui.config.mono_font));

    let mut bar = container(strip)
        .width(Length::Fill)
        .style(move |_| bar_style(theme));
    if let Some(height) = titlebar_height {
        bar = bar
            .height(Length::Fixed(height))
            .align_y(alignment::Vertical::Bottom);
    }

    // When the native title bar is hidden/replaced (macOS today), the strip's
    // empty area doubles as the window-drag handle: tabs and buttons consume
    // their own clicks, so only the gaps initiate a drag. A double-click on that
    // same empty area runs the system zoom/minimize action, matching a native
    // title bar (the native double-click handling is lost with the title bar).
    if chrome::drag_region() {
        mouse_area(bar)
            .on_press(Message::Window(WindowEvent::TitleBarDrag))
            .on_double_click(Message::Window(WindowEvent::TitleBarDoubleClick))
            .into()
    } else {
        bar.into()
    }
}

/// Background for the tab strip / title-bar surface: a recessed band, darker
/// than the toolbar below it, so the active tab — filled with the toolbar's
/// color — reads as a connected block against it (browser-style). No border:
/// the seam between the active tab and the toolbar must stay invisible.
fn bar_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(well_fill(theme))),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// One repo tab. The trailing `×` closes it; a press anywhere else selects it
/// and arms a reorder drag, both of which [`TabStrip`] handles — a drag has to
/// keep tracking the cursor after it leaves the tab that started it.
fn tab_widget<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    tab: &'a crate::Tab,
    active: bool,
) -> Element<'a, Message> {
    let badge = match &tab.source {
        crate::TabSource::Repo { vcs, .. } => vcs_badge(*vcs, theme, ui.config.mono_font),
        crate::TabSource::GitHubPr(_) => pr_badge(theme, ui.config.mono_font),
    };

    let name_color = if active { theme.text } else { theme.muted_text };
    // owner + name as a tight unit (no gap), matching the design's RepoLabel —
    // "code/diffui" reads as one path. The badge and dirty dot get the 6px
    // rhythm from the outer row instead. The mono badge and the sans name
    // share a baseline (see `crate::typography`).
    let name_font = emphasis_font(ui.config.ui_font, Weight::Medium);
    let mut repo_label = Vec::new();
    if !tab.owner.is_empty() {
        repo_label.push(Aligned::text(
            text(format!("{}/", tab.owner))
                .size(TAB_TEXT_SIZE)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
            ui.config.ui_font,
            TAB_TEXT_SIZE,
        ));
    }
    repo_label.push(Aligned::text(
        text(tab.name.as_str())
            .size(TAB_TEXT_SIZE)
            .color(name_color)
            .font(name_font),
        name_font,
        TAB_TEXT_SIZE,
    ));

    let label = typography::baseline_row([badge, typography::group(repo_label, 0.0)], 6.0);
    // The dot gives way to the `×` under the cursor, so it can't carry its
    // own tooltip; the label says what the tab is and what the dot means.
    let label = crate::ui::tooltip(
        label,
        tab_tooltip(tab),
        iced::widget::tooltip::Position::Bottom,
        ui.config.ui_font,
        theme,
    );

    let id = tab.id;
    let hovered = ui.hovered == Some(HoverTarget::Tab(id));

    // The uncommitted-changes dot lives in the close button's slot and gives
    // way to the `×` under the cursor, as in editor tabs: every tab keeps one
    // trailing slot, so the gap after the name is the same whether or not a
    // repo is dirty, and the tab's width never jumps as that changes.
    let trailing: Element<'a, Message> = if tab_is_dirty(tab) && !hovered {
        container(dirty_dot(theme))
            .center_x(Length::Fixed(CLOSE_SIDE))
            .center_y(Length::Fixed(CLOSE_SIDE))
            .into()
    } else {
        button(
            container(icons::icon(icons::CLOSE, 13.0, theme.subtle_text))
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fixed(CLOSE_SIDE))
        .height(Length::Fixed(CLOSE_SIDE))
        .padding(0)
        .on_press(Message::Action(Action::CloseTab(id)))
        .style(move |_, status| ghost_button_style(theme, status))
        .into()
    };

    let inner = row![label, trailing]
        .spacing(theme::space::XS)
        .align_y(alignment::Vertical::Center);

    // Browser-style tab: the active tab is a top-rounded block filled with the
    // toolbar band's own color and running flush to the strip's bottom edge.
    // Its concave joins share the top corners' radius and are painted outside
    // layout in the strip's background pass, continuing beneath neighboring
    // inactive tab areas.
    // Inactive tabs stay quiet; hovering one paints a floating wash over the
    // same box minus `WASH_BOTTOM_GAP`. The label centers in the same
    // top-anchored box in every state, so nothing shifts on hover or selection.
    let wash_fill = (!active && hovered).then(|| chip_background(theme.muted_text));
    let wash = container(inner)
        .padding(Padding::from([0.0, LABEL_PAD_X]))
        .height(Length::Fixed(TAB_HEIGHT - WASH_BOTTOM_GAP))
        .align_y(alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: wash_fill.map(Background::Color),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: radius::BUTTON.into(),
            },
            ..container::Style::default()
        });

    let body = container(wash)
        .height(Length::Fixed(TAB_HEIGHT))
        .align_y(alignment::Vertical::Top)
        .style(move |_| container::Style {
            background: active.then_some(Background::Color(theme.panel_background_elevated)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: iced::border::top(radius::BUTTON),
            },
            ..container::Style::default()
        });

    // Hover is tracked in app state; `on_move` re-asserts it every frame the
    // cursor is over the tab, so the one-frame flicker that `on_enter`/`on_exit`
    // race into when crossing between adjacent tabs is immediately corrected.
    let interactive = mouse_area(body)
        .on_enter(Message::Ui(UiEvent::SetHover(Some(HoverTarget::Tab(id)))))
        .on_move(move |_| Message::Ui(UiEvent::SetHover(Some(HoverTarget::Tab(id)))))
        .on_exit(Message::Ui(UiEvent::SetHover(None)))
        .interaction(mouse::Interaction::Pointer);

    interactive.into()
}

/// The tabs plus the trailing `+`, laid out as a row — and the owner of the
/// press on a tab, which selects it and may grow into a reorder drag.
///
/// The order itself lives in the app: a drag publishes [`Action::MoveTab`] each
/// time the dragged tab crosses a neighbor, so the strip is re-laid out around
/// it while it is still in the air. Everything that makes that look like motion
/// is widget-local — the dragged tab following the cursor, and the slide that
/// carries a displaced tab from where it was drawn to where its slot now is.
struct TabStrip<'a> {
    /// One element per tab, in tab order, then the add button.
    children: Vec<Element<'a, Message>>,
    /// Identity of each leading child, so a drag names its tab by id and
    /// survives the reorders it causes.
    tab_ids: Vec<TabId>,
    active: usize,
    theme: ThemeSpec,
}

#[derive(Default)]
struct StripState {
    /// Armed on a tab press, active past [`DRAG_THRESHOLD`].
    drag: Option<TabDrag>,
    /// How far each tab is drawn from its slot, easing to zero. Tabs at rest
    /// have no entry.
    slides: Vec<(TabId, f32)>,
    /// Each tab's slot (x within the strip) as of the last layout — what a
    /// reorder is measured against to start the slides above.
    slots: Vec<(TabId, f32)>,
    /// The tab a drop left in the air. Drawn above its neighbors until the
    /// slides settle, like the drag it just ended.
    settling: Option<TabId>,
    /// When the slides were last stepped.
    last_frame: Option<Instant>,
}

struct TabDrag {
    id: TabId,
    /// Where the press landed — the activation threshold anchor.
    origin_x: f32,
    /// How far into the tab the cursor sits, so the tab stays under it the
    /// way it was picked up. Taken when the drag activates, not at the press.
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
}

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

impl TabStrip<'_> {
    /// Where an active drag puts its tab's left edge: under the cursor, held
    /// within the run of tabs so it can't be dragged over the `+` or off the
    /// strip.
    fn dragged_x(&self, drag: &TabDrag, layout: Layout<'_>, index: usize) -> f32 {
        let first = layout.child(0).bounds();
        let last = layout.child(self.tab_ids.len() - 1).bounds();
        let width = layout.child(index).bounds().width;
        let max_x = (last.x + last.width - width).max(first.x);
        (drag.cursor_x - drag.grab).clamp(first.x, max_x)
    }

    /// How far child `index` is drawn — and hit-tested — from its slot.
    fn offset(&self, state: &StripState, layout: Layout<'_>, index: usize) -> f32 {
        let Some(id) = self.tab_ids.get(index).copied() else {
            return 0.0;
        };
        match state.dragged() {
            Some(drag) if drag.id == id => {
                self.dragged_x(drag, layout, index) - layout.child(index).bounds().x
            }
            // Whole pixels: a tab at rest keeps the exact position the layout
            // gave it, and one in motion doesn't shimmer between them.
            _ => state
                .slides
                .iter()
                .find(|(tab, _)| *tab == id)
                .map_or(0.0, |(_, offset)| offset.round()),
        }
    }

    fn child_layout<'l>(&self, state: &StripState, layout: Layout<'l>, index: usize) -> Layout<'l> {
        shifted(layout.child(index), self.offset(state, layout, index))
    }

    /// Let go of an active drag: the tab slides into its slot from wherever
    /// the cursor left it.
    fn drop_tab(&self, state: &mut StripState, drag: &TabDrag, layout: Layout<'_>) {
        if let Some(index) = self.tab_ids.iter().position(|id| *id == drag.id) {
            let offset = self.dragged_x(drag, layout, index) - layout.child(index).bounds().x;
            state.add_slide(drag.id, offset);
            state.settling = Some(drag.id);
        }
    }
}

fn shifted(layout: Layout<'_>, offset: f32) -> Layout<'_> {
    if offset == 0.0 {
        return layout;
    }
    let position = layout.position();
    layout.move_to(Point::new(position.x + offset, position.y))
}

impl Widget<Message, Theme, iced::Renderer> for TabStrip<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<StripState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(StripState::default())
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
        // A tab can close under a drag (⌘W needs no pointer).
        let state = tree.state.downcast_mut::<StripState>();
        if state
            .drag
            .as_ref()
            .is_some_and(|drag| !self.tab_ids.contains(&drag.id))
        {
            state.drag = None;
        }
        state.slides.retain(|(id, _)| self.tab_ids.contains(id));
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fit, Length::Fit)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = layout::flex::resolve(
            layout::flex::Axis::Horizontal,
            renderer,
            limits,
            Length::Fit,
            Length::Fit,
            Padding::ZERO,
            TAB_GAP,
            Alignment::Center,
            &mut self.children,
            &mut tree.children,
        );

        // A reorder lands here as tabs whose slots moved. Each one keeps being
        // drawn where it was and slides to the new slot from there — all but
        // the dragged tab, which is wherever the cursor is.
        let state = tree.state.downcast_mut::<StripState>();
        let slots: Vec<(TabId, f32)> = self
            .tab_ids
            .iter()
            .zip(node.children())
            .map(|(id, child)| (*id, child.bounds().x))
            .collect();
        if state.dragged().is_some() || state.settling.is_some() {
            let dragged = state.dragged().map(|drag| drag.id);
            for (id, x) in &slots {
                let previous = state.slots.iter().find(|(tab, _)| tab == id);
                if let Some((_, previous)) = previous
                    && Some(*id) != dragged
                    && (previous - x).abs() > 0.5
                {
                    state.add_slide(*id, previous - x);
                }
            }
        }
        state.slots = slots;

        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds(), viewport);
        operation.traverse(&mut |operation| {
            self.children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
                .for_each(|((child, state), layout)| {
                    child
                        .as_widget_mut()
                        .operate(state, layout, viewport, renderer, operation);
                });
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: advanced_mouse::Cursor,
        renderer: &iced::Renderer,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let Tree {
            state, children, ..
        } = tree;
        let state = state.downcast_mut::<StripState>();

        // An active drag owns the cursor: no hover washes, no `×` presses.
        let child_cursor = if state.dragged().is_some() {
            advanced_mouse::Cursor::Unavailable
        } else {
            cursor
        };
        let offsets: Vec<f32> = (0..self.children.len())
            .map(|index| self.offset(state, layout, index))
            .collect();
        for (((child, tree), child_layout), offset) in self
            .children
            .iter_mut()
            .zip(children)
            .zip(layout.children())
            .zip(offsets)
        {
            child.as_widget_mut().update(
                tree,
                event,
                shifted(child_layout, offset),
                child_cursor,
                renderer,
                shell,
                viewport,
            );
        }

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                // A release that never arrived must not leave a tab stuck to
                // the cursor past the next press.
                state.drag = None;
                // The `×` and the `+` are buttons and have taken theirs.
                if shell.is_event_captured() {
                    return;
                }
                let Some(position) = cursor.position() else {
                    return;
                };
                let pressed = (0..self.tab_ids.len()).find(|index| {
                    self.child_layout(state, layout, *index)
                        .bounds()
                        .contains(position)
                });
                let Some(index) = pressed else {
                    return;
                };
                let id = self.tab_ids[index];
                shell.publish(Message::Action(Action::SelectTab(id)));
                shell.capture_event();
                // A lone tab has nowhere to go.
                if self.tab_ids.len() > 1 {
                    state.drag = Some(TabDrag {
                        id,
                        origin_x: position.x,
                        grab: 0.0,
                        home: index,
                        active: false,
                        cursor_x: position.x,
                    });
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let Some(armed) = state.drag.as_ref() else {
                    return;
                };
                let (id, activating) = (armed.id, !armed.active);
                if activating && (position.x - armed.origin_x).abs() < DRAG_THRESHOLD {
                    return;
                }
                let Some(index) = self.tab_ids.iter().position(|tab| *tab == id) else {
                    return;
                };
                // Take hold of the tab where it is drawn right now, not where
                // the press landed: anchored to the press it would jump the
                // threshold's worth the moment it came loose. From here the
                // cursor places it, so any slide it was in ends.
                let grab = activating
                    .then(|| position.x - self.child_layout(state, layout, index).bounds().x);
                if activating {
                    state.slides.retain(|(tab, _)| *tab != id);
                }
                let Some(drag) = state.drag.as_mut() else {
                    return;
                };
                if let Some(grab) = grab {
                    drag.active = true;
                    drag.grab = grab;
                }
                drag.cursor_x = position.x;
                let widths: Vec<f32> = (0..self.tab_ids.len())
                    .map(|tab| layout.child(tab).bounds().width)
                    .collect();
                let target = drop_index(
                    &widths,
                    index,
                    layout.child(index).bounds().x,
                    self.dragged_x(drag, layout, index),
                );
                if target != index {
                    shell.publish(Message::Action(Action::MoveTab {
                        id: drag.id,
                        to: target,
                    }));
                }
                shell.capture_event();
                shell.request_redraw();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | Event::Window(window::Event::Unfocused) => {
                let Some(drag) = state.drag.take() else {
                    return;
                };
                if !drag.active {
                    return;
                }
                self.drop_tab(state, &drag, layout);
                shell.capture_event();
                shell.request_redraw();
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) => {
                // Esc abandons the drag and puts the tab back where it was
                // picked up. The app's own Esc handling rides the same event
                // through the subscription, as it does for a revision drag.
                let Some(drag) = state.drag.take() else {
                    return;
                };
                if !drag.active {
                    return;
                }
                self.drop_tab(state, &drag, layout);
                shell.publish(Message::Action(Action::MoveTab {
                    id: drag.id,
                    to: drag.home,
                }));
                shell.request_redraw();
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                if state.slides.is_empty() {
                    state.settling = None;
                    state.last_frame = None;
                    return;
                }
                let elapsed = state
                    .last_frame
                    .map_or(0.0, |last| now.duration_since(last).as_secs_f32());
                state.last_frame = Some(*now);
                let remaining = (-elapsed / SLIDE_TAU).exp();
                for (_, offset) in &mut state.slides {
                    *offset *= remaining;
                }
                state.slides.retain(|(_, offset)| offset.abs() >= 0.5);
                shell.request_redraw();
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: advanced_mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> advanced_mouse::Interaction {
        let state = tree.state.downcast_ref::<StripState>();
        if state.dragged().is_some() {
            return advanced_mouse::Interaction::Grabbing;
        }
        self.children
            .iter()
            .zip(&tree.children)
            .enumerate()
            .map(|(index, (child, tree))| {
                child.as_widget().mouse_interaction(
                    tree,
                    self.child_layout(state, layout, index),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: advanced_mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<StripState>();
        let cursor = if state.dragged().is_some() {
            advanced_mouse::Cursor::Unavailable
        } else {
            cursor
        };
        // Drawn above the rest: the tab being dragged, or the one a drop is
        // still settling.
        let raised = state
            .dragged()
            .map(|drag| drag.id)
            .or(state.settling)
            .and_then(|id| self.tab_ids.iter().position(|tab| *tab == id));

        if raised != Some(self.active) && self.active < self.tab_ids.len() {
            let bounds = self.child_layout(state, layout, self.active).bounds();
            draw_join_fillet(renderer, self.theme, bounds, true);
            draw_join_fillet(renderer, self.theme, bounds, false);
        }
        for (index, (child, tree)) in self.children.iter().zip(&tree.children).enumerate() {
            if raised == Some(index) {
                continue;
            }
            let layout = self.child_layout(state, layout, index);
            child
                .as_widget()
                .draw(tree, renderer, theme, style, layout, cursor, viewport);
        }

        // The tab in the air goes in a layer of its own. Drawing it last isn't
        // enough: within a layer text lands above every quad, so the labels it
        // passes over would show through its fill.
        let Some(index) = raised else {
            return;
        };
        renderer.with_layer(*viewport, |renderer| {
            let layout = self.child_layout(state, layout, index);
            if index == self.active {
                // A join's cover quad is twice the join's size and only
                // disappears against the bare strip. Over a neighbor it would
                // blank the corner of its label, so in the air each join is
                // clipped to its own square, which sits below the text.
                for left in [true, false] {
                    renderer.with_layer(join_bounds(layout.bounds(), left), |renderer| {
                        draw_join_fillet(renderer, self.theme, layout.bounds(), left);
                    });
                }
            } else {
                // Only the active tab has a fill to hide what it passes over;
                // a background one (its activation was refused) gets a stand-in.
                draw_lifted_backing(renderer, self.theme, layout.bounds());
            }
            self.children[index].as_widget().draw(
                &tree.children[index],
                renderer,
                theme,
                style,
                layout,
                cursor,
                viewport,
            );
        });
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Vec<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<TabStrip<'a>> for Element<'a, Message> {
    fn from(strip: TabStrip<'a>) -> Self {
        Element::new(strip)
    }
}

/// Stand-in body for a dragged tab that isn't the active one: the strip's own
/// color so the labels it passes over don't show through, under the wash an
/// inactive tab wears on hover.
fn draw_lifted_backing(renderer: &mut iced::Renderer, theme: ThemeSpec, tab: Rectangle) {
    let wash = Rectangle {
        height: tab.height - WASH_BOTTOM_GAP,
        ..tab
    };
    for fill in [well_fill(theme), chip_background(theme.muted_text)] {
        renderer.fill_quad(
            renderer::Quad {
                bounds: wash,
                border: Border {
                    width: 0.0,
                    color: Color::TRANSPARENT,
                    radius: radius::BUTTON.into(),
                },
                shadow: Shadow::default(),
                snap: true,
            },
            fill,
        );
    }
}

/// The square outside one of the active tab's bottom corners that its concave
/// join to the toolbar band fills.
fn join_bounds(tab: Rectangle, left: bool) -> Rectangle {
    Rectangle {
        x: if left {
            tab.x - radius::BUTTON
        } else {
            tab.x + tab.width
        },
        y: tab.y + tab.height - radius::BUTTON,
        width: radius::BUTTON,
        height: radius::BUTTON,
    }
}

fn draw_join_fillet(renderer: &mut iced::Renderer, theme: ThemeSpec, tab: Rectangle, left: bool) {
    renderer.fill_quad(
        renderer::Quad {
            bounds: join_bounds(tab, left),
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        theme.panel_background_elevated,
    );

    let cover_bounds = Rectangle {
        x: if left {
            tab.x - radius::BUTTON * 2.0
        } else {
            tab.x + tab.width
        },
        y: tab.y + tab.height - radius::BUTTON * 2.0,
        width: radius::BUTTON * 2.0,
        height: radius::BUTTON * 2.0,
    };
    renderer.fill_quad(
        renderer::Quad {
            bounds: cover_bounds,
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: if left {
                    iced::border::bottom_right(radius::BUTTON)
                } else {
                    iced::border::bottom_left(radius::BUTTON)
                },
            },
            shadow: Shadow::default(),
            snap: true,
        },
        well_fill(theme),
    );
}

/// `jj` / `git` badge — a soft colored chip carrying the VCS kind, so a
/// mixed set of repos reads at a glance.
fn vcs_badge(vcs: Vcs, theme: ThemeSpec, mono: iced::Font) -> Aligned<'static, Message> {
    let (label, color) = match vcs {
        // Violet for jj (the trunk/lane hue), warm amber for git — distinct
        // from the diff add/del greens and reds.
        Vcs::Jj => ("jj", theme.lane_base),
        Vcs::Git => ("git", theme.modified_token),
    };
    badge_chip(label, color, mono)
}

/// `pr` badge for GitHub pull-request tabs — accent-colored so it reads as
/// "remote" next to the local jj/git chips.
fn pr_badge(theme: ThemeSpec, mono: iced::Font) -> Aligned<'static, Message> {
    badge_chip("pr", theme.accent, mono)
}

fn badge_chip(label: &'static str, color: Color, mono: iced::Font) -> Aligned<'static, Message> {
    let padding = typography::trimmed_padding(Padding::from([1, 4]), mono, text_size::BADGE);
    Aligned::boxed(
        container(text(label).size(text_size::BADGE).color(color).font(mono))
            .padding(padding)
            .style(move |_| container::Style {
                background: Some(Background::Color(chip_background(color))),
                border: Border {
                    width: 0.0,
                    color: Color::TRANSPARENT,
                    radius: 4.0.into(),
                },
                ..container::Style::default()
            }),
        padding,
        typography::TextBox::default_line(mono, text_size::BADGE),
    )
}

/// Small ring signalling the working copy (`@`) has changes. A ring in the
/// "modified" colour rather than a filled accent dot: in an editor a filled
/// dot means unsaved work, and jj has none — it snapshots `@` on its own.
fn dirty_dot(theme: ThemeSpec) -> Element<'static, Message> {
    container(Space::new())
        .width(Length::Fixed(7.0))
        .height(Length::Fixed(7.0))
        .style(move |_| container::Style {
            background: None,
            border: Border {
                width: 1.5,
                color: theme.modified_token,
                radius: 3.5.into(),
            },
            ..container::Style::default()
        })
        .into()
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

/// `+` button — opens the path dialog to add another repository. A square
/// sized and anchored exactly like an inactive tab's hover wash (same
/// height, same top edge, same clearance from the strip floor), so the
/// strip's controls sit on one visual grid.
fn add_button(theme: ThemeSpec) -> Element<'static, Message> {
    let side = TAB_HEIGHT - WASH_BOTTOM_GAP;
    container(
        button(container(icons::icon(icons::PLUS, 15.0, theme.muted_text)).center(Length::Fill))
            .width(Length::Fixed(side))
            .height(Length::Fixed(side))
            .padding(0)
            .on_press(Message::Action(Action::OpenRepoDialog))
            .style(move |_, status| ghost_button_style(theme, status)),
    )
    .height(Length::Fixed(TAB_HEIGHT))
    .align_y(alignment::Vertical::Top)
    .into()
}

/// The right-hand `⌘K` control. The design put a repo-search `⌘P` here; per
/// the spec it instead opens our existing command palette. Styled as a
/// keycap — hairline border, mono glyphs — so it reads as "press this key",
/// not as another action button.
fn palette_hint(theme: ThemeSpec, mono: iced::Font) -> Element<'static, Message> {
    let chip = container(
        text(chrome::cmd_label("K"))
            .size(text_size::CAPTION)
            .color(theme.muted_text)
            .font(mono),
    )
    .padding(Padding::from([1, 5]))
    .style(move |_| container::Style {
        background: Some(Background::Color(chip_background(theme.muted_text))),
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: 4.0.into(),
        },
        ..container::Style::default()
    });

    button(chip)
        .padding(Padding::from([2, 4]))
        .on_press(Message::Action(Action::OpenPalette))
        .style(move |_, _| button::Style {
            background: None,
            text_color: theme.muted_text,
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: radius::SURFACE.into(),
            },
            shadow: Default::default(),
            snap: true,
        })
        .into()
}

/// Modal confirmation for a guarded mutation (see [`crate::ConfirmDialog`]).
/// Returns an empty `Space` when closed. Lives here because it mirrors the
/// open-repository modal's scrim + card construction exactly.
pub fn build_confirm_dialog(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(dialog) = ui.confirm() else {
        return Space::new().into();
    };
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    // Red fill: the confirm runs a mutation the jj CLI refuses by default.
    let accept = ui::text_button(
        None,
        dialog.confirm_label.as_str(),
        ui::Variant::Destructive,
        ui::Size::Standard,
        font,
        theme,
    )
    .on_press(Message::Action(Action::ConfirmAccept));
    let cancel = ui::cancel_button(ui::Size::Standard, font, theme)
        .on_press(Message::Action(Action::ConfirmCancel));

    let mut body =
        column![ui::dialog_title(&dialog.title, font, mono, theme)].spacing(ui::DIALOG_SPACING);
    if !dialog.body.is_empty() {
        body = body.push(ui::dialog_text(&dialog.body, font, mono, theme));
    }
    body = body.push(ui::button_row(None, [cancel.into(), accept.into()]));

    ui::modal(
        ui::dialog_card(body, ui::DIALOG_WIDTH, theme),
        Message::Action(Action::ConfirmCancel),
        Message::Ui(UiEvent::ConfirmNoOp),
    )
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

/// Modal overlay for opening a repository from a path. Returns an empty
/// `Space` when the dialog is closed. Click-outside and Esc cancel, so the
/// card has no Cancel button.
pub fn build_open_repo_dialog(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(dialog) = ui.open_repo_dialog() else {
        return Space::new().into();
    };

    ui::modal(
        open_repo_card(ui, theme, dialog),
        Message::Action(Action::CloseRepoDialog),
        Message::Ui(UiEvent::OpenRepoNoOp),
    )
}

/// The open prompt itself, shared by the modal and the welcome screen: a
/// palette-family card with one input band (path, folder picker, Open), an
/// error line when the last open failed, then the recent repositories.
pub(crate) fn open_repo_card<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    dialog: &'a crate::OpenRepoDialog,
) -> Element<'a, Message> {
    let input = iced::widget::text_input("Path, owner/repo#123, or PR URL", &dialog.path)
        .id(OPEN_REPO_INPUT_ID)
        .padding(typography::trimmed_padding(
            Padding {
                top: theme::space::MD,
                right: theme::space::MD,
                bottom: theme::space::MD,
                left: OPEN_CONTENT_X,
            },
            ui.config.mono_font,
            text_size::BODY_LG,
        ))
        .size(text_size::BODY_LG)
        .font(ui.config.mono_font)
        .on_input(|path| Message::Ui(UiEvent::OpenRepoPathChanged(path)))
        .on_submit(Message::Action(Action::SubmitRepoDialog))
        .style(move |_, _| iced::widget::text_input::Style {
            // Bare: the card is the field.
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
            ..input_style(theme)
        });

    let mut trailing = row![]
        .spacing(theme::space::XS)
        .align_y(alignment::Vertical::Center);
    #[cfg(target_os = "macos")]
    {
        trailing = trailing.push(ui::tooltip(
            ui::icon_button(
                icons::FOLDER_OPEN,
                theme.muted_text,
                ui::Size::Compact,
                theme,
            )
            .on_press(Message::Action(Action::ChooseRepoFolder)),
            "Choose folder…",
            iced::widget::tooltip::Position::Bottom,
            ui.config.ui_font,
            theme,
        ));
    }
    let mut open = ui::text_button(
        None,
        "Open",
        ui::Variant::Primary,
        ui::Size::Compact,
        ui.config.ui_font,
        theme,
    );
    if !dialog.path.trim().is_empty() {
        open = open.on_press(Message::Action(Action::SubmitRepoDialog));
    }
    trailing = trailing.push(open);

    // The trailing controls sit as far in from the right edge as from the
    // band's top and bottom.
    let band_inset = (OPEN_INPUT_BAND - ui::Size::Compact.height()) / 2.0;
    let band = container(
        row![
            crate::input::single_line(input, &dialog.path, ui.config.mono_font),
            trailing,
        ]
        .align_y(alignment::Vertical::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(OPEN_INPUT_BAND))
    .center_y(Length::Fixed(OPEN_INPUT_BAND))
    .padding(Padding {
        right: band_inset,
        ..Padding::ZERO
    });

    let mut body = column![band].spacing(0);

    if let Some(error) = &dialog.error {
        body = body.push(
            container(
                // The glyph centres on the first line's caps.
                typography::baseline_row(
                    [
                        Aligned::glyph(
                            icons::icon(icons::ALERT_TRIANGLE, ui::ICON_SIZE, theme.removed_text),
                            ui::ICON_SIZE,
                        ),
                        Aligned::text(
                            text(error.as_str())
                                .size(text_size::UI)
                                .color(theme.removed_text)
                                .font(ui.config.ui_font)
                                .width(Length::Fill)
                                .wrapping(text::Wrapping::Word),
                            ui.config.ui_font,
                            text_size::UI,
                        ),
                    ],
                    theme::space::MD,
                ),
            )
            .width(Length::Fill)
            .padding(Padding {
                top: 0.0,
                right: OPEN_CONTENT_X,
                bottom: theme::space::LG,
                left: OPEN_CONTENT_X,
            }),
        );
    }

    // A path being typed lists the folders it can continue into; anything
    // else shows the recents.
    let section = match dialog.suggestions() {
        Some((query, rows)) => folder_section(ui, theme, dialog, query, rows),
        None => recents_section(ui, theme),
    };
    if let Some((header, list, rows)) = section {
        body = body.push(
            container(Space::new())
                .width(Length::Fill)
                .height(Length::Fixed(1.0))
                .style(move |_| container::Style::default().background(theme.border)),
        );
        body = body.push(
            container(header)
                .width(Length::Fill)
                .height(Length::Fixed(OPEN_SECTION_HEIGHT))
                .center_y(Length::Fixed(OPEN_SECTION_HEIGHT)),
        );
        let list: Element<'_, Message> = if rows > OPEN_LIST_ROWS {
            // Past the fixed height the list scrolls, its bar at the card's
            // edge and the rows clear of it.
            container(
                ui::scrollable(
                    container(list).padding(Padding {
                        right: crate::scrollbar::GUTTER,
                        ..Padding::ZERO
                    }),
                    theme,
                )
                .height(Length::Fixed(OPEN_LIST_ROWS as f32 * OPEN_ROW_HEIGHT)),
            )
            .padding(Padding {
                top: 0.0,
                right: 0.0,
                ..Padding::from(OPEN_LIST_INSET)
            })
            .into()
        } else {
            container(list)
                .padding(Padding {
                    top: 0.0,
                    ..Padding::from(OPEN_LIST_INSET)
                })
                .into()
        };
        body = body.push(container(list).width(Length::Fill));
    }

    container(body)
        .width(Length::Fixed(OPEN_DIALOG_WIDTH))
        .style(move |_| modal_style(theme))
        .into()
}

/// The open prompt's recent list: anything in the MRU that isn't already
/// open; one click reopens it. `None` when there is nothing to list.
fn recents_section<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Option<(Element<'a, Message>, Element<'a, Message>, usize)> {
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
    let clear = ui::button(
        typography::centered(
            text("Clear")
                .size(text_size::CAPTION)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
            ui.config.ui_font,
            text_size::CAPTION,
        ),
        ui::Variant::Ghost,
        ui::Size::Compact,
        theme,
    )
    .height(Length::Fixed(OPEN_SECTION_PILL))
    .padding([0.0, OPEN_SECTION_PILL_PAD_X])
    .on_press(Message::Action(Action::ClearRecentRepos));
    let header = container(
        row![
            typography::centered(
                text("Recent")
                    .size(text_size::CAPTION)
                    .color(theme.subtle_text)
                    .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
                emphasis_font(ui.config.ui_font, Weight::Medium),
                text_size::CAPTION,
            ),
            Space::new().width(Length::Fill),
            clear,
        ]
        .align_y(alignment::Vertical::Center),
    )
    // "Clear"'s label, not its wash, ends on the content edge.
    .padding(Padding {
        right: OPEN_CONTENT_X - OPEN_SECTION_PILL_PAD_X,
        ..Padding::from([0.0, OPEN_CONTENT_X])
    });

    let rows = recents.len();
    let mut list = column![].spacing(0);
    for root in recents {
        list = list.push(recent_repo_compact_row(ui, theme, root));
    }
    Some((header.into(), list.into(), rows))
}

/// The folders under the typed path, headed by the folder being browsed.
/// `None` when nothing matches (or the folder is still being read).
fn folder_section<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    dialog: &'a crate::OpenRepoDialog,
    query: crate::path_browse::Query,
    rows: Vec<&'a crate::path_browse::Entry>,
) -> Option<(Element<'a, Message>, Element<'a, Message>, usize)> {
    if rows.is_empty() {
        return None;
    }
    let header = container(typography::centered(
        text(query.typed_dir)
            .size(text_size::CAPTION)
            .color(theme.subtle_text)
            .font(ui.config.mono_font)
            .width(Length::Fill)
            .wrapping(text::Wrapping::None)
            .ellipsis(text::Ellipsis::Middle),
        ui.config.mono_font,
        text_size::CAPTION,
    ))
    .padding([0.0, OPEN_CONTENT_X]);

    let count = rows.len();
    let mut list = column![].spacing(0);
    for (index, entry) in rows.into_iter().enumerate() {
        list = list.push(folder_row(
            ui,
            theme,
            entry,
            index,
            dialog.selected == Some(index),
        ));
    }
    Some((header.into(), list.into(), count))
}

/// One folder suggestion: glyph · name · a jj/git chip when the folder is a
/// repository (which a click opens rather than browses into).
fn folder_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    entry: &'a crate::path_browse::Entry,
    index: usize,
    selected: bool,
) -> Element<'a, Message> {
    let mut content = vec![
        Aligned::glyph(
            container(icons::icon(icons::FOLDER, ui::ICON_SIZE, theme.subtle_text))
                .width(Length::Fixed(OPEN_GLYPH_BOX))
                .center_x(Length::Fixed(OPEN_GLYPH_BOX)),
            ui::ICON_SIZE,
        ),
        Aligned::gap(OPEN_GLYPH_GAP),
        Aligned::text(
            text(entry.name.as_str())
                .size(text_size::BODY)
                .color(theme.text)
                .font(ui.config.ui_font)
                .width(Length::Fill)
                .wrapping(text::Wrapping::None)
                .ellipsis(text::Ellipsis::End),
            ui.config.ui_font,
            text_size::BODY,
        ),
    ];
    if let Some(vcs) = entry.vcs {
        content.push(vcs_badge(vcs, theme, ui.config.mono_font));
    }

    button(
        container(typography::baseline_row(content, 0.0))
            .height(Length::Fill)
            .center_y(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fixed(OPEN_ROW_HEIGHT))
    .padding(Padding::from([0.0, OPEN_CONTENT_X - OPEN_LIST_INSET]))
    .on_press(Message::Action(Action::OpenRepoPick(index)))
    .style(move |_, status| button::Style {
        background: (selected
            || matches!(status, button::Status::Hovered | button::Status::Pressed))
        .then_some(Background::Color(theme.selected_file)),
        text_color: theme.text,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: radius::CONTROL.into(),
        },
        shadow: Default::default(),
        snap: true,
    })
    .into()
}

/// One line of the open prompt's recent list, laid out like a palette row:
/// glyph · `owner/name` · the home-contracted path, with a remove `×` that
/// floats over the row's right end.
fn recent_repo_compact_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    root: &'a str,
) -> Element<'a, Message> {
    let (owner, name) = crate::repo_label(std::path::Path::new(root));
    // The name (sans) and the path (smaller mono) share a baseline; the
    // glyph centres on the caps (see `crate::typography`).
    let name_font = emphasis_font(ui.config.ui_font, Weight::Medium);
    let mut label = Vec::new();
    if !owner.is_empty() {
        label.push(Aligned::text(
            text(format!("{owner}/"))
                .size(text_size::BODY)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
            ui.config.ui_font,
            text_size::BODY,
        ));
    }
    label.push(Aligned::text(
        text(name)
            .size(text_size::BODY)
            .color(theme.text)
            .font(name_font),
        name_font,
        text_size::BODY,
    ));
    let content = container(typography::baseline_row(
        [
            Aligned::glyph(
                container(icons::icon(icons::FOLDER, ui::ICON_SIZE, theme.subtle_text))
                    .width(Length::Fixed(OPEN_GLYPH_BOX))
                    .center_x(Length::Fixed(OPEN_GLYPH_BOX)),
                ui::ICON_SIZE,
            ),
            Aligned::gap(OPEN_GLYPH_GAP),
            typography::group(label, 0.0),
            Aligned::gap(theme::space::MD),
            // Middle-ellipsized: a path's ends (where it lives, what it's
            // called) say more than its middle.
            Aligned::text(
                text(crate::contract_user_path(root))
                    .size(text_size::CAPTION)
                    .color(theme.subtle_text)
                    .font(ui.config.mono_font)
                    .width(Length::Fill)
                    .wrapping(text::Wrapping::None)
                    .ellipsis(text::Ellipsis::Middle),
                ui.config.mono_font,
                text_size::CAPTION,
            ),
            // Keeps the path clear of the floating `×`.
            Aligned::gap(OPEN_REMOVE_SIDE),
        ],
        0.0,
    ))
    .height(Length::Fill)
    .center_y(Length::Fill);

    let row_button = button(content)
        .width(Length::Fill)
        .height(Length::Fixed(OPEN_ROW_HEIGHT))
        .padding(Padding::from([0.0, OPEN_CONTENT_X - OPEN_LIST_INSET]))
        .on_press(Message::Action(Action::OpenRepo(root.to_owned())))
        .style(move |_, status| button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed => {
                    Some(Background::Color(theme.selected_file))
                }
                _ => None,
            },
            text_color: theme.text,
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: radius::CONTROL.into(),
            },
            shadow: Default::default(),
            snap: true,
        });
    // Inset from the row's end as far as from its top and bottom.
    let remove_inset = (OPEN_ROW_HEIGHT - OPEN_REMOVE_SIDE) / 2.0;
    let remove = button(centered_control_content_fill(icons::icon(
        icons::CLOSE,
        13.0,
        theme.subtle_text,
    )))
    .width(Length::Fixed(OPEN_REMOVE_SIDE))
    .height(Length::Fixed(OPEN_REMOVE_SIDE))
    .padding(0)
    .on_press(Message::Action(Action::RemoveRecentRepo(root.to_owned())))
    .style(move |_, status| {
        let mut style = ghost_button_style(theme, status);
        style.border.radius = radius::CONTROL.into();
        style
    });

    stack![
        row_button,
        container(remove)
            .width(Length::Fill)
            .height(Length::Fixed(OPEN_ROW_HEIGHT))
            .align_x(alignment::Horizontal::Right)
            .center_y(Length::Fixed(OPEN_ROW_HEIGHT))
            .padding(Padding {
                right: remove_inset,
                ..Padding::ZERO
            }),
    ]
    .into()
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

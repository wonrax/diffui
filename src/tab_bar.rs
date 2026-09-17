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
    widget::{Space, button, column, container, mouse_area, opaque, row, stack, text},
    window,
};

use crate::chrome;
use crate::icons;
use crate::repository::Vcs;
use crate::theme::{
    ThemeSpec, centered_control_content, centered_control_content_fill, chip_background,
    destructive_button_style, dialog_button_style, emphasis_font, ghost_button_style, input_style,
    modal_style, primary_button_style, radius, scrim_style, text_size, well_fill,
};
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

/// Both modal cards (confirm, open-repo) hang at the same distance below the
/// top edge, like the command palette — anchored rather than centered, so
/// they don't jump when their content grows (error lines, recents).
const DIALOG_TOP_OFFSET: f32 = 80.0;
const OPEN_DIALOG_WIDTH: f32 = 560.0;

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
    let v_pad = if titlebar_height.is_some() { 0.0 } else { 6.0 };
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
    // rhythm from the outer row instead.
    let mut repo_label = row![].spacing(0).align_y(alignment::Vertical::Center);
    if !tab.owner.is_empty() {
        repo_label = repo_label.push(
            text(format!("{}/", tab.owner))
                .size(TAB_TEXT_SIZE)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
        );
    }
    repo_label = repo_label.push(
        text(tab.name.as_str())
            .size(TAB_TEXT_SIZE)
            .color(name_color)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
    );

    let mut label = row![badge, repo_label]
        .spacing(6)
        .align_y(alignment::Vertical::Center);
    // Always reserve the dot's slot (a transparent placeholder when clean) so a
    // tab's width doesn't jump as its dirty status changes — e.g. when a revset
    // that excludes `@` makes the working copy drop out of the loaded set.
    label = label.push(if tab_is_dirty(tab) {
        dirty_dot(theme)
    } else {
        Space::new()
            .width(Length::Fixed(6.0))
            .height(Length::Fixed(6.0))
            .into()
    });

    let id = tab.id;

    let close = button(icons::icon(icons::CLOSE, 13.0, theme.subtle_text))
        // Symmetric padding around the fixed-size icon box makes the hover
        // target a square that fills the tab's text line height. The old
        // asymmetric padding (0 vertical, 4 horizontal) left it short and wide.
        .padding(Padding::from([2, 2]))
        .on_press(Message::Action(Action::CloseTab(id)))
        .style(move |_, status| ghost_button_style(theme, status));

    let inner = row![label, close]
        .spacing(4)
        .align_y(alignment::Vertical::Center);

    // Browser-style tab: the active tab is a top-rounded block filled with the
    // toolbar band's own color and running flush to the strip's bottom edge.
    // Its concave joins share the top corners' radius and are painted outside
    // layout in the strip's background pass, continuing beneath neighboring
    // inactive tab areas.
    // Inactive tabs stay quiet; hovering one paints a floating wash over the
    // same box minus `WASH_BOTTOM_GAP`. The label centers in the same
    // top-anchored box in every state, so nothing shifts on hover or selection.
    let hovered = ui.hovered == Some(HoverTarget::Tab(id));
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
fn vcs_badge(vcs: Vcs, theme: ThemeSpec, mono: iced::Font) -> Element<'static, Message> {
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
fn pr_badge(theme: ThemeSpec, mono: iced::Font) -> Element<'static, Message> {
    badge_chip("pr", theme.accent, mono)
}

fn badge_chip(label: &'static str, color: Color, mono: iced::Font) -> Element<'static, Message> {
    container(text(label).size(text_size::BADGE).color(color).font(mono))
        .padding(Padding::from([1, 4]))
        .style(move |_| container::Style {
            background: Some(Background::Color(chip_background(color))),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// Small filled dot signalling the repo has uncommitted changes.
fn dirty_dot(theme: ThemeSpec) -> Element<'static, Message> {
    container(Space::new())
        .width(Length::Fixed(6.0))
        .height(Length::Fixed(6.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.accent)),
            border: Border {
                width: 0.0,
                color: Color::TRANSPARENT,
                radius: 3.0.into(),
            },
            ..container::Style::default()
        })
        .into()
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

    let scrim = mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| scrim_style()),
    )
    .on_press(Message::Action(Action::ConfirmCancel));

    let cancel = button(
        text("Cancel")
            .size(text_size::BODY)
            .color(theme.text)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([7, 16]))
    .on_press(Message::Action(Action::ConfirmCancel))
    .style(move |_, status| dialog_button_style(theme, status));

    // Red fill: the confirm runs a mutation the jj CLI refuses by default.
    let accept = button(
        text(dialog.confirm_label.as_str())
            .size(text_size::BODY)
            .color(theme.background)
            .font(ui.config.ui_font),
    )
    .padding(Padding::from([7, 16]))
    .on_press(Message::Action(Action::ConfirmAccept))
    .style(move |_, _| destructive_button_style(theme));

    let body = column![
        text(dialog.title.as_str())
            .size(text_size::TITLE)
            .color(theme.text)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
        text(dialog.body.as_str())
            .size(text_size::UI)
            .color(theme.muted_text)
            .font(ui.config.ui_font),
        row![Space::new().width(Length::Fill), cancel, accept]
            .spacing(8)
            .align_y(alignment::Vertical::Center),
    ]
    .spacing(12);

    let card = mouse_area(
        container(body)
            .width(Length::Fixed(460.0))
            .padding(Padding::from([20, 22]))
            .style(move |_| modal_style(theme)),
    )
    .on_press(Message::Ui(UiEvent::ConfirmNoOp));

    let centered = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .padding(Padding {
            top: DIALOG_TOP_OFFSET,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        });

    // `opaque` keeps wheel events and the cursor from bleeding through to the
    // shell below the modal (see `activity_popover` for the mechanics).
    opaque(stack![scrim, centered])
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

/// One quick-pick row in the open dialog's recent list: `owner/name` over the
/// home-contracted path, clicking it reopens that repo. Shared with the welcome
/// screen (`empty_state`) so both recent lists render identically.
pub(crate) fn recent_repo_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    root: &'a str,
) -> Element<'a, Message> {
    let (owner, name) = crate::repo_label(std::path::Path::new(root));
    let display_path = crate::contract_user_path(root);
    let mut label = row![].spacing(0).align_y(alignment::Vertical::Center);
    if !owner.is_empty() {
        label = label.push(
            text(format!("{owner}/"))
                .size(text_size::UI)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
        );
    }
    label = label.push(
        text(name)
            .size(text_size::UI)
            .color(theme.text)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
    );

    let identity = container(
        column![
            label,
            text(display_path)
                .size(text_size::CAPTION)
                .color(theme.subtle_text)
                .font(ui.config.mono_font)
                .wrapping(text::Wrapping::None),
        ]
        .spacing(2),
    )
    .width(Length::Fill)
    .clip(true);
    let folder = container(icons::icon(icons::FOLDER, 15.0, theme.info))
        .width(Length::Fixed(32.0))
        .height(Length::Fixed(32.0))
        .center(Length::Fixed(32.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(chip_background(theme.info))),
            border: Border {
                radius: radius::PUSH.into(),
                ..Border::default()
            },
            ..container::Style::default()
        });
    let disclosure = container(centered_control_content_fill(icons::icon(
        icons::CHEVRON_RIGHT,
        13.0,
        theme.subtle_text,
    )))
    .width(Length::Fixed(36.0))
    .height(Length::Fixed(36.0));
    let content = row![folder, identity, disclosure]
        .spacing(10)
        .align_y(alignment::Vertical::Center);

    let repo_button = button(
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_y(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fixed(52.0))
    .padding(Padding::from([0, 10]))
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
            radius: radius::BUTTON.into(),
        },
        shadow: Default::default(),
        snap: true,
    });
    let remove = button(centered_control_content_fill(icons::icon(
        icons::CLOSE,
        13.0,
        theme.subtle_text,
    )))
    .width(Length::Fixed(36.0))
    .height(Length::Fixed(52.0))
    .padding(0)
    .on_press(Message::Action(Action::RemoveRecentRepo(root.to_owned())))
    .style(move |_, status| {
        let mut style = ghost_button_style(theme, status);
        style.border.radius = radius::BUTTON.into();
        style
    });

    row![repo_button, remove]
        .spacing(2)
        .align_y(alignment::Vertical::Center)
        .into()
}

/// Modal overlay for opening a repository from a path. Returns an empty
/// `Space` when the dialog is closed. Mirrors the palette's scrim + card
/// construction so click-outside dismisses and the card itself doesn't.
pub fn build_open_repo_dialog(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(dialog) = ui.open_repo_dialog() else {
        return Space::new().into();
    };

    let scrim = mouse_area(
        container(Space::new().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| scrim_style()),
    )
    .on_press(Message::Action(Action::CloseRepoDialog));

    let input = iced::widget::text_input(
        "~/code/project, a GitHub PR URL, or owner/repo#123",
        &dialog.path,
    )
    .id(OPEN_REPO_INPUT_ID)
    .padding(Padding::from([9, 11]))
    .size(text_size::BODY)
    .font(ui.config.mono_font)
    .on_input(|path| Message::Ui(UiEvent::OpenRepoPathChanged(path)))
    .on_submit(Message::Action(Action::SubmitRepoDialog))
    .style(move |_, _| iced::widget::text_input::Style {
        // Recessed into the elevated card, otherwise identical to the
        // shared input identity.
        background: Background::Color(theme.background),
        ..input_style(theme)
    });

    let mark = container(centered_control_content(icons::icon(
        icons::FOLDER_OPEN,
        18.0,
        theme.info,
    )))
    .width(Length::Fixed(40.0))
    .height(Length::Fixed(40.0))
    .center(Length::Fixed(40.0))
    .style(move |_| container::Style {
        background: Some(Background::Color(chip_background(theme.info))),
        border: Border {
            radius: radius::SURFACE.into(),
            ..Border::default()
        },
        ..container::Style::default()
    });
    let intro = column![
        text("Open a repository")
            .size(text_size::TITLE)
            .color(theme.text)
            .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
        text("Open a local jj or Git working copy, or review a GitHub pull request.")
            .size(text_size::UI)
            .color(theme.muted_text)
            .font(ui.config.ui_font),
    ]
    .spacing(3);
    let header = row![mark, intro]
        .spacing(12)
        .align_y(alignment::Vertical::Center);

    let input_label = text("Repository path or pull request")
        .size(text_size::UI)
        .color(theme.text)
        .font(emphasis_font(ui.config.ui_font, Weight::Medium));
    let mut input_row = row![crate::input::single_line(input)]
        .spacing(8)
        .align_y(alignment::Vertical::Center);
    #[cfg(target_os = "macos")]
    {
        let choose_folder = button(centered_control_content(
            row![
                icons::icon(icons::FOLDER, 14.0, theme.muted_text),
                text("Choose folder")
                    .size(text_size::UI)
                    .font(ui.config.ui_font),
            ]
            .spacing(6)
            .align_y(alignment::Vertical::Center),
        ))
        .height(Length::Fixed(36.0))
        .padding([0, 12])
        .on_press(Message::Action(Action::ChooseRepoFolder))
        .style(move |_, status| dialog_button_style(theme, status));
        input_row = input_row.push(choose_folder);
    }

    let mut body = column![
        header,
        container(Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| container::Style::default().background(theme.border)),
        column![input_label, input_row].spacing(6),
    ]
    .spacing(16);

    if let Some(error) = &dialog.error {
        body = body.push(
            container(
                row![
                    icons::icon(icons::ALERT_TRIANGLE, 14.0, theme.removed_text),
                    text(error.as_str())
                        .size(text_size::UI)
                        .color(theme.removed_text)
                        .font(ui.config.ui_font)
                        .width(Length::Fill)
                        .wrapping(text::Wrapping::Word),
                ]
                .spacing(8)
                .align_y(alignment::Vertical::Center),
            )
            .width(Length::Fill)
            .padding([8, 10])
            .style(move |_| container::Style {
                background: Some(Background::Color(chip_background(theme.removed_text))),
                border: Border {
                    radius: radius::PUSH.into(),
                    ..Border::default()
                },
                ..container::Style::default()
            }),
        );
    }

    // Recent repositories quick-pick: anything in the MRU that isn't already
    // open. One click reopens it (no need to retype the path).
    let open_roots: Vec<&str> = ui
        .tabs
        .iter()
        .filter_map(|tab| tab.root()?.to_str())
        .collect();
    let recents: Vec<&String> = ui
        .recent_repos
        .iter()
        .filter(|root| !open_roots.contains(&root.as_str()))
        .take(5)
        .collect();
    if !recents.is_empty() {
        let clear = button(centered_control_content(
            text("Clear")
                .size(text_size::CAPTION)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
        ))
        .height(Length::Fill)
        .padding([0, 7])
        .on_press(Message::Action(Action::ClearRecentRepos))
        .style(move |_, status| ghost_button_style(theme, status));
        let recent_header = row![
            text("Recent repositories")
                .size(text_size::CAPTION)
                .color(theme.text)
                .font(emphasis_font(ui.config.ui_font, Weight::Medium)),
            Space::new().width(Length::Fill),
            clear,
        ]
        .height(Length::Fixed(32.0))
        .align_y(alignment::Vertical::Center);
        let mut list = column![].spacing(1);
        for root in recents {
            list = list.push(recent_repo_row(ui, theme, root));
        }
        body = body.push(
            column![
                recent_header,
                container(list)
                    .width(Length::Fill)
                    .padding(4)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(theme.panel_background)),
                        border: Border {
                            width: 1.0,
                            color: theme.border,
                            radius: radius::SURFACE.into(),
                        },
                        ..container::Style::default()
                    }),
            ]
            .spacing(6),
        );
    }

    let cancel = button(centered_control_content(
        text("Cancel")
            .size(text_size::BODY)
            .color(theme.text)
            .font(ui.config.ui_font),
    ))
    .height(Length::Fixed(34.0))
    .padding(Padding::from([0, 16]))
    .on_press(Message::Action(Action::CloseRepoDialog))
    .style(move |_, status| dialog_button_style(theme, status));

    let can_open = !dialog.path.trim().is_empty();
    let mut open = button(centered_control_content(
        text("Open")
            .size(text_size::BODY)
            .color(if can_open {
                theme.background
            } else {
                theme.subtle_text
            })
            .font(ui.config.ui_font),
    ))
    .height(Length::Fixed(34.0))
    .padding(Padding::from([0, 18]))
    .style(move |_, status| primary_button_style(theme, status));
    if can_open {
        open = open.on_press(Message::Action(Action::SubmitRepoDialog));
    }

    body = body.push(
        row![
            text(format!("{} to open anytime", crate::chrome::cmd_label("O")))
                .size(text_size::CAPTION)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
            Space::new().width(Length::Fill),
            cancel,
            open,
        ]
        .spacing(8)
        .align_y(alignment::Vertical::Center),
    );

    // Catch clicks on the card so they don't fall through to the scrim and
    // dismiss the dialog while the user is interacting with it.
    let card = mouse_area(
        container(body)
            .width(Length::Fixed(OPEN_DIALOG_WIDTH))
            .padding(Padding::from([22, 24]))
            .style(move |_| modal_style(theme)),
    )
    .on_press(Message::Ui(UiEvent::OpenRepoNoOp));

    let centered = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(alignment::Horizontal::Center)
        .padding(Padding {
            top: DIALOG_TOP_OFFSET,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        });

    // `opaque` keeps wheel events and the cursor from bleeding through to the
    // shell below the modal (see `activity_popover` for the mechanics).
    opaque(stack![scrim, centered])
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

//! Word/line drag-extension for `text_editor`s, matching the diff view's
//! multi-click selection: double-click-drag grows the selection by words,
//! triple-click-drag by lines.
//!
//! iced's `text_editor` gets halfway there — a double click publishes
//! `Action::SelectWord`, a triple `Action::SelectLine`, and cosmic-text keeps
//! that selection *mode* alive while `Action::Drag` moves the cursor, expanding
//! the bounds word- or line-wise for free. But the widget's event mapping only
//! emits `Drag` while the last click was a `Single`, so a double/triple-click
//! drag goes nowhere. Rather than fork the widget, [`EditorDragArea`] wraps it,
//! runs the same `mouse::Click` state machine on the same events (identical
//! thresholds, so the two never disagree on a click's kind), and publishes the
//! `Drag` actions the widget refuses to — straight into the caller's normal
//! `on_action` pipeline, where they land in the shared `Content` the widget
//! renders from.

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::{
    Element, Event, Length, Padding, Point, Rectangle, Size, Theme, Vector,
    widget::text_editor::Action,
};

/// Wrap `content` (a `text_editor` with `on_action` wired) so double/triple
/// -click drags extend its selection by word/line, publishing the synthesized
/// [`Action::Drag`]s through `on_action`. `padding` must match the editor's
/// own — action positions are editor-content-relative, and the wrapper can't
/// read the child's padding back out of the `Element`.
pub(crate) fn editor_drag_area<'a, Message>(
    content: impl Into<Element<'a, Message, Theme>>,
    padding: impl Into<Padding>,
    on_action: impl Fn(Action) -> Message + 'a,
) -> EditorDragArea<'a, Message> {
    EditorDragArea {
        content: content.into(),
        padding: padding.into(),
        on_action: Box::new(on_action),
    }
}

pub(crate) struct EditorDragArea<'a, Message> {
    content: Element<'a, Message, Theme>,
    padding: Padding,
    on_action: Box<dyn Fn(Action) -> Message + 'a>,
}

/// The wrapper's own click bookkeeping — a shadow of the `text_editor`'s
/// private `last_click`/`drag_click`. Both machines feed `mouse::Click::new`
/// the same positions off the same event stream, so their click kinds match.
#[derive(Debug, Clone, Copy, Default)]
struct DragState {
    last_click: Option<mouse::Click>,
    drag_kind: Option<mouse::click::Kind>,
}

impl<Message> Widget<Message, Theme, iced::Renderer> for EditorDragArea<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<DragState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(DragState::default())
    }

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
        // The editor reacts first (focus, its own click handling); the shadow
        // tracker then observes the same event. No `is_event_captured` gate —
        // the editor itself has none for clicks, and skipping here while it
        // processed would desync the two click counters.
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            shell,
            viewport,
        );

        let state = tree.state.downcast_mut::<DragState>();
        let bounds = layout.bounds();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                // Same gate as the editor: only a press whose cursor position
                // resolves inside its bounds counts as a click on it.
                if let Some(position) = cursor.position_in(bounds) {
                    let position = position - Vector::new(self.padding.left, self.padding.top);
                    let click =
                        mouse::Click::new(position, mouse::Button::Left, state.last_click);
                    state.drag_kind = Some(click.kind());
                    state.last_click = Some(click);
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.drag_kind = None;
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                // The editor emits its own char-wise `Drag` for single-click
                // sweeps; only the kinds it drops on the floor are filled in.
                if matches!(
                    state.drag_kind,
                    Some(mouse::click::Kind::Double | mouse::click::Kind::Triple)
                ) && let Some(position) = cursor.position()
                {
                    // Clamp into the editor so overshooting an edge keeps
                    // extending to the boundary unit instead of freezing —
                    // the same feel as the diff view's drag.
                    let clamped = Point::new(
                        position.x.clamp(bounds.x, bounds.x + bounds.width - 1.0),
                        position.y.clamp(bounds.y, bounds.y + bounds.height - 1.0),
                    );
                    let local = clamped
                        - Vector::new(bounds.x + self.padding.left, bounds.y + self.padding.top);
                    shell.publish((self.on_action)(Action::Drag(local)));
                }
            }
            _ => {}
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
        translation: Vector,
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

impl<'a, Message: 'a> From<EditorDragArea<'a, Message>> for Element<'a, Message, Theme> {
    fn from(area: EditorDragArea<'a, Message>) -> Self {
        Element::new(area)
    }
}

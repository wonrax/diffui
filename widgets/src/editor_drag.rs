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
//!
//! It also brings scrolling to the speed of the app's other scroll views. The
//! widget turns a pixel delta into `delta / 4` *lines*, so a 20px-line editor
//! moved five times as far as the finger did; the wrapper rescales deltas so
//! the text moves as many points as [`crate::wheel`] moves everything else.

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::{
    Element, Event, Length, Padding, Point, Rectangle, Size, Theme, Vector,
    widget::text_editor::Action,
};

/// The pixels-per-line the stock editor assumes when it turns a pixel
/// delta into lines.
const STOCK_PIXELS_PER_LINE: f32 = 4.0;

/// Wrap `content` (a `text_editor` with `on_action` wired) so double/triple
/// -click drags extend its selection by word/line, publishing the synthesized
/// [`Action::Drag`]s through `on_action`. `padding` must match the editor's
/// own — action positions are editor-content-relative, and the wrapper can't
/// read the child's padding back out of the `Element`. `line_height` is the
/// editor's line box in pixels, for the scroll-speed correction.
pub fn editor_drag_area<'a, Message>(
    content: impl Into<Element<'a, Message, Theme>>,
    padding: impl Into<Padding>,
    line_height: f32,
    on_action: impl Fn(Action) -> Message + 'a,
) -> EditorDragArea<'a, Message> {
    EditorDragArea {
        content: content.into(),
        padding: padding.into(),
        line_height,
        on_action: Box::new(on_action),
    }
}

pub struct EditorDragArea<'a, Message> {
    content: Element<'a, Message, Theme>,
    padding: Padding,
    line_height: f32,
    on_action: Box<dyn Fn(Action) -> Message + 'a>,
}

/// `delta` rescaled so the stock editor, which reads a pixel delta as
/// `delta / 4` lines, scrolls the text as many points as
/// [`crate::wheel::movement`] says.
fn rescale_wheel(delta: mouse::ScrollDelta, line_height: f32) -> mouse::ScrollDelta {
    let factor = STOCK_PIXELS_PER_LINE / line_height.max(1.0);
    let points = crate::wheel::movement(delta);
    mouse::ScrollDelta::Pixels {
        x: -points.x * factor,
        y: -points.y * factor,
    }
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

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
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
        viewport: &Rectangle,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout,
            viewport,
            renderer,
            operation,
        );
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
        let rescaled;
        let event = match event {
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                rescaled = Event::Mouse(mouse::Event::WheelScrolled {
                    delta: rescale_wheel(*delta, self.line_height),
                });
                &rescaled
            }
            _ => event,
        };
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
                    let click = mouse::Click::new(position, mouse::Button::Left, state.last_click);
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
    ) -> Vec<overlay::Element<'b, Message, Theme, iced::Renderer>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The stock editor's own conversion, so the test checks the outcome
    /// (pixels of text moved) rather than the factor.
    fn stock_lines(delta: mouse::ScrollDelta) -> f32 {
        match delta {
            mouse::ScrollDelta::Pixels { y, .. } => -y / STOCK_PIXELS_PER_LINE,
            mouse::ScrollDelta::Lines { y, .. } => -y * 4.0,
        }
    }

    #[test]
    fn the_editor_scrolls_as_far_as_every_other_scroll_view() {
        let line_height = 18.2;
        for delta in [
            mouse::ScrollDelta::Pixels { x: 0.0, y: -100.0 },
            mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
        ] {
            let moved = stock_lines(rescale_wheel(delta, line_height)) * line_height;
            let expected = crate::wheel::movement(delta).y;
            assert!((moved - expected).abs() < 0.01, "{moved} vs {expected}");
        }
    }
}

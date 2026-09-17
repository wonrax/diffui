//! The app's single-line text input: iced's `text_input`, with presses in its
//! vertical padding resolved by x.
//!
//! The stock widget hit-tests through a multi-line editor, where a press above
//! the first line means "start of text" and one below the last means "end". A
//! one-line field is mostly padding, so those bands cover a third of its
//! height and a slightly high click throws the caret to index 0. Native fields
//! place the caret by x wherever the press lands vertically; this wrapper does
//! the same by pinning the cursor's y to the text line before the stock widget
//! sees it. A drag that leaves the field still selects to the start or end,
//! as it does natively.
//!
//! This replaces the `text_input` vendored from iced `4255f61` for its missing
//! stateful horizontal scroll, which upstream has since grown.

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::widget::TextInput;
use iced::{Element, Event, Length, Point, Rectangle, Size, Theme, Vector};

pub(crate) fn single_line<'a, Message: Clone + 'a>(
    input: TextInput<'a, Message>,
) -> Element<'a, Message> {
    Element::new(SingleLine {
        content: input.into(),
    })
}

struct SingleLine<'a, Message> {
    content: Element<'a, Message>,
}

/// `cursor`, moved onto the text line when it is over `bounds`. The line is
/// vertically centered by the input's padding, so the center is always on it.
fn on_line(cursor: mouse::Cursor, bounds: Rectangle) -> mouse::Cursor {
    match cursor.position_over(bounds) {
        Some(point) => mouse::Cursor::Available(Point::new(point.x, bounds.center_y())),
        None => cursor,
    }
}

impl<Message> Widget<Message, Theme, iced::Renderer> for SingleLine<'_, Message> {
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn diff(&mut self, tree: &mut Tree) {
        self.content.as_widget_mut().diff(tree);
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
        self.content.as_widget_mut().layout(tree, renderer, limits)
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
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, viewport, renderer, operation);
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
            tree,
            event,
            layout,
            on_line(cursor, layout.bounds()),
            renderer,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Vec<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

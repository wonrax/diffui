//! The app's single-line text input: iced's `text_input`, with presses in its
//! vertical padding resolved by x, and its font kept across programmatic
//! value changes.
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
//! The font: when the value changes from outside (a restored revset, a picked
//! preset, a palette scope pill, a chosen folder) the stock widget rewrites
//! its editor buffer with default attributes — the platform sans — and only
//! re-applies its own font when that font *changes*. So a mono field showed
//! typed text in mono and every other value in sans. On such a change this
//! wrapper lays the input out once with a stand-in font before the real one,
//! which makes the editor re-apply it.
//!
//! This replaces the `text_input` vendored from iced `4255f61` for its missing
//! stateful horizontal scroll, which upstream has since grown.

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::widget::TextInput;
use iced::{Element, Event, Font, Length, Point, Rectangle, Size, Theme, Vector};

/// `input` must have been built with `value` and `font` — the wrapper reads
/// them back from here because the stock widget doesn't expose them.
pub(crate) fn single_line<'a, Message: Clone + 'a>(
    input: TextInput<'a, Message>,
    value: &str,
    font: Font,
) -> Element<'a, Message> {
    Element::new(SingleLine {
        input: Some(input),
        value: value.to_owned(),
        font,
    })
}

struct SingleLine<'a, Message> {
    /// Always `Some` outside of [`SingleLine::layout`], which briefly takes it
    /// to swap the font (the builder consumes the widget).
    input: Option<TextInput<'a, Message>>,
    value: String,
    font: Font,
}

/// The value the input was last laid out with.
#[derive(Default)]
struct Seen(Option<String>);

impl<'a, Message: Clone + 'a> SingleLine<'a, Message> {
    fn input(&self) -> &dyn Widget<Message, Theme, iced::Renderer> {
        self.input
            .as_ref()
            .expect("input is present outside layout")
    }

    fn input_mut(&mut self) -> &mut (dyn Widget<Message, Theme, iced::Renderer> + 'a) {
        self.input
            .as_mut()
            .expect("input is present outside layout")
    }

    fn set_font(&mut self, font: Font) {
        self.input = self.input.take().map(|input| input.font(font));
    }
}

/// `cursor`, moved onto the text line when it is over `bounds`. The line is
/// vertically centered by the input's padding, so the center is always on it.
fn on_line(cursor: mouse::Cursor, bounds: Rectangle) -> mouse::Cursor {
    match cursor.position_over(bounds) {
        Some(point) => mouse::Cursor::Available(Point::new(point.x, bounds.center_y())),
        None => cursor,
    }
}

impl<'a, Message: Clone + 'a> Widget<Message, Theme, iced::Renderer> for SingleLine<'a, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Seen>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Seen::default())
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut [self.input_mut()]);
    }

    fn size(&self) -> Size<Length> {
        self.input().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let seen = tree.state.downcast_mut::<Seen>();
        let changed = seen.0.as_deref() != Some(self.value.as_str());
        if changed {
            seen.0 = Some(self.value.clone());
            // Any font but the real one: the editor only re-applies its font
            // when it differs from the last one it was laid out with.
            let stand_in = if self.font == Font::MONOSPACE {
                Font::DEFAULT
            } else {
                Font::MONOSPACE
            };
            self.set_font(stand_in);
            let _ = self
                .input_mut()
                .layout(&mut tree.children[0], renderer, limits);
            self.set_font(self.font);
        }
        let node = self
            .input_mut()
            .layout(&mut tree.children[0], renderer, limits);
        layout::Node::with_children(node.size(), vec![node])
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
        self.input().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            child(layout),
            cursor,
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.input_mut().operate(
            &mut tree.children[0],
            child(layout),
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
        self.input_mut().update(
            &mut tree.children[0],
            event,
            child(layout),
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
        self.input()
            .mouse_interaction(&tree.children[0], child(layout), cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Vec<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.input_mut().overlay(
            &mut tree.children[0],
            child(layout),
            renderer,
            viewport,
            translation,
        )
    }
}

/// The input's own layout inside the wrapper's.
fn child(layout: Layout<'_>) -> Layout<'_> {
    layout.children().next().expect("the input's layout")
}

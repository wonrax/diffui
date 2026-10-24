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
//! The keys: the stock widget takes no key binding, so the macOS ⌃ chords
//! every native field honours (⌃A, ⌃E, ⌃K, …) reach it rewritten as the named
//! keys they stand for ([`crate::ui::cocoa_key_event`]).
//!
//! This replaces the `text_input` vendored from iced `4255f61` for its missing
//! stateful horizontal scroll, which upstream has since grown.

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::widget::{TextInput, text, text_editor};
use iced::{Element, Event, Font, Length, Padding, Point, Rectangle, Size, Theme, Vector};

use crate::theme::ThemeSpec;

/// `input` must have been built with `value` and `font` — the wrapper reads
/// them back from here because the stock widget doesn't expose them.
pub fn single_line<'a, Message: Clone + 'a>(
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
        let cocoa = crate::ui::cocoa_key_event(event);
        self.input_mut().update(
            &mut tree.children[0],
            cocoa.as_ref().unwrap_or(event),
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

/// Inset of a [`multi_line`] editor's text from its border.
pub const EDITOR_PADDING: Padding = Padding {
    top: 10.0,
    right: 12.0,
    bottom: 10.0,
    left: 12.0,
};

/// The app's multi-line editor (revision descriptions, the split prompt):
/// the code font at the code size, word wrap, the input outline, ⌘⏎ left to
/// the form ([`crate::ui::editor_key_binding`]), and word/line drag selection
/// ([`crate::editor_drag`]). `on_action` is `None` while the text is being
/// saved, which leaves the editor read-only.
pub fn multi_line<'a, Message: Clone + 'a>(
    content: &'a text_editor::Content,
    spec: MultiLine,
    theme: ThemeSpec,
    on_action: Option<impl Fn(text_editor::Action) -> Message + Clone + 'a>,
) -> Element<'a, Message> {
    let mut editor = text_editor(content)
        .key_binding(crate::ui::editor_key_binding)
        .placeholder(spec.placeholder)
        .size(spec.size)
        .font(spec.font)
        .line_height(text::LineHeight::Relative(
            crate::measure::LINE_HEIGHT_MULTIPLIER,
        ))
        .height(Length::Fixed(spec.height))
        .padding(EDITOR_PADDING)
        .wrapping(text::Wrapping::WordOrGlyph)
        .style(move |_, _| editor_style(theme));
    if let Some(id) = spec.id {
        editor = editor.id(iced::widget::Id::new(id));
    }
    match on_action {
        Some(on_action) => {
            editor = editor.on_action(on_action.clone());
            let line_height = spec.size * crate::measure::LINE_HEIGHT_MULTIPLIER;
            crate::editor_drag::editor_drag_area(editor, EDITOR_PADDING, line_height, on_action)
                .into()
        }
        None => editor.into(),
    }
}

/// What varies between [`multi_line`] editors.
pub struct MultiLine {
    pub id: Option<&'static str>,
    pub placeholder: &'static str,
    pub font: Font,
    pub size: f32,
    /// Fixed, usually from [`fit_height`] so the box grows with its text.
    pub height: f32,
}

/// The height that shows all of `content` wrapped at `width` (the editor's
/// outer width), between `min` and `max`; past `max` the editor scrolls.
pub fn fit_height(content: &str, font: Font, size: f32, width: f32, min: f32, max: f32) -> f32 {
    let line_height = (size * crate::measure::LINE_HEIGHT_MULTIPLIER).max(1.0);
    let text_width = (width - EDITOR_PADDING.left - EDITOR_PADDING.right).max(1.0);
    let lines = crate::measure::wrapped_line_count(content, size, font, line_height, text_width);
    let natural = (lines as f32 * line_height).ceil() + EDITOR_PADDING.top + EDITOR_PADDING.bottom;
    natural.clamp(min, max.max(min))
}

/// The tallest a [`multi_line`] editor grows before it scrolls: room for a
/// commit message's body, never most of the window.
pub fn editor_max_height(window_height: f32) -> f32 {
    (window_height * 0.45).clamp(160.0, 420.0)
}

/// The multi-line editor's box: exactly the single-line input's.
pub fn editor_style(theme: ThemeSpec) -> text_editor::Style {
    let input = crate::theme::input_style(theme);
    text_editor::Style {
        background: input.background,
        border: input.border,
        placeholder: input.placeholder,
        value: input.value,
        selection: input.selection,
    }
}

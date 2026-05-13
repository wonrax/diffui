//! Transparent overlay that handles drag-to-resize for a vertical divider.
//!
//! It draws nothing — the visible 1px line is the static divider sitting
//! beneath it inside the panel `row!`. The overlay's only job is to catch
//! mouse events in a horizontal band around the divider's x position, which
//! is wider than the line itself so the user doesn't have to land on a
//! single pixel to grab.
//!
//! Sits inside an `iced::widget::stack!` on top of the panels. Stack
//! delivers events to the topmost child first, so the overlay can capture
//! near the seam before the `RevisionList` underneath would otherwise
//! interpret the press as a row click.

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use iced::{Element, Event, Length, Rectangle, Size, Theme};

pub struct ResizeHandle<Message> {
    handle_x: f32,
    min_width: f32,
    hit_padding: f32,
    on_resize: fn(f32) -> Message,
}

impl<Message> ResizeHandle<Message> {
    pub fn new(
        handle_x: f32,
        min_width: f32,
        hit_padding: f32,
        on_resize: fn(f32) -> Message,
    ) -> Self {
        Self {
            handle_x,
            min_width,
            hit_padding,
            on_resize,
        }
    }

    fn hit_band(&self, bounds: Rectangle) -> Rectangle {
        let center_x = bounds.x + self.handle_x;
        Rectangle {
            x: center_x - self.hit_padding,
            y: bounds.y,
            width: self.hit_padding * 2.0,
            height: bounds.height,
        }
    }
}

#[derive(Default)]
struct State {
    drag: Option<DragState>,
}

struct DragState {
    start_cursor_x: f32,
    start_handle_x: f32,
}

impl<Message, Renderer> Widget<Message, Theme, Renderer> for ResizeHandle<Message>
where
    Renderer: renderer::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(Length::Fill, Length::Fill, Size::ZERO))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State>();
        let band = self.hit_band(bounds);

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(point) = cursor.position() else {
                    return;
                };
                if !band.contains(point) {
                    return;
                }
                state.drag = Some(DragState {
                    start_cursor_x: point.x,
                    start_handle_x: self.handle_x,
                });
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let Some(drag) = state.drag.as_ref() else {
                    return;
                };
                let delta = position.x - drag.start_cursor_x;
                let new_width = (drag.start_handle_x + delta).max(self.min_width);
                if (new_width - self.handle_x).abs() > f32::EPSILON {
                    shell.publish((self.on_resize)(new_width));
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.drag.take().is_some() {
                    shell.capture_event();
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
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        if state.drag.is_some() {
            return mouse::Interaction::ResizingHorizontally;
        }
        let Some(point) = cursor.position() else {
            return mouse::Interaction::None;
        };
        if self.hit_band(layout.bounds()).contains(point) {
            mouse::Interaction::ResizingHorizontally
        } else {
            // Falling through to None lets `stack`'s mouse_interaction descend
            // to the row underneath so panels keep their normal cursors.
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        _tree: &Tree,
        _renderer: &mut Renderer,
        _theme: &Theme,
        _renderer_style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        // Visual divider lives in the row beneath us; the overlay is invisible.
    }
}

impl<Message: 'static, Renderer> From<ResizeHandle<Message>>
    for Element<'_, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer + 'static,
{
    fn from(widget: ResizeHandle<Message>) -> Self {
        Element::new(widget)
    }
}

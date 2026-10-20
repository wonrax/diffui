//! One scroll speed for every scroll view.
//!
//! winit hands a trackpad's precise deltas over in *physical* pixels (on
//! macOS it multiplies AppKit's points by the window's scale factor), while
//! every widget scrolls in logical points, so unscaled content would run at
//! twice the finger's speed on a Retina screen. A mouse wheel's notches
//! arrive as lines, which need one shared distance.
//!
//! [`normalized`] wraps the whole window content and rewrites every
//! `WheelScrolled` before any widget or overlay sees it: pixel deltas are
//! divided by the scale factor (content follows the finger 1:1) and line
//! deltas become [`POINTS_PER_LINE`] points each. Widgets downstream only
//! ever see logical [`mouse::ScrollDelta::Pixels`], which iced's `scrollable`
//! already applies 1:1; the custom scroll views read them through
//! [`movement`].

use iced::advanced::{
    Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree},
};
use iced::{Element, Event, Length, Rectangle, Size, Theme, Vector};

/// Points one wheel line scrolls. macOS reports a slow notch as one line and
/// accelerates faster spins into several, so this is the slow-notch step:
/// WebKit's and Chromium's per-line distance on macOS, about two diff rows
/// (AppKit's own text views step 10pt, its tables one row). Elsewhere a notch
/// is one unaccelerated line, and Chromium's 100 (three lines) applies.
pub(crate) const POINTS_PER_LINE: f32 = if cfg!(target_os = "macos") {
    40.0
} else {
    100.0
};

/// `delta` as logical points: pixel deltas divided by the window's scale
/// factor, line deltas at [`POINTS_PER_LINE`] each.
pub(crate) fn normalize(delta: mouse::ScrollDelta, scale_factor: f32) -> mouse::ScrollDelta {
    match delta {
        mouse::ScrollDelta::Pixels { x, y } => {
            let scale = if scale_factor > 0.0 {
                scale_factor
            } else {
                1.0
            };
            mouse::ScrollDelta::Pixels {
                x: x / scale,
                y: y / scale,
            }
        }
        mouse::ScrollDelta::Lines { x, y } => mouse::ScrollDelta::Pixels {
            x: x * POINTS_PER_LINE,
            y: y * POINTS_PER_LINE,
        },
    }
}

/// How far content moves for a normalized `delta`, in points: the opposite
/// of the gesture, as iced's `scrollable` does it.
pub(crate) fn movement(delta: mouse::ScrollDelta) -> Vector {
    match normalize(delta, 1.0) {
        mouse::ScrollDelta::Pixels { x, y } | mouse::ScrollDelta::Lines { x, y } => {
            Vector::new(-x, -y)
        }
    }
}

/// Wrap the window's `content` so every wheel event it and its overlays see
/// is normalized for `scale_factor`.
pub(crate) fn normalized<'a, Message: 'a>(
    content: impl Into<Element<'a, Message, Theme>>,
    scale_factor: f32,
) -> Element<'a, Message, Theme> {
    Element::new(Normalized {
        content: content.into(),
        scale_factor,
    })
}

fn rewrite(event: &Event, scale_factor: f32) -> Option<Event> {
    match event {
        Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
            Some(Event::Mouse(mouse::Event::WheelScrolled {
                delta: normalize(*delta, scale_factor),
            }))
        }
        _ => None,
    }
}

struct Normalized<'a, Message> {
    content: Element<'a, Message, Theme>,
    scale_factor: f32,
}

impl<Message> Widget<Message, Theme, iced::Renderer> for Normalized<'_, Message> {
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
        let rewritten = rewrite(event, self.scale_factor);
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            rewritten.as_ref().unwrap_or(event),
            layout,
            cursor,
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
        let scale_factor = self.scale_factor;
        self.content
            .as_widget_mut()
            .overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            )
            .into_iter()
            .map(|content| wrap_overlay(content, scale_factor))
            .collect()
    }
}

fn wrap_overlay<'a, Message: 'a>(
    content: overlay::Element<'a, Message, Theme, iced::Renderer>,
    scale_factor: f32,
) -> overlay::Element<'a, Message, Theme, iced::Renderer> {
    overlay::Element::new(Box::new(NormalizedOverlay {
        content,
        scale_factor,
    }))
}

/// Overlays (menus, pick lists, tooltips) get their events from the runtime
/// directly, not through the widget that spawned them, so they're wrapped too.
struct NormalizedOverlay<'a, Message> {
    content: overlay::Element<'a, Message, Theme, iced::Renderer>,
    scale_factor: f32,
}

impl<Message> overlay::Overlay<Message, Theme, iced::Renderer> for NormalizedOverlay<'_, Message> {
    fn layout(&mut self, renderer: &iced::Renderer, bounds: Size) -> layout::Node {
        self.content.as_overlay_mut().layout(renderer, bounds)
    }

    fn draw(
        &self,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.content
            .as_overlay()
            .draw(renderer, theme, style, layout, cursor);
    }

    fn operate(
        &mut self,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_overlay_mut()
            .operate(layout, renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        shell: &mut Shell<'_, Message>,
    ) {
        let rewritten = rewrite(event, self.scale_factor);
        self.content.as_overlay_mut().update(
            rewritten.as_ref().unwrap_or(event),
            layout,
            cursor,
            renderer,
            shell,
        );
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_overlay()
            .mouse_interaction(layout, cursor, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
    ) -> Vec<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        let scale_factor = self.scale_factor;
        self.content
            .as_overlay_mut()
            .overlay(layout, renderer)
            .into_iter()
            .map(|content| wrap_overlay(content, scale_factor))
            .collect()
    }

    fn index(&self) -> f32 {
        self.content.as_overlay().index()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retina_trackpad_delta_moves_content_with_the_finger() {
        // AppKit reported 10pt; winit doubled it to physical pixels.
        let delta = mouse::ScrollDelta::Pixels { x: 6.0, y: -20.0 };
        assert_eq!(
            normalize(delta, 2.0),
            mouse::ScrollDelta::Pixels { x: 3.0, y: -10.0 }
        );
        assert_eq!(movement(normalize(delta, 2.0)), Vector::new(-3.0, 10.0));
    }

    #[test]
    fn a_standard_display_trackpad_delta_passes_through() {
        let delta = mouse::ScrollDelta::Pixels { x: 0.0, y: -20.0 };
        assert_eq!(normalize(delta, 1.0), delta);
    }

    #[test]
    fn a_wheel_notch_scrolls_the_shared_step_at_any_scale() {
        let notch = mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 };
        for scale in [1.0, 2.0] {
            assert_eq!(
                normalize(notch, scale),
                mouse::ScrollDelta::Pixels {
                    x: 0.0,
                    y: -POINTS_PER_LINE
                }
            );
        }
        let sideways = mouse::ScrollDelta::Lines { x: 2.0, y: 0.0 };
        assert_eq!(movement(sideways), Vector::new(-2.0 * POINTS_PER_LINE, 0.0));
    }

    #[test]
    fn normalizing_twice_changes_nothing_more() {
        let notch = normalize(mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 }, 2.0);
        assert_eq!(movement(notch), Vector::new(0.0, -POINTS_PER_LINE));
    }
}

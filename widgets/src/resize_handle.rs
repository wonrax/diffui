//! Drag-to-resize for a vertical divider.
//!
//! It draws nothing: the visible 1px line is the divider beneath it in the
//! panel row. It catches the mouse in a band around the divider's x position,
//! wider than the line itself so the user doesn't have to land on a single
//! pixel to grab, and it sits above the panels so a press near the seam reaches
//! it before the row underneath could read it as a click.
//!
//! Dragging uses GPUI's drag machinery rather than tracking the button by
//! hand, so a release outside the window still ends the drag.

use std::rc::Rc;

use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, CursorStyle, DragMoveEvent, ElementId, IntoElement, Render, Window,
    div, px,
};

/// How much of the window the diff pane keeps when the sidebar is dragged
/// wide. Roughly the width of a narrow hunk plus its gutter — enough that the
/// pane is still readable rather than a sliver.
pub const MIN_DIFF_PANE_WIDTH: f32 = 240.0;

/// Clamp a sidebar width to what the window can actually show: at least
/// `min_width`, and never so wide that the diff pane drops below
/// [`MIN_DIFF_PANE_WIDTH`]. Without the ceiling an overshooting drag (or
/// reopening on a smaller display) leaves the diff pane at zero width with the
/// grab band off-screen, and that width is what gets persisted. The floor wins
/// on a window too narrow to honour both.
pub fn clamp_width(width: f32, min_width: f32, window_width: f32) -> f32 {
    clamp_width_with_reserve(width, min_width, window_width, MIN_DIFF_PANE_WIDTH)
}

pub fn clamp_width_with_reserve(
    width: f32,
    min_width: f32,
    window_width: f32,
    right_reserve: f32,
) -> f32 {
    let max_width = (window_width - right_reserve).max(min_width);
    width.clamp(min_width, max_width)
}

/// What a drag carries: which divider it came from, so a band only answers its
/// own drag.
#[derive(Clone)]
struct ResizeDrag(ElementId);

/// The empty view GPUI requires a drag to have.
struct NoDragPreview;

impl Render for NoDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// An overlay for a divider `handle_x` px into its parent, which it covers;
/// the parent must be `relative`, and its left edge is where widths are
/// measured from. `on_resize` gets the width under the cursor, clamped so the
/// pane keeps `min_width` and the pane to its right keeps `right_reserve`.
type OnResize = Rc<dyn Fn(f32, &mut Window, &mut App)>;

pub struct ResizeHandle {
    id: ElementId,
    handle_x: f32,
    min_width: f32,
    right_reserve: f32,
    hit_padding: f32,
    grabbable: bool,
    on_resize: OnResize,
}

impl ResizeHandle {
    pub fn new(
        id: impl Into<ElementId>,
        handle_x: f32,
        min_width: f32,
        hit_padding: f32,
        on_resize: impl Fn(f32, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            handle_x,
            min_width,
            right_reserve: MIN_DIFF_PANE_WIDTH,
            hit_padding,
            grabbable: true,
            on_resize: Rc::new(on_resize),
        }
    }

    pub fn right_reserve(mut self, width: f32) -> Self {
        self.right_reserve = width;
        self
    }

    /// Whether a press can start a drag here. A collapsed pane's handle keeps
    /// listening without a band to grab, so a drag that collapsed it can open
    /// it again before the button comes up.
    pub fn grabbable(mut self, grabbable: bool) -> Self {
        self.grabbable = grabbable;
        self
    }

    pub fn view(self) -> AnyElement {
        let drag = ResizeDrag(self.id.clone());
        let own = self.id.clone();
        let (min_width, reserve) = (self.min_width, self.right_reserve);
        let on_resize = self.on_resize;
        div()
            .id((self.id.clone(), "overlay"))
            .absolute()
            .inset_0()
            .on_drag_move(move |event: &DragMoveEvent<ResizeDrag>, window, cx| {
                if event.drag(cx).0 != own {
                    return;
                }
                let bounds = event.bounds;
                let width = f32::from(event.event.position.x - bounds.origin.x);
                let clamped = clamp_width_with_reserve(
                    width,
                    min_width,
                    f32::from(bounds.size.width),
                    reserve,
                );
                on_resize(clamped, window, cx);
            })
            .when(self.grabbable, |overlay| {
                overlay.child(
                    div()
                        .id(self.id)
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(self.handle_x - self.hit_padding))
                        .w(px(self.hit_padding * 2.0))
                        .cursor(CursorStyle::ResizeLeftRight)
                        .on_drag(drag, |_, _, _, cx| cx.new(|_| NoDragPreview))
                        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation()
                        }),
                )
            })
            .into_any_element()
    }
}

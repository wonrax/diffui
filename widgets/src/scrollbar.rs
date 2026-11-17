//! The scrollbar of the views that scroll themselves (the diff, the graph, the
//! file tree): kit's own bar, the one every plain scroll view gets, fed through
//! [`Shared`]. [`place_kit_bars`] puts every kit bar in the app where these views
//! keep text clear of it.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::base::{ScrollbarHandle, ScrollbarTheme, Theme};
use gpui_kit::{App, Bounds, Pixels, Point as GpuiPoint, Size, point, px, size};

use crate::geometry::{Point, Rectangle};

/// How far from a scroll view's right edge its content keeps text: the bar's
/// whole hover zone, so text never sits beneath it. Row washes and backgrounds
/// still run under the bar. Every scroll view leaves this gutter.
pub const GUTTER: f32 = 12.0;
/// The thumb's gap from the view's right edge.
const INSET: f32 = 2.0;

/// Places kit's bars: within [`GUTTER`], [`INSET`] off the edge. Kit's own
/// theme sync rewrites these, so this runs after each one.
pub fn place_kit_bars(cx: &mut App) {
    let base = Theme::global_mut(cx);
    let bar = base.scrollbar.clone();
    base.scrollbar = ScrollbarTheme::new()
        .with_mode(bar.mode())
        .with_motion(bar.motion())
        .with_styles(
            bar.styles()
                .clone()
                .track(|track| track.width(px(GUTTER)))
                .track_hover(|track| track.width(px(GUTTER)))
                .track_active(|track| track.width(px(GUTTER)))
                .thumb(|thumb| thumb.inset(px(INSET)))
                .thumb_hover(|thumb| thumb.inset(px(INSET)))
                .thumb_active(|thumb| thumb.inset(px(INSET))),
        );
}

/// Whether `cursor` is over the bar's zone: nothing under it takes hover or a
/// text cursor.
pub fn hits_container(bounds: Rectangle, cursor: Point, content_height: f32) -> bool {
    if content_height <= bounds.height {
        return false;
    }
    let zone = Rectangle {
        x: bounds.x + bounds.width - GUTTER,
        y: bounds.y,
        width: GUTTER,
        height: bounds.height,
    };
    zone.contains(cursor)
}

/// What kit's bar reads from a view that owns its offset, and how a drag on the
/// bar reaches it. The view [`publishes`](Shared::publish) its viewport and
/// offset each frame and [takes](Shared::take_requested) a dragged-to offset
/// before laying out; kit redraws the view that holds the bar.
#[derive(Clone, Default)]
pub struct Shared(Rc<RefCell<SharedState>>);

#[derive(Default)]
struct SharedState {
    viewport: Bounds<Pixels>,
    content_height: f32,
    offset: f32,
    requested: Option<f32>,
}

impl Shared {
    /// What this frame shows: the viewport, the content's height, and how far
    /// down it is scrolled.
    pub fn publish(&self, viewport: Rectangle, content_height: f32, offset: f32) {
        let mut state = self.0.borrow_mut();
        state.viewport = viewport.bounds();
        state.content_height = content_height;
        state.offset = offset;
    }

    /// The offset a drag on the bar asked for since the last frame.
    pub fn take_requested(&self) -> Option<f32> {
        self.0.borrow_mut().requested.take()
    }
}

impl ScrollbarHandle for Shared {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().viewport
    }

    /// GPUI's convention: the offset goes negative as content scrolls up.
    fn offset(&self) -> GpuiPoint<Pixels> {
        point(px(0.0), px(-self.0.borrow().offset))
    }

    fn set_offset(&self, offset: GpuiPoint<Pixels>) {
        let mut state = self.0.borrow_mut();
        let max = (state.content_height - f32::from(state.viewport.size.height)).max(0.0);
        let offset = (-f32::from(offset.y)).clamp(0.0, max);
        // The bar reads its thumb from here before the view's next frame.
        state.offset = offset;
        state.requested = Some(offset);
    }

    fn content_size(&self) -> Size<Pixels> {
        let state = self.0.borrow();
        size(state.viewport.size.width, px(state.content_height))
    }
}

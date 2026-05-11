//! Vertical scrollbar geometry, drawing, and drag handling shared by
//! `RevisionList` and `DiffView`.
//!
//! The scrollable widgets already own their `vertical_offset` in tree
//! state; this module just supplies pure helpers so they don't each
//! reinvent the same hit-testing and pill drawing. The scrollbar is an
//! overlay — it draws on top of content rather than reserving layout
//! space, matching the macOS / VS Code feel.

use iced::advanced::renderer;
use iced::{Background, Border, Color, Point, Rectangle, Shadow, border};

/// Width of the outer (transparent) container holding the track.
const WIDTH: f32 = 12.0;
/// Padding from each side of the container to the inner pill track.
const PADDING: f32 = 2.0;

#[derive(Debug, Clone, Copy)]
pub struct ScrollbarStyle {
    /// Pill background color.
    pub track_color: Color,
    /// Draggable thumb color.
    pub thumb_color: Color,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScrollbarState {
    drag: Option<DragState>,
}

#[derive(Debug, Clone, Copy)]
struct DragState {
    start_cursor_y: f32,
    start_offset: f32,
}

pub struct ScrollbarGeometry {
    pub container: Rectangle,
    pub track: Rectangle,
    pub thumb: Option<Rectangle>,
}

pub fn geometry(bounds: Rectangle, content_height: f32, offset: f32) -> ScrollbarGeometry {
    let container = Rectangle {
        x: bounds.x + bounds.width - WIDTH,
        y: bounds.y,
        width: WIDTH,
        height: bounds.height,
    };
    let track = Rectangle {
        x: container.x + PADDING,
        y: container.y + PADDING,
        width: (container.width - 2.0 * PADDING).max(0.0),
        height: (container.height - 2.0 * PADDING).max(0.0),
    };
    let thumb = if content_height <= bounds.height || track.height <= 0.0 || track.width <= 0.0 {
        None
    } else {
        // Thumb height proportional to viewport/content ratio, but never
        // shorter than the track width so it stays at least a circle.
        let min_thumb = track.width;
        let raw_h = track.height * (bounds.height / content_height);
        let thumb_h = raw_h.max(min_thumb).min(track.height);
        let scroll_range = (content_height - bounds.height).max(0.0);
        let track_range = (track.height - thumb_h).max(0.0);
        let progress = if scroll_range > 0.0 {
            (offset / scroll_range).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_y = track.y + progress * track_range;
        Some(Rectangle {
            x: track.x,
            y: thumb_y,
            width: track.width,
            height: thumb_h,
        })
    };
    ScrollbarGeometry {
        container,
        track,
        thumb,
    }
}

pub fn draw<R: renderer::Renderer>(
    renderer: &mut R,
    geometry: &ScrollbarGeometry,
    style: &ScrollbarStyle,
) {
    let Some(thumb) = geometry.thumb else {
        return;
    };
    let track_radius = (geometry.track.width.min(geometry.track.height)) / 2.0;
    renderer.fill_quad(
        renderer::Quad {
            bounds: geometry.track,
            border: Border {
                radius: border::Radius::from(track_radius),
                ..Border::default()
            },
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(style.track_color),
    );
    let thumb_radius = (thumb.width.min(thumb.height)) / 2.0;
    renderer.fill_quad(
        renderer::Quad {
            bounds: thumb,
            border: Border {
                radius: border::Radius::from(thumb_radius),
                ..Border::default()
            },
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(style.thumb_color),
    );
}

#[derive(Debug, Clone, Copy)]
pub enum ScrollbarEvent {
    /// Cursor missed the scrollbar — fall through to the widget's
    /// usual handling.
    None,
    /// The scrollbar consumed the event but the offset didn't change
    /// (e.g. mouse-released after a drag).
    Captured,
    /// Caller should set its `vertical_offset` to this value, capture
    /// the event, and request a redraw.
    OffsetChanged(f32),
}

pub fn on_button_pressed(
    state: &mut ScrollbarState,
    cursor: Point,
    bounds: Rectangle,
    content_height: f32,
    offset: f32,
) -> ScrollbarEvent {
    let geom = geometry(bounds, content_height, offset);
    let Some(thumb) = geom.thumb else {
        return ScrollbarEvent::None;
    };
    if !geom.container.contains(cursor) {
        return ScrollbarEvent::None;
    }
    if thumb.contains(cursor) {
        state.drag = Some(DragState {
            start_cursor_y: cursor.y,
            start_offset: offset,
        });
        return ScrollbarEvent::Captured;
    }
    let new_offset = cursor_to_offset(
        cursor.y,
        geom.track,
        thumb.height,
        content_height,
        bounds.height,
    );
    state.drag = Some(DragState {
        start_cursor_y: cursor.y,
        start_offset: new_offset,
    });
    ScrollbarEvent::OffsetChanged(new_offset)
}

pub fn on_cursor_moved(
    state: &mut ScrollbarState,
    cursor: Point,
    bounds: Rectangle,
    content_height: f32,
) -> ScrollbarEvent {
    let Some(drag) = state.drag else {
        return ScrollbarEvent::None;
    };
    let geom = geometry(bounds, content_height, drag.start_offset);
    let Some(thumb) = geom.thumb else {
        return ScrollbarEvent::None;
    };
    let scroll_range = (content_height - bounds.height).max(0.0);
    let track_range = (geom.track.height - thumb.height).max(0.0);
    if track_range <= 0.0 || scroll_range <= 0.0 {
        return ScrollbarEvent::Captured;
    }
    let delta_y = cursor.y - drag.start_cursor_y;
    let new_offset =
        (drag.start_offset + delta_y * scroll_range / track_range).clamp(0.0, scroll_range);
    ScrollbarEvent::OffsetChanged(new_offset)
}

pub fn on_button_released(state: &mut ScrollbarState) -> ScrollbarEvent {
    if state.drag.take().is_some() {
        ScrollbarEvent::Captured
    } else {
        ScrollbarEvent::None
    }
}

pub fn is_dragging(state: &ScrollbarState) -> bool {
    state.drag.is_some()
}

pub fn hits_container(bounds: Rectangle, cursor: Point, content_height: f32) -> bool {
    if content_height <= bounds.height {
        return false;
    }
    let container = Rectangle {
        x: bounds.x + bounds.width - WIDTH,
        y: bounds.y,
        width: WIDTH,
        height: bounds.height,
    };
    container.contains(cursor)
}

fn cursor_to_offset(
    cursor_y: f32,
    track: Rectangle,
    thumb_height: f32,
    content_height: f32,
    viewport_height: f32,
) -> f32 {
    let scroll_range = (content_height - viewport_height).max(0.0);
    let track_range = (track.height - thumb_height).max(0.0);
    if track_range <= 0.0 || scroll_range <= 0.0 {
        return 0.0;
    }
    let target_thumb_y = cursor_y - thumb_height / 2.0;
    let raw_progress = (target_thumb_y - track.y) / track_range;
    raw_progress.clamp(0.0, 1.0) * scroll_range
}

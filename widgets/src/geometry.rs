//! Rectangles, points and sizes in plain floats, for the geometry the app
//! computes and stores (menu anchors, window frames, drop rects) and for
//! painters that test without converting units at each step. GPUI's own
//! `Bounds<Pixels>` and friends convert in with `.into()` at the element edge,
//! and out with [`Rectangle::bounds`].

use gpui_kit::{Bounds, Pixels, point, px, size};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ORIGIN: Self = Self::new(0.0, 0.0);

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub fn distance(&self, other: Point) -> f32 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }

    pub fn gpui(self) -> gpui_kit::Point<Pixels> {
        point(px(self.x), px(self.y))
    }
}

impl From<gpui_kit::Point<Pixels>> for Point {
    fn from(point: gpui_kit::Point<Pixels>) -> Self {
        Self {
            x: f32::from(point.x),
            y: f32::from(point.y),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Self = Self::new(0.0, 0.0);

    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

impl From<gpui_kit::Size<Pixels>> for Size {
    fn from(size: gpui_kit::Size<Pixels>) -> Self {
        Self {
            width: f32::from(size.width),
            height: f32::from(size.height),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rectangle {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rectangle {
    pub fn new(top_left: Point, size: Size) -> Self {
        Self {
            x: top_left.x,
            y: top_left.y,
            width: size.width,
            height: size.height,
        }
    }

    pub fn contains(&self, cursor: Point) -> bool {
        cursor.x >= self.x
            && cursor.x < self.x + self.width
            && cursor.y >= self.y
            && cursor.y < self.y + self.height
    }

    pub fn center_y(&self) -> f32 {
        self.y + self.height / 2.0
    }

    pub fn center_x(&self) -> f32 {
        self.x + self.width / 2.0
    }

    pub fn position(&self) -> Point {
        Point::new(self.x, self.y)
    }

    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(
            point(px(self.x), px(self.y)),
            size(px(self.width), px(self.height)),
        )
    }
}

impl From<Bounds<Pixels>> for Rectangle {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        }
    }
}

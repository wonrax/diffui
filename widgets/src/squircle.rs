//! Continuous-curvature corners ("squircles"), the way macOS and iOS round
//! their surfaces. GPUI's quads only round with circular arcs and its shaders
//! aren't ours to change, so a squircle is a path: a fill, and a stroke inside
//! the edge for the border. [`background`] puts one behind an element's
//! content; [`paint`] is for painters that place their own shapes.
//!
//! The curve is Figma's corner smoothing ("Desperately seeking squircles"): a
//! circular arc in the middle of the corner, eased into each edge by a cubic,
//! so the curvature grows from zero instead of jumping at the tangent point.
//!
//! Credit: painting squircles in GPUI as paths from an element is the approach
//! of gpui_squircle by cameronpcampbell (github.com/astrum-chat/gpui_squircle,
//! MIT). The corner measures follow figma-squircle by Tien Pham
//! (github.com/phamfoo/figma-squircle, MIT), which gpui_squircle builds on
//! through its Rust port.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{
    App, BorderStyle, Bounds, Canvas, ElementId, Hitbox, HitboxBehavior, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Path, PathBuilder, Pixels, Point, Styled, Window,
    canvas, point, px,
};

use crate::theme::Color;

/// How far each corner eases into its edges, as a share of its radius. 0.6
/// is iOS's.
pub const SMOOTHING: f32 = 0.6;

/// A radius per corner, in logical pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Corners {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl Corners {
    pub const fn all(radius: f32) -> Self {
        Self {
            top_left: radius,
            top_right: radius,
            bottom_right: radius,
            bottom_left: radius,
        }
    }

    /// Only the bottom corners: a footer flush with its card's bottom edge.
    pub const fn bottom(radius: f32) -> Self {
        Self {
            top_left: 0.0,
            top_right: 0.0,
            bottom_right: radius,
            bottom_left: radius,
        }
    }

    /// Only the right corners: a card flush with the window's left edge.
    pub const fn right(radius: f32) -> Self {
        Self {
            top_left: 0.0,
            top_right: radius,
            bottom_right: radius,
            bottom_left: 0.0,
        }
    }

    fn inset(self, by: f32) -> Self {
        let shrink = |radius: f32| (radius - by).max(0.0);
        Self {
            top_left: shrink(self.top_left),
            top_right: shrink(self.top_right),
            bottom_right: shrink(self.bottom_right),
            bottom_left: shrink(self.bottom_left),
        }
    }
}

/// One piece of a squircle's outline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Segment {
    Move(Point<f32>),
    Line(Point<f32>),
    Cubic {
        control_a: Point<f32>,
        control_b: Point<f32>,
        to: Point<f32>,
    },
    /// A clockwise circular arc of `radius`.
    Arc {
        radius: f32,
        to: Point<f32>,
    },
}

/// The measures of one smoothed corner, along its two edges from where the
/// curve leaves the edge (`p` from the corner) to its middle.
#[derive(Debug, Clone, Copy)]
struct Corner {
    radius: f32,
    /// How far from the corner the curve starts, on both edges.
    p: f32,
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    /// Each leg of the circular arc's chord.
    arc: f32,
}

/// `radius` smoothed by `smoothing`, within `budget`: the most of each edge
/// this corner may take before it would meet its neighbour.
fn corner(radius: f32, smoothing: f32, budget: f32) -> Corner {
    let radius = radius.min(budget).max(0.0);
    // A corner without room to ease keeps less smoothing rather than
    // overrunning its edge.
    let smoothing = if radius > 0.0 {
        smoothing.min(budget / radius - 1.0).max(0.0)
    } else {
        0.0
    };
    let p = ((1.0 + smoothing) * radius).min(budget);
    let arc_measure = 90.0 * (1.0 - smoothing);
    let arc = (arc_measure / 2.0).to_radians().sin() * radius * std::f32::consts::SQRT_2;
    let alpha = (90.0 - arc_measure) / 2.0;
    let p3_to_p4 = radius * (alpha / 2.0).to_radians().tan();
    let beta = 45.0 * smoothing;
    let c = p3_to_p4 * beta.to_radians().cos();
    let d = c * beta.to_radians().tan();
    let b = (p - arc - c - d) / 3.0;
    Corner {
        radius,
        p,
        a: 2.0 * b,
        b,
        c,
        d,
        arc,
    }
}

/// The rounded corners of a box of `size`, clockwise from the top right.
fn corner_shapes(size: (f32, f32), corners: Corners, smoothing: f32) -> [Option<Corner>; 4] {
    let (width, height) = size;
    // A corner may take half of an edge, or all of it when the corner at the
    // edge's other end is square.
    let budget = |radius: f32, along_x: f32, along_y: f32| {
        let x = if along_x > 0.0 { width / 2.0 } else { width };
        let y = if along_y > 0.0 { height / 2.0 } else { height };
        (radius > 0.0).then(|| corner(radius, smoothing, x.min(y)))
    };
    [
        budget(corners.top_right, corners.top_left, corners.bottom_right),
        budget(corners.bottom_right, corners.bottom_left, corners.top_right),
        budget(corners.bottom_left, corners.bottom_right, corners.top_left),
        budget(corners.top_left, corners.top_right, corners.bottom_left),
    ]
}

/// `corners` as the circular radii a quad draws, when no corner eases off its
/// circle by more than one of [`MIN_RADIUS`] does: a small control, or one too
/// short to keep its smoothing. A quad is drawn with the rest; a path costs the
/// frame a pass of its own.
fn circular(size: (f32, f32), corners: Corners) -> Option<Corners> {
    let shapes = corner_shapes(size, corners, SMOOTHING);
    let eased = |shape: &Corner| shape.p - shape.radius;
    if shapes
        .iter()
        .flatten()
        .any(|shape| eased(shape) + 1e-3 >= SMOOTHING * MIN_RADIUS)
    {
        return None;
    }
    let [top_right, bottom_right, bottom_left, top_left] =
        shapes.map(|shape| shape.map_or(0.0, |shape| shape.radius));
    Some(Corners {
        top_left,
        top_right,
        bottom_right,
        bottom_left,
    })
}

/// The outline of a box at `origin` of `size` with `corners`, clockwise from
/// the top edge.
pub(crate) fn outline(
    origin: Point<f32>,
    size: (f32, f32),
    corners: Corners,
    smoothing: f32,
) -> Vec<Segment> {
    let (width, height) = size;
    let (left, top) = (origin.x, origin.y);
    let (right, bottom) = (left + width, top + height);
    let [top_right, bottom_right, bottom_left, top_left] = corner_shapes(size, corners, smoothing);
    // Each corner with its point and its frame: `u` runs along the edge into
    // the corner, `v` along the edge out of it.
    let shapes = [
        (top_right, point(right, top), (1.0, 0.0), (0.0, 1.0)),
        (bottom_right, point(right, bottom), (0.0, 1.0), (-1.0, 0.0)),
        (bottom_left, point(left, bottom), (-1.0, 0.0), (0.0, -1.0)),
        (top_left, point(left, top), (0.0, -1.0), (1.0, 0.0)),
    ];

    let mut segments = Vec::with_capacity(17);
    // Start where the top-left corner hands over to the top edge.
    let top_left_p = shapes[3].0.map_or(0.0, |corner| corner.p);
    segments.push(Segment::Move(point(left + top_left_p, top)));
    for (corner, at, u, v) in shapes {
        let Some(corner) = corner else {
            segments.push(Segment::Line(at));
            continue;
        };
        // `x` along `u`, `y` along `v`, from `from`.
        let step = |from: Point<f32>, x: f32, y: f32| {
            point(from.x + x * u.0 + y * v.0, from.y + x * u.1 + y * v.1)
        };
        let Corner {
            radius,
            p,
            a,
            b,
            c,
            d,
            arc,
        } = corner;
        let start = step(at, -p, 0.0);
        segments.push(Segment::Line(start));
        let eased = step(start, a + b + c, d);
        segments.push(Segment::Cubic {
            control_a: step(start, a, 0.0),
            control_b: step(start, a + b, 0.0),
            to: eased,
        });
        let arc_end = step(eased, arc, arc);
        segments.push(Segment::Arc {
            radius,
            to: arc_end,
        });
        segments.push(Segment::Cubic {
            control_a: step(arc_end, d, c),
            control_b: step(arc_end, d, b + c),
            to: step(arc_end, d, a + b + c),
        });
    }
    segments
}

fn trace(builder: &mut PathBuilder, segments: &[Segment]) {
    let at = |p: Point<f32>| point(px(p.x), px(p.y));
    for segment in segments {
        match *segment {
            Segment::Move(to) => builder.move_to(at(to)),
            Segment::Line(to) => builder.line_to(at(to)),
            Segment::Cubic {
                control_a,
                control_b,
                to,
            } => builder.cubic_bezier_to(at(to), at(control_a), at(control_b)),
            Segment::Arc { radius, to } => {
                builder.arc_to(point(px(radius), px(radius)), px(0.0), false, true, at(to))
            }
        }
    }
    builder.close();
}

/// What a squircle is filled and edged with. The border is drawn inside the
/// bounds, as GPUI draws a border.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fill {
    pub corners: Corners,
    pub background: Option<Color>,
    pub border: Option<(f32, Color)>,
}

impl Fill {
    pub fn new(corners: Corners) -> Self {
        Self {
            corners,
            background: None,
            border: None,
        }
    }

    pub fn background(mut self, color: Color) -> Self {
        self.background = (color.a > 0.0).then_some(color);
        self
    }

    pub fn border(mut self, width: f32, color: Color) -> Self {
        self.border = (width > 0.0 && color.a > 0.0).then_some((width, color));
        self
    }
}

/// Below this radius a squircle and a circular corner differ by less than a
/// pixel, so painters keep GPUI's cheaper quad.
pub const MIN_RADIUS: f32 = 8.0;

fn frame(bounds: Bounds<Pixels>) -> Option<(Point<f32>, (f32, f32))> {
    let origin = point(f32::from(bounds.origin.x), f32::from(bounds.origin.y));
    let size = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    (size.0 > 0.0 && size.1 > 0.0).then_some((origin, size))
}

/// The filled squircle over `bounds`, for a painter to paint.
pub fn fill_path(bounds: Bounds<Pixels>, corners: Corners) -> Option<Path<Pixels>> {
    let (origin, size) = frame(bounds)?;
    let mut builder = PathBuilder::fill();
    trace(&mut builder, &outline(origin, size, corners, SMOOTHING));
    builder.build().ok()
}

/// A `width` edge inside `bounds`, following the squircle, for a painter to
/// paint.
pub fn stroke_path(bounds: Bounds<Pixels>, corners: Corners, width: f32) -> Option<Path<Pixels>> {
    let (origin, size) = frame(bounds)?;
    // The stroke runs down the middle of the edge.
    let half = width / 2.0;
    let mut builder = PathBuilder::stroke(px(width));
    trace(
        &mut builder,
        &outline(
            point(origin.x + half, origin.y + half),
            (size.0 - width, size.1 - width),
            corners.inset(half),
            SMOOTHING,
        ),
    );
    builder.build().ok()
}

/// Paints `fill` over `bounds`: as a quad when its corners are as good as
/// circular (see [`circular`]), else as paths.
pub fn paint(bounds: Bounds<Pixels>, fill: Fill, window: &mut Window) {
    let Some((_, size)) = frame(bounds) else {
        return;
    };
    if fill.background.is_none() && fill.border.is_none() {
        return;
    }
    if let Some(radii) = circular(size, fill.corners) {
        let (width, color) = fill.border.unwrap_or((0.0, crate::theme::TRANSPARENT));
        window.paint_quad(gpui_kit::quad(
            bounds,
            gpui_kit::Corners {
                top_left: px(radii.top_left),
                top_right: px(radii.top_right),
                bottom_right: px(radii.bottom_right),
                bottom_left: px(radii.bottom_left),
            },
            fill.background.unwrap_or(crate::theme::TRANSPARENT),
            px(width),
            color,
            BorderStyle::Solid,
        ));
        return;
    }
    if let Some(color) = fill.background
        && let Some(path) = fill_path(bounds, fill.corners)
    {
        window.paint_path(path, color);
    }
    if let Some((width, color)) = fill.border
        && let Some(path) = stroke_path(bounds, fill.corners, width)
    {
        window.paint_path(path, color);
    }
}

/// An element that paints `fill` over its parent's whole box, behind the
/// parent's other children: make it the parent's first child. `border` is the
/// width of the parent's own (transparent) border, which the fill covers too.
pub fn background(fill: Fill, border: f32) -> Canvas<()> {
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| paint(bounds, fill, window),
    )
    .absolute()
    .top(px(-border))
    .left(px(-border))
    .right(px(-border))
    .bottom(px(-border))
}

/// [`background`] painted only while the pointer is over the parent: the
/// hover wash of a row or a caret. It must come before the parent's content.
pub fn hover_background(fill: Fill, border: f32) -> Canvas<Hitbox> {
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, _| {
            let hovered = hitbox.is_hovered(window);
            if hovered {
                paint(bounds, fill, window);
            }
            repaint_on_hover_change(hitbox, hovered, window);
        },
    )
    .absolute()
    .top(px(-border))
    .left(px(-border))
    .right(px(-border))
    .bottom(px(-border))
}

/// Repaints the view when the pointer crosses `hitbox`, painted as `hovered`.
/// GPUI does this for an element with a hover style; a canvas has none.
fn repaint_on_hover_change(hitbox: Hitbox, hovered: bool, window: &mut Window) {
    let view = window.current_view();
    window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
        if phase.capture() && hitbox.is_hovered(window) != hovered {
            cx.notify(view);
        }
    });
}

/// The fills of a control that answers the pointer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct States {
    pub rest: Fill,
    pub hover: Fill,
    pub active: Fill,
}

/// [`background`] for a control: `hover` while the pointer is over it, and
/// `active` while it's pressed there. GPUI's hover and press styles apply to
/// the element's own (circular) background, so the control keeps those
/// transparent and this paints its states instead.
pub fn interactive_background(
    id: impl Into<ElementId>,
    states: States,
    border: f32,
) -> Canvas<Hitbox> {
    let id = id.into();
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, cx: &mut App| {
            let pressed = window.use_keyed_state(id, cx, |_, _| Rc::new(Cell::new(false)));
            let pressed = pressed.read(cx).clone();
            let hovered = hitbox.is_hovered(window);
            let fill = match (hovered, pressed.get()) {
                (true, true) => states.active,
                (true, false) => states.hover,
                (false, _) => states.rest,
            };
            paint(bounds, fill, window);
            repaint_on_hover_change(hitbox.clone(), hovered, window);

            let down = pressed.clone();
            let target = hitbox.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _| {
                if phase.bubble() && event.button == MouseButton::Left && target.is_hovered(window)
                {
                    down.set(true);
                    window.refresh();
                }
            });
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, _| {
                if phase.capture() && event.button == MouseButton::Left && pressed.get() {
                    pressed.set(false);
                    window.refresh();
                }
            });
        },
    )
    .absolute()
    .top(px(-border))
    .left(px(-border))
    .right(px(-border))
    .bottom(px(-border))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn end(segment: &Segment) -> Point<f32> {
        match *segment {
            Segment::Move(to) | Segment::Line(to) => to,
            Segment::Cubic { to, .. } | Segment::Arc { to, .. } => to,
        }
    }

    fn close(a: Point<f32>, b: Point<f32>) -> bool {
        (a.x - b.x).abs() < 1e-3 && (a.y - b.y).abs() < 1e-3
    }

    #[test]
    fn each_corner_leaves_its_edge_where_the_next_edge_starts() {
        let segments = outline(
            point(10.0, 20.0),
            (200.0, 80.0),
            Corners::all(16.0),
            SMOOTHING,
        );
        let p = corner(16.0, SMOOTHING, 40.0).p;
        assert!((p - 25.6).abs() < 1e-3, "1.6 radii of edge: {p}");
        // Move, then four corners of line, cubic, arc, cubic.
        assert_eq!(segments.len(), 17);
        assert!(close(end(&segments[0]), point(10.0 + p, 20.0)));
        // Each corner ends on the next edge, `p` from the corner.
        assert!(close(end(&segments[4]), point(210.0, 20.0 + p)));
        assert!(close(end(&segments[8]), point(210.0 - p, 100.0)));
        assert!(close(end(&segments[12]), point(10.0, 100.0 - p)));
        // And the last one closes the loop where it began.
        assert!(close(end(&segments[16]), end(&segments[0])));
    }

    #[test]
    fn the_curve_meets_a_circular_corner_at_45_degrees_and_leaves_the_edge_earlier() {
        let r = 20.0;
        let segments = outline(point(0.0, 0.0), (100.0, 100.0), Corners::all(r), SMOOTHING);
        for segment in &segments {
            let p = end(segment);
            let inside = |v: f32| (-1e-3..=100.0 + 1e-3).contains(&v);
            assert!(inside(p.x) && inside(p.y), "{p:?}");
        }
        // The arc in the middle of the top-right corner is the circle a plain
        // rounded corner of the same radius would draw.
        let centre = point(100.0 - r, r);
        for index in [2, 3] {
            let p = end(&segments[index]);
            let distance = ((p.x - centre.x).powi(2) + (p.y - centre.y).powi(2)).sqrt();
            assert!(
                (distance - r).abs() < 1e-3,
                "{p:?} is {distance} from the centre"
            );
        }
        // But the curve leaves the top edge 1.6 radii from the corner, not 1.
        assert!(close(end(&segments[1]), point(100.0 - 1.6 * r, 0.0)));
    }

    #[test]
    fn without_smoothing_a_corner_is_a_circular_arc() {
        let c = corner(12.0, 0.0, 50.0);
        assert!((c.p - 12.0).abs() < 1e-4 && (c.arc - 12.0).abs() < 1e-4);
        assert!(c.a.abs() < 1e-4 && c.b.abs() < 1e-4 && c.c.abs() < 1e-4 && c.d.abs() < 1e-4);
    }

    #[test]
    fn a_short_box_trades_smoothing_for_room() {
        // 24 tall with radius 10: half the height is 12, less than 1.6 × 10.
        let c = corner(10.0, SMOOTHING, 12.0);
        assert!(c.p <= 12.0 + 1e-4, "{c:?}");
        assert!(c.a >= -1e-4 && c.b >= -1e-4, "{c:?}");
        // A pill keeps its full radius and no smoothing.
        let pill = corner(30.0, SMOOTHING, 12.0);
        assert!((pill.radius - 12.0).abs() < 1e-4 && (pill.p - 12.0).abs() < 1e-4);
    }

    #[test]
    fn small_controls_draw_circular_corners() {
        // A 24 px button with a 6 px radius, and one too short for its 10 px.
        assert_eq!(
            circular((80.0, 24.0), Corners::all(6.0)),
            Some(Corners::all(6.0))
        );
        assert_eq!(
            circular((80.0, 24.0), Corners::all(10.0)),
            Some(Corners::all(10.0))
        );
        // A pill keeps the radius it has room for.
        assert_eq!(
            circular((80.0, 24.0), Corners::all(30.0)),
            Some(Corners::all(12.0))
        );
        assert_eq!(
            circular((80.0, 24.0), Corners::default()),
            Some(Corners::default())
        );
        // A surface keeps its squircle, and so does a box with one large corner.
        assert_eq!(circular((400.0, 300.0), Corners::all(12.0)), None);
        assert_eq!(circular((400.0, 300.0), Corners::all(MIN_RADIUS)), None);
        assert_eq!(circular((400.0, 300.0), Corners::right(12.0)), None);
    }

    #[test]
    fn square_corners_are_plain_lines() {
        let segments = outline(
            point(0.0, 0.0),
            (40.0, 30.0),
            Corners::bottom(8.0),
            SMOOTHING,
        );
        assert_eq!(segments[0], Segment::Move(point(0.0, 0.0)));
        assert_eq!(segments[1], Segment::Line(point(40.0, 0.0)));
        // The bottom corners may use the full height: the top ones are square.
        let p = corner(8.0, SMOOTHING, 20.0).p;
        assert!(close(end(&segments[2]), point(40.0, 30.0 - p)));
        assert_eq!(*segments.last().unwrap(), Segment::Line(point(0.0, 0.0)));
    }
}

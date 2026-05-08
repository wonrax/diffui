//! Drawing primitives for the revision-graph gutter. Called from
//! [`crate::revision_list`] inside its custom widget's `draw`.
//!
//! Edges are stroked paths drawn through
//! `iced::advanced::graphics::geometry::Frame` so we get rounded corners
//! and a slight angle at the node — neither of which is reachable through
//! the plain quad renderer.
//!
//! Two shapes:
//! * [`draw_revision_row`] — a row whose center hosts a node circle, with
//!   incoming edges in the top half and outgoing edges in the bottom half.
//! * [`draw_continuation_row`] — a strip that just runs lanes straight
//!   through, used under expanded file rows so the gutter stays unbroken
//!   between one revision and the next.

use iced::advanced::graphics::geometry::{self, Frame, LineCap, LineJoin, Path, Stroke};
use iced::advanced::renderer;
use iced::{Color, Point, Rectangle, Size};
use jj_lib::graph::GraphEdgeType;

use crate::graph::LaneFrame;

#[derive(Debug, Clone, Copy)]
pub struct RevisionGraphStyle {
    pub lane_width: f32,
    pub line_thickness: f32,
    pub node_radius: f32,
    /// Base color for lane 0 (the trunk). Other lanes — and the node
    /// discs that sit on them — are derived from this via a deterministic
    /// hue rotation; see [`Self::lane_color`]. We don't expose a separate
    /// `node_color`: a node always wears its own lane's color so the eye
    /// can trace a branch's hue continuously through both edges and
    /// commits.
    pub lane_base_color: Color,
    /// Color used for the missing-parent stub. Doesn't participate in lane
    /// coloring because it isn't a real edge.
    pub missing_color: Color,
}

impl RevisionGraphStyle {
    /// Color for a given lane, deterministic in `lane`. Lane 0 is always
    /// `lane_base_color`; subsequent lanes rotate the hue by a fixed step
    /// so a branch's color stays the same as long as it stays in its lane,
    /// and adjacent lanes are visually distinct without becoming a
    /// rainbow. The step is chosen so lanes 0..7 are unambiguously
    /// distinguishable; beyond that we wrap, but typical jj graphs rarely
    /// exceed 5-6 concurrent branches.
    pub fn lane_color(&self, lane: usize) -> Color {
        if lane == 0 {
            return self.lane_base_color;
        }
        // 60° per lane gives unmistakably distinct colors at the cost of a
        // shorter cycle (lane 6 wraps back to lane 0's hue). For typical
        // jj graphs that rarely exceed 5–6 concurrent branches this is the
        // sweet spot — anything subtler and adjacent branches blur into
        // each other when they share the gutter.
        const HUE_STEP_DEG: f32 = 60.0;
        let (h, s, l) = rgb_to_hsl(
            self.lane_base_color.r,
            self.lane_base_color.g,
            self.lane_base_color.b,
        );
        let new_h = (h + lane as f32 * HUE_STEP_DEG).rem_euclid(360.0);
        let (r, g, b) = hsl_to_rgb(new_h, s, l);
        Color {
            r,
            g,
            b,
            a: self.lane_base_color.a,
        }
    }

    /// Stroke style for an edge of a given kind in a given lane. Indirect
    /// edges are dashed so they remain distinguishable when they share a
    /// lane color with a direct edge.
    fn edge_stroke<'a>(&self, kind: GraphEdgeType, lane: usize, dash: &'a [f32]) -> Stroke<'a> {
        let color = match kind {
            GraphEdgeType::Missing => self.missing_color,
            _ => self.lane_color(lane),
        };
        let mut stroke = Stroke::default()
            .with_color(color)
            .with_width(self.line_thickness)
            .with_line_cap(LineCap::Round)
            .with_line_join(LineJoin::Round);
        if matches!(kind, GraphEdgeType::Indirect) {
            stroke.line_dash = geometry::LineDash {
                segments: dash,
                offset: 0,
            };
        }
        stroke
    }
}

/// Width in pixels needed to render a row with `lane_count` lanes.
pub fn lane_strip_width(lane_count: usize, style: &RevisionGraphStyle) -> f32 {
    lane_count as f32 * style.lane_width
}

fn lane_x(x_origin: f32, lane: usize, style: &RevisionGraphStyle) -> f32 {
    x_origin + (lane as f32 + 0.5) * style.lane_width
}

/// Tilt of the branch-off segment, measured from horizontal. A pure
/// `jj log`-style square corner is 0° (perfectly horizontal departure
/// off the vertical lane); we tilt by `NODE_TILT_DEG` so two unrelated
/// branches that happen to reuse the same lane index in succession
/// don't visually merge into one continuous line at the node.
///
/// 10° is the sweet spot: visible enough to break the continuity, not
/// so much that the gutter starts looking sketched.
const NODE_TILT_DEG: f32 = 10.0;

/// Corner radius as a fraction of `lane_width`. Bigger = softer, more
/// rounded turn; smaller = closer to a true square corner. Half the
/// lane width is the practical ceiling — anything more starts to crowd
/// the adjacent lane's vertical line; this value sits just under that
/// to give a noticeably smooth turn while keeping a small straight stub
/// of the tilted leg visible before the arc takes over.
const CORNER_RADIUS_FRAC: f32 = 0.5;

/// Draw an outgoing connector from the node at `(x_node, y_mid)` down to
/// `(x_target, y_bot)`. Same-lane edges get a plain vertical; cross-lane
/// edges get the classic L-bend with two refinements:
///
/// * The horizontal-ish leg is tilted `NODE_TILT_DEG` off horizontal, so
///   the line *droops* slightly as it travels from the node to the
///   target column instead of running perfectly flat. The tilt is what
///   visually separates branches that reuse a lane.
/// * The corner where the tilted leg meets the vertical drop is rounded
///   via `arc_to`, which fits a circular arc tangent to both segments.
///
/// The line runs all the way to `(x_node, y_mid)` — the disc centre.
/// Anything inside the disc area gets covered when the disc is painted
/// on top, so the messy convergence of multiple edges at the centre is
/// hidden by the disc rather than fanned out around it.
fn draw_outgoing(
    builder: &mut iced::advanced::graphics::geometry::path::Builder,
    x_node: f32,
    x_target: f32,
    y_mid: f32,
    y_bot: f32,
    lane_width: f32,
) {
    if (x_target - x_node).abs() < 0.5 {
        builder.move_to(Point::new(x_node, y_mid));
        builder.line_to(Point::new(x_node, y_bot));
        return;
    }

    let dx = (x_target - x_node).abs();
    // Where the tilted horizontal would intersect the target column.
    // `tan(NODE_TILT_DEG)` per pixel of horizontal travel — for 10° that's
    // ~17.6% of dx as the vertical drop, so a one-lane jump dips only a
    // couple of pixels before the corner, keeping the leg unmistakably
    // "horizontal".
    let corner_y = y_mid + dx * NODE_TILT_DEG.to_radians().tan();
    // Cap against the remaining vertical drop so `arc_to` doesn't try to
    // fit an arc taller than the row — that degenerates into a plain
    // line and we lose the rounding entirely.
    let radius = (lane_width * CORNER_RADIUS_FRAC).min((y_bot - corner_y) * 0.5);

    let corner = Point::new(x_target, corner_y);
    let end = Point::new(x_target, y_bot);

    builder.move_to(Point::new(x_node, y_mid));
    builder.arc_to(corner, end, radius);
    builder.line_to(end);
}

/// Mirror of `draw_outgoing` for edges arriving at the node from above:
/// vertical drop in the source column, rounded corner, tilted approach
/// to the node centre. As with `draw_outgoing`, the path runs to the
/// disc centre and the disc covers the convergence.
fn draw_incoming(
    builder: &mut iced::advanced::graphics::geometry::path::Builder,
    x_source: f32,
    x_node: f32,
    y_top: f32,
    y_mid: f32,
    lane_width: f32,
) {
    if (x_node - x_source).abs() < 0.5 {
        builder.move_to(Point::new(x_node, y_top));
        builder.line_to(Point::new(x_node, y_mid));
        return;
    }

    let dx = (x_node - x_source).abs();
    let corner_y = y_mid - dx * NODE_TILT_DEG.to_radians().tan();
    let radius = (lane_width * CORNER_RADIUS_FRAC).min((corner_y - y_top) * 0.5);

    let corner = Point::new(x_source, corner_y);
    let node_point = Point::new(x_node, y_mid);

    builder.move_to(Point::new(x_source, y_top));
    builder.arc_to(corner, node_point, radius);
    builder.line_to(node_point);
}

pub fn draw_revision_row<R>(
    renderer: &mut R,
    bounds: Rectangle,
    frame_data: &LaneFrame,
    style: &RevisionGraphStyle,
) where
    R: renderer::Renderer + geometry::Renderer,
{
    let top = bounds.y;
    let bot = bounds.y + bounds.height;
    let mid = bounds.y + bounds.height / 2.0;
    let x_node = lane_x(bounds.x, frame_data.node_lane, style);

    // Indirect-edge dash pattern. Allocated once per row so the borrow
    // outlives the strokes; the pattern itself is just two `f32`s.
    let dash: [f32; 2] = [4.0, 3.0];

    // Allocate one geometry frame per row covering the whole row's bounds.
    // Using row-local coords would be cleaner but `Frame::translate` adds
    // complexity for little gain at this granularity.
    let mut frame = Frame::new(renderer, Size::new(bounds.x + bounds.width, bot));

    // Incoming edges (top half).
    for (i, kind) in frame_data.before.iter().enumerate() {
        let Some(kind) = *kind else { continue };
        let x_i = lane_x(bounds.x, i, style);

        if frame_data.merging_lanes.contains(&i) {
            // This lane terminates at the node — vertical from top, then
            // either continues straight (if i == node_lane) or curves into
            // the node from the side.
            let path = Path::new(|builder| {
                draw_incoming(builder, x_i, x_node, top, mid, style.lane_width);
            });
            frame.stroke(&path, style.edge_stroke(kind, i, &dash));
        } else if frame_data.is_pass_through(i) {
            // Pure pass-through: draw a single straight vertical so we
            // benefit from the round line cap and dashed style if needed.
            let path = Path::line(Point::new(x_i, top), Point::new(x_i, bot));
            frame.stroke(&path, style.edge_stroke(kind, i, &dash));
        } else {
            // Lane terminates above the node without merging into it
            // (e.g., ended on the previous row). Just a half-line.
            let path = Path::line(Point::new(x_i, top), Point::new(x_i, mid));
            frame.stroke(&path, style.edge_stroke(kind, i, &dash));
        }
    }

    // Outgoing edges (bottom half).
    for (j, kind) in frame_data.after.iter().enumerate() {
        let Some(kind) = *kind else { continue };
        if frame_data.is_pass_through(j) {
            // Already drawn above as part of the incoming pass — avoid
            // double-stroking.
            continue;
        }
        let x_j = lane_x(bounds.x, j, style);

        let path = Path::new(|builder| {
            draw_outgoing(builder, x_node, x_j, mid, bot, style.lane_width);
        });
        // Color outgoing edges by the destination lane: a branch that
        // *opens* on lane 2 should already wear lane 2's color from the
        // moment it leaves the node, so the eye picks it up consistently
        // wherever the branch travels.
        frame.stroke(&path, style.edge_stroke(kind, j, &dash));
    }

    // Missing-parent stub goes into the same geometry frame as the edges
    // (and the disc that follows). iced renders geometry on a layer above
    // quad primitives, so a `fill_quad` here would visibly sit *under*
    // the stroked edges — order of *calls* doesn't matter, layer does.
    // Within a single frame the order *does* matter (later draws on top),
    // so adding the stub before the disc lets the disc still cover any
    // overlap with it.
    if frame_data.missing_parents > 0 {
        let stub_x = x_node + style.node_radius * 1.2;
        let stub_top = mid + style.node_radius;
        let stub_bot = (mid + style.node_radius + bounds.height * 0.3).min(bot);
        let stub_path = Path::line(Point::new(stub_x, stub_top), Point::new(stub_x, stub_bot));
        let stub_stroke = Stroke::default()
            .with_color(style.missing_color)
            .with_width(style.line_thickness)
            .with_line_cap(LineCap::Round);
        frame.stroke(&stub_path, stub_stroke);
    }

    // Node disc — fill, not quad, so it lives in the same layer as the
    // strokes. Two jobs:
    //   1) Hide the messy convergence of multiple edges meeting at the
    //      centre — the disc just covers it (only works because it's the
    //      *last* operation on this frame).
    //   2) Wear the colour of the node's own lane so a branch reads as
    //      a single continuous hue from edge → commit → edge. For merge
    //      nodes (multiple parents) the lane-assignment first-parent
    //      rule means `node_lane` is also the first parent's lane, so
    //      this picks the closest possible match in hue.
    let disc_color = style.lane_color(frame_data.node_lane);
    let disc_path = Path::new(|b| b.circle(Point::new(x_node, mid), style.node_radius));
    frame.fill(&disc_path, disc_color);

    renderer.draw_geometry(frame.into_geometry());
}

pub fn draw_continuation_row<R>(
    renderer: &mut R,
    bounds: Rectangle,
    lanes: &[Option<GraphEdgeType>],
    style: &RevisionGraphStyle,
) where
    R: renderer::Renderer + geometry::Renderer,
{
    let top = bounds.y;
    let bot = bounds.y + bounds.height;
    let dash: [f32; 2] = [4.0, 3.0];

    let mut frame = Frame::new(renderer, Size::new(bounds.x + bounds.width, bot));
    for (i, kind) in lanes.iter().enumerate() {
        let Some(kind) = *kind else { continue };
        let x = lane_x(bounds.x, i, style);
        let path = Path::line(Point::new(x, top), Point::new(x, bot));
        frame.stroke(&path, style.edge_stroke(kind, i, &dash));
    }
    renderer.draw_geometry(frame.into_geometry());
}

/// sRGB-channel → HSL. Inputs are in [0, 1]; output hue is in degrees,
/// saturation and lightness in [0, 1]. Standard formula — kept inline so
/// we don't pull a color crate just for this.
fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;

    if (max - min).abs() < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    let h = h * 60.0;
    let h = if h < 0.0 { h + 360.0 } else { h };
    (h, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s.abs() < f32::EPSILON {
        return (l, l, l);
    }
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime.rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r1, g1, b1) = match h_prime as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r1 + m, g1 + m, b1 + m)
}

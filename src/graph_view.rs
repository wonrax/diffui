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

/// Horizontal width occupied by one lane in the gutter. `lane_x` centers
/// the node disc in the middle of its lane, so adjacent lanes sit at
/// `LANE_WIDTH` apart. Exposed because the revision list reserves
/// `lanes * LANE_WIDTH` of horizontal space for the gutter, and sidebar
/// layout math needs the same number to size the path column.
pub const LANE_WIDTH: f32 = 10.0;
const LINE_THICKNESS: f32 = 1.5;
const LINE_THICKNESS_EMPHASIZED: f32 = 2.75;
const NODE_RADIUS: f32 = 4.0;

/// Geometry of the elided-ancestry squiggle (jj log's `~`): drawn below a
/// node whose parent(s) sit outside the loaded set — the bottom of a
/// filtered revset, or the first sighting of a boundary commit. Half-width
/// and amplitude in px; small enough to read as graph punctuation, not data.
const ELIDED_SQUIGGLE_HALF_WIDTH: f32 = 3.5;
const ELIDED_SQUIGGLE_AMPLITUDE: f32 = 3.2;

#[derive(Debug, Clone, Copy)]
pub struct RevisionGraphStyle {
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
    /// lane color with a direct edge. When `emphasized` is true the
    /// stroke renders thicker — used for the lane under the cursor.
    fn edge_stroke<'a>(
        &self,
        kind: GraphEdgeType,
        lane: usize,
        dash: &'a [f32],
        emphasized: bool,
    ) -> Stroke<'a> {
        let color = match kind {
            GraphEdgeType::Missing => self.missing_color,
            _ => self.lane_color(lane),
        };
        let width = if emphasized {
            LINE_THICKNESS_EMPHASIZED
        } else {
            LINE_THICKNESS
        };
        let mut stroke = Stroke::default()
            .with_color(color)
            .with_width(width)
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
pub fn lane_strip_width(lane_count: usize) -> f32 {
    lane_count as f32 * LANE_WIDTH
}

fn lane_x(x_origin: f32, lane: usize) -> f32 {
    x_origin + (lane as f32 + 0.5) * LANE_WIDTH
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
    let natural_corner_y = y_mid + dx * NODE_TILT_DEG.to_radians().tan();
    // Keep the corner inside the row with room left for the arc. On wide
    // graphs (many concurrent lanes) the natural 10° tilt would overshoot
    // the row bottom — without this cap, the path either clips against
    // the geometry frame or bleeds into the next revision. Clamping flattens
    // the effective tilt so the line reads as a long diagonal into the
    // target column instead.
    let max_corner_y = (y_bot - LANE_WIDTH * CORNER_RADIUS_FRAC * 0.5).max(y_mid);
    let corner_y = natural_corner_y.min(max_corner_y);
    let radius = (LANE_WIDTH * CORNER_RADIUS_FRAC)
        .min((y_bot - corner_y) * 0.5)
        .max(0.0);

    let corner = Point::new(x_target, corner_y);
    let end = Point::new(x_target, y_bot);

    builder.move_to(Point::new(x_node, y_mid));
    builder.arc_to(corner, end, radius);
    builder.line_to(end);
}

/// Smooth S-bend for a lane whose warped column changed between rows: the
/// line leaves the previous row's column at the row top and eases into this
/// row's column by mid-height, with vertical tangents at both ends. Unlike
/// the tilted node connectors there is no node to point at here — the curve
/// should read as the same vertical lane drifting sideways, not as a branch
/// — so it gets a symmetric ease instead of the tilt + corner treatment.
fn draw_lane_shift(
    builder: &mut iced::advanced::graphics::geometry::path::Builder,
    x_from: f32,
    x_to: f32,
    y_top: f32,
    y_mid: f32,
) {
    builder.move_to(Point::new(x_from, y_top));
    if (x_to - x_from).abs() < 0.5 {
        builder.line_to(Point::new(x_from, y_mid));
        return;
    }
    // Control points at half height in each column: the standard
    // vertical-tangent cubic, so the bend enters and leaves perfectly
    // upright and adjacent shifting lanes stay parallel through the turn.
    let y_blend = (y_top + y_mid) / 2.0;
    builder.bezier_curve_to(
        Point::new(x_from, y_blend),
        Point::new(x_to, y_blend),
        Point::new(x_to, y_mid),
    );
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
) {
    if (x_node - x_source).abs() < 0.5 {
        builder.move_to(Point::new(x_node, y_top));
        builder.line_to(Point::new(x_node, y_mid));
        return;
    }

    let dx = (x_node - x_source).abs();
    let natural_corner_y = y_mid - dx * NODE_TILT_DEG.to_radians().tan();
    // Mirror of `draw_outgoing`: keep the corner inside the row with room
    // for the arc, so wide graphs don't push the path above `y_top`.
    let min_corner_y = (y_top + LANE_WIDTH * CORNER_RADIUS_FRAC * 0.5).min(y_mid);
    let corner_y = natural_corner_y.max(min_corner_y);
    let radius = (LANE_WIDTH * CORNER_RADIUS_FRAC)
        .min((corner_y - y_top) * 0.5)
        .max(0.0);

    let corner = Point::new(x_source, corner_y);
    let node_point = Point::new(x_node, y_mid);

    builder.move_to(Point::new(x_source, y_top));
    builder.arc_to(corner, node_point, radius);
    builder.line_to(node_point);
}

#[allow(clippy::too_many_arguments)]
pub fn draw_revision_row<R>(
    renderer: &mut R,
    bounds: Rectangle,
    frame_data: &LaneFrame,
    // Warped lane → display-column maps for this row and the previous one
    // (`LaneFrame::display_columns`). All x positions come from these; lane
    // *indices* — colors, dash kinds, emphasis — stay original. A lane
    // whose column differs between the rows slides over in the top half.
    columns: &[Option<usize>],
    prev_columns: &[Option<usize>],
    style: &RevisionGraphStyle,
    // Forces the node disc to this color instead of the lane color.
    // Used to paint the working-copy node in the accent (coral) so the
    // current branching point is unmistakable in the graph, regardless
    // of which lane it happens to live in. Edges still wear their lane
    // colors — only the disc gets overridden.
    node_color_override: Option<Color>,
    // When `Some(lane)`, the incoming (top-half) strokes for that lane
    // render thicker. Separate from `emphasized_lane_after` because a
    // merge can reuse a lane index for an unrelated outgoing branch,
    // and emphasizing both halves of such a lane would falsely highlight
    // the merged-in branch when the user is hovering the new branch.
    emphasized_lane_before: Option<usize>,
    // When `Some(lane)`, the outgoing (bottom-half) strokes for that
    // lane render thicker. See `emphasized_lane_before`.
    emphasized_lane_after: Option<usize>,
) where
    R: renderer::Renderer + geometry::Renderer,
{
    let top = bounds.y;
    let bot = bounds.y + bounds.height;
    let mid = bounds.y + bounds.height / 2.0;
    let col = |lane: usize| columns.get(lane).copied().flatten().unwrap_or(lane);
    let prev_col = |lane: usize| {
        prev_columns
            .get(lane)
            .copied()
            .flatten()
            .unwrap_or_else(|| col(lane))
    };
    let x_node = lane_x(bounds.x, col(frame_data.node_lane));

    // Indirect-edge dash pattern. Allocated once per row so the borrow
    // outlives the strokes; the pattern itself is just two `f32`s.
    let dash: [f32; 2] = [4.0, 3.0];

    // Allocate one geometry frame per row covering the whole row's bounds.
    // Using row-local coords would be cleaner but `Frame::translate` adds
    // complexity for little gain at this granularity.
    //
    // The frame must be wide enough for the *previous* row's columns too: a
    // lane sliding left out of a now-free column starts its bend at the old
    // x, which can sit beyond this row's (narrower) packed strip — sized to
    // this row alone, the slide's upper half clips away and the lane appears
    // chopped off mid-air.
    let rightmost_lane = columns
        .iter()
        .chain(prev_columns.iter())
        .copied()
        .flatten()
        .max();
    let frame_width = rightmost_lane.map_or(bounds.width, |lane| {
        bounds.width.max(lane_strip_width(lane + 1))
    });
    let mut frame = Frame::new(renderer, Size::new(bounds.x + frame_width, bot));

    // Incoming edges (top half). Each lane enters at the column it occupied
    // in the previous row and (when the warp shifts it) slides into this
    // row's column by mid-height.
    for (i, kind) in frame_data.before.iter().enumerate() {
        let Some(kind) = *kind else { continue };
        let x_top = lane_x(bounds.x, prev_col(i));
        let x_here = lane_x(bounds.x, col(i));
        let stroke = style.edge_stroke(kind, i, &dash, emphasized_lane_before == Some(i));

        if frame_data.merging_lanes.contains(&i) {
            // This lane terminates at the node — vertical from top, then
            // either continues straight (if it shares the node's column) or
            // curves into the node from the side.
            let path = Path::new(|builder| {
                draw_incoming(builder, x_top, x_node, top, mid);
            });
            frame.stroke(&path, stroke);
        } else if frame_data.is_pass_through(i) {
            // Pass-through: a single straight vertical when the column is
            // stable; an S-bend into the new column over the top half when
            // the warp moved it. (Pass-through means the lane isn't in
            // `merging_lanes` and both halves are alive, so the before/
            // after segments match — either emphasis flag is sufficient.)
            let path = if (x_top - x_here).abs() < 0.5 {
                Path::line(Point::new(x_here, top), Point::new(x_here, bot))
            } else {
                Path::new(|builder| {
                    draw_lane_shift(builder, x_top, x_here, top, mid);
                    builder.line_to(Point::new(x_here, bot));
                })
            };
            frame.stroke(&path, stroke);
        } else {
            // Lane terminates above the node without merging into it
            // (e.g., ended on the previous row). Just a half-line, sliding
            // if the warp shifted its column.
            let path = Path::new(|builder| {
                draw_lane_shift(builder, x_top, x_here, top, mid);
            });
            frame.stroke(&path, stroke);
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
        let x_j = lane_x(bounds.x, col(j));

        let path = Path::new(|builder| {
            draw_outgoing(builder, x_node, x_j, mid, bot);
        });
        // Color outgoing edges by the destination lane: a branch that
        // *opens* on lane 2 should already wear lane 2's color from the
        // moment it leaves the node, so the eye picks it up consistently
        // wherever the branch travels.
        frame.stroke(
            &path,
            style.edge_stroke(kind, j, &dash, emphasized_lane_after == Some(j)),
        );
    }

    // Elided-ancestry marker — jj log's `~` — goes into the same geometry
    // frame as the edges (and the disc that follows). iced renders geometry
    // on a layer above quad primitives, so a `fill_quad` here would visibly
    // sit *under* the stroked edges — order of *calls* doesn't matter,
    // layer does. Within a single frame the order *does* matter (later
    // draws on top), so adding it before the disc lets the disc cover any
    // overlap. Centered under the node when its lane ends here (the common
    // trailing-boundary read); nudged right when an edge continues below so
    // the squiggle doesn't sit on the line.
    if frame_data.missing_parents > 0 {
        let lane_continues = frame_data
            .after
            .get(frame_data.node_lane)
            .copied()
            .flatten()
            .is_some();
        let x = if lane_continues {
            x_node + NODE_RADIUS * 2.4
        } else {
            x_node
        };
        let y = (mid + NODE_RADIUS * 2.0 + 4.0).min(bot - 3.0);
        let half = ELIDED_SQUIGGLE_HALF_WIDTH;
        let amplitude = ELIDED_SQUIGGLE_AMPLITUDE;
        let squiggle = Path::new(|builder| {
            builder.move_to(Point::new(x - half, y));
            builder.bezier_curve_to(
                Point::new(x - half * 0.4, y - amplitude),
                Point::new(x + half * 0.4, y + amplitude),
                Point::new(x + half, y),
            );
        });
        let stroke = Stroke::default()
            .with_color(style.missing_color)
            .with_width(LINE_THICKNESS)
            .with_line_cap(LineCap::Round);
        frame.stroke(&squiggle, stroke);
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
    let disc_color = node_color_override.unwrap_or_else(|| style.lane_color(frame_data.node_lane));
    // The working copy (the only override caller) gets a soft halo behind
    // its disc so "you are here" reads even at a glance across the gutter.
    if node_color_override.is_some() {
        let halo = Path::new(|b| b.circle(Point::new(x_node, mid), NODE_RADIUS + 2.5));
        frame.fill(
            &halo,
            Color {
                a: 0.30,
                ..disc_color
            },
        );
    }
    let disc_path = Path::new(|b| b.circle(Point::new(x_node, mid), NODE_RADIUS));
    frame.fill(&disc_path, disc_color);

    renderer.draw_geometry(frame.into_geometry());
}

pub fn draw_continuation_row<R>(
    renderer: &mut R,
    bounds: Rectangle,
    lanes: &[Option<GraphEdgeType>],
    // Warped display columns of the parent revision row (the file rows sit
    // inside its `after` snapshot, so they inherit its packing as-is).
    columns: &[Option<usize>],
    style: &RevisionGraphStyle,
    emphasized_lane: Option<usize>,
) where
    R: renderer::Renderer + geometry::Renderer,
{
    let top = bounds.y;
    let bot = bounds.y + bounds.height;
    let dash: [f32; 2] = [4.0, 3.0];

    let mut frame = Frame::new(renderer, Size::new(bounds.x + bounds.width, bot));
    for (i, kind) in lanes.iter().enumerate() {
        let Some(kind) = *kind else { continue };
        let column = columns.get(i).copied().flatten().unwrap_or(i);
        let x = lane_x(bounds.x, column);
        let path = Path::line(Point::new(x, top), Point::new(x, bot));
        frame.stroke(
            &path,
            style.edge_stroke(kind, i, &dash, emphasized_lane == Some(i)),
        );
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

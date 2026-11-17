//! Painting for the revision-graph gutter. Called from
//! [`crate::revision_list`] inside its custom element's `paint`.
//!
//! Edges are stroked paths built with GPUI's [`PathBuilder`], which gives
//! rounded corners and a slight angle at the node: neither is reachable with
//! plain quads.
//!
//! Two shapes:
//! * [`revision_row`]: a row whose center hosts a node circle, with incoming
//!   edges in the top half and outgoing edges in the bottom half.
//! * [`continuation_row`]: a strip that just runs lanes straight through, used
//!   under expanded file rows so the gutter stays unbroken between one revision
//!   and the next.
//!
//! Rows add to a [`GraphFrame`] rather than painting directly. A frame holds one
//! path per stroke style, so a screenful of rows is a handful of paths instead
//! of one per edge, and every disc lands on top of every stroke.

use diffui_core::graph::LaneFrame;
use diffui_core::graph_layout::GraphEdgeType;
use gpui_kit::{BorderStyle, Bounds, PathBuilder, Pixels, Point, Window, point, px, quad, size};

use crate::theme::Color;

/// Lane spacing of a files-only list's strip: see [`LaneGeometry::strip`].
pub const LANE_WIDTH: f32 = 10.0;
const LINE_THICKNESS: f32 = 1.75;
const LINE_THICKNESS_EMPHASIZED: f32 = 2.75;
/// A mutable revision's disc, and the ring of the pane's colour around it
/// that keeps it apart from the strokes it sits on.
const NODE_RADIUS: f32 = 4.0;
const NODE_RING: f32 = 2.0;
/// An immutable revision: a small hollow ring.
const IMMUTABLE_RADIUS: f32 = 4.4;
const IMMUTABLE_RING: f32 = 1.75;
/// A conflicted revision: a larger red disc carrying a `!`.
const CONFLICT_RADIUS: f32 = 6.0;
/// The working copy: an accent disc with a centre dot and a soft halo.
const WORKING_COPY_RADIUS: f32 = 6.5;
const WORKING_COPY_DOT: f32 = 2.4;
pub const HALO_RADIUS: f32 = 10.0;
const HALO_ALPHA: f32 = 0.18;
/// Indirect edges are dashed, so they stay distinguishable when they share a
/// lane color with a direct edge.
const DASH: [Pixels; 2] = [px(4.0), px(3.0)];

/// Geometry of the elided-ancestry squiggle (jj log's `~`): drawn below a
/// node whose parent(s) sit outside the loaded set: the bottom of a filtered
/// revset, or the first sighting of a boundary commit. Half-width and
/// amplitude in px; small enough to read as graph punctuation, not data.
const ELIDED_SQUIGGLE_HALF_WIDTH: f32 = 3.5;
const ELIDED_SQUIGGLE_AMPLITUDE: f32 = 3.2;

#[derive(Debug, Clone, Copy)]
pub struct RevisionGraphStyle {
    /// The theme's lane colours, cycled by lane. A node always wears its
    /// own lane's colour, so the eye can trace a branch through both edges
    /// and commits.
    pub lanes: [Color; 4],
    /// Color used for the missing-parent stub. Doesn't participate in lane
    /// coloring because it isn't a real edge.
    pub missing_color: Color,
    /// Immutable rings: ink that names no branch.
    pub neutral: Color,
    /// The pane under the graph: the rings that cut nodes out of strokes.
    pub surface: Color,
    /// The working copy's disc and halo.
    pub accent: Color,
    /// The working copy's centre dot.
    pub accent_foreground: Color,
    /// A conflicted revision's disc.
    pub conflict: Color,
}

/// What a revision's node says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Mutable,
    /// In `immutable()`.
    Immutable,
    /// Its tree holds a conflict.
    Conflict,
    /// The working copy: the most visible node in the graph.
    WorkingCopy,
    /// A previewed ghost: where a moved or abandoned commit used to be.
    Ghost,
}

/// Where a list's lane columns sit: shared by every row so lanes line up from
/// row to row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LaneGeometry {
    /// Column 0's centre, from the row's left edge.
    pub first: f32,
    /// Distance between neighbouring columns.
    pub spacing: f32,
}

impl LaneGeometry {
    /// The strip of a files-only list: lane centres `LANE_WIDTH` apart past
    /// `left_padding`.
    pub fn strip(left_padding: f32) -> Self {
        Self {
            first: left_padding + LANE_WIDTH / 2.0,
            spacing: LANE_WIDTH,
        }
    }

    /// Centre of `column`, for a row whose left edge is at `origin`.
    pub fn x(&self, origin: f32, column: usize) -> f32 {
        origin + self.first + column as f32 * self.spacing
    }

    /// The column under `local_x` (from the row's left edge), when it falls
    /// within half a spacing of one. `columns` is how many there are.
    pub fn column_at(&self, local_x: f32, columns: usize) -> Option<usize> {
        (0..columns).find(|&column| (local_x - self.x(0.0, column)).abs() <= self.spacing / 2.0)
    }
}

impl RevisionGraphStyle {
    /// Color for a given lane, deterministic in `lane`: the palette repeats
    /// every four lanes, which keeps neighbours distinct without a rainbow.
    pub fn lane_color(&self, lane: usize) -> Color {
        self.lanes[lane % self.lanes.len()]
    }
}

/// The stroke a path is drawn with. One frame holds one path per distinct value.
#[derive(Debug, Clone, Copy, PartialEq)]
struct StrokeStyle {
    color: Color,
    width: f32,
    dashed: bool,
}

impl RevisionGraphStyle {
    /// Stroke for an edge of a given kind in a given lane. When `emphasized`
    /// it renders thicker: the lane under the cursor.
    fn edge_stroke(&self, kind: GraphEdgeType, lane: usize, emphasized: bool) -> StrokeStyle {
        StrokeStyle {
            color: match kind {
                GraphEdgeType::Missing => self.missing_color,
                _ => self.lane_color(lane),
            },
            width: if emphasized {
                LINE_THICKNESS_EMPHASIZED
            } else {
                LINE_THICKNESS
            },
            dashed: matches!(kind, GraphEdgeType::Indirect),
        }
    }
}

/// A node disc, painted after every stroke so it hides the messy convergence
/// of edges at its centre.
#[derive(Debug, Clone, Copy)]
struct Disc {
    x: f32,
    y: f32,
    radius: f32,
    fill: Color,
    border: Option<(Color, f32)>,
}

/// The gutter of the rows painted this frame.
#[derive(Default)]
pub struct GraphFrame {
    strokes: Vec<(StrokeStyle, PathBuilder)>,
    discs: Vec<Disc>,
    /// Rounded bars painted over the discs.
    marks: Vec<(Bounds<Pixels>, Color)>,
}

impl GraphFrame {
    /// A rounded bar `width` wide from `top` to `bottom`, centred on `x`.
    fn bar(&mut self, x: f32, top: f32, bottom: f32, width: f32, color: Color) {
        self.marks.push((
            Bounds::new(
                point(px(x - width / 2.0), px(top)),
                size(px(width), px(bottom - top)),
            ),
            color,
        ));
    }

    fn disc(&mut self, x: f32, y: f32, radius: f32, fill: Color, border: Option<(Color, f32)>) {
        self.discs.push(Disc {
            x,
            y,
            radius,
            fill,
            border,
        });
    }
}

impl GraphFrame {
    fn path(&mut self, style: StrokeStyle) -> &mut PathBuilder {
        let index = match self.strokes.iter().position(|(have, _)| *have == style) {
            Some(index) => index,
            None => {
                let mut builder = PathBuilder::stroke(px(style.width));
                if style.dashed {
                    builder = builder.dash_array(&DASH);
                }
                self.strokes.push((style, builder));
                self.strokes.len() - 1
            }
        };
        &mut self.strokes[index].1
    }

    /// Paints what the rows added: strokes first, discs over them.
    pub fn paint(self, window: &mut Window) {
        for (style, builder) in self.strokes {
            match builder.build() {
                Ok(path) => window.paint_path(path, style.color),
                Err(error) => tracing::warn!("the graph's path didn't tessellate: {error}"),
            }
        }
        for disc in self.discs {
            let bounds: Bounds<Pixels> = Bounds::new(
                point(px(disc.x - disc.radius), px(disc.y - disc.radius)),
                size(px(disc.radius * 2.0), px(disc.radius * 2.0)),
            );
            let (border_color, border_width) = disc.border.unwrap_or((disc.fill, 0.0));
            window.paint_quad(quad(
                bounds,
                px(disc.radius),
                disc.fill,
                px(border_width),
                border_color,
                BorderStyle::Solid,
            ));
        }
        // Marks drawn on the discs: a conflict's `!`.
        for (bounds, color) in self.marks {
            let radius = f32::from(bounds.size.width.min(bounds.size.height)) / 2.0;
            window.paint_quad(quad(
                bounds,
                px(radius),
                color,
                px(0.0),
                color,
                BorderStyle::Solid,
            ));
        }
    }
}

/// Width in pixels needed to render a row with `lane_count` lanes in a
/// [`LaneGeometry::strip`].
pub fn lane_strip_width(lane_count: usize) -> f32 {
    lane_count as f32 * LANE_WIDTH
}

fn at(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
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

/// Corner radius as a fraction of the lane spacing. Bigger = softer, more
/// rounded turn; smaller = closer to a true square corner. Half the
/// lane width is the practical ceiling: anything more starts to crowd
/// the adjacent lane's vertical line. This value sits just under that
/// to give a noticeably smooth turn while keeping a small straight stub
/// of the tilted leg visible before the arc takes over.
const CORNER_RADIUS_FRAC: f32 = 0.5;

/// A rounded corner: where it enters and leaves its legs.
#[derive(Debug, Clone, Copy)]
struct CornerArc {
    enter: (f32, f32),
    leave: (f32, f32),
    clockwise: bool,
}

/// Where a circular arc of `radius` tangent to both `from -> corner` and
/// `corner -> to` touches them, and whether it sweeps clockwise on screen.
/// `None` when the three points don't make a corner to round.
fn corner_arc(
    from: (f32, f32),
    corner: (f32, f32),
    to: (f32, f32),
    radius: f32,
) -> Option<CornerArc> {
    let unit = |(x, y): (f32, f32)| {
        let length = (x * x + y * y).sqrt();
        (length > f32::EPSILON).then(|| (x / length, y / length))
    };
    let back = unit((from.0 - corner.0, from.1 - corner.1))?;
    let onward = unit((to.0 - corner.0, to.1 - corner.1))?;
    let cos = (back.0 * onward.0 + back.1 * onward.1).clamp(-1.0, 1.0);
    let half_angle = cos.acos() / 2.0;
    let tan = half_angle.tan();
    if radius <= 0.0 || tan.abs() < f32::EPSILON || half_angle >= std::f32::consts::FRAC_PI_2 - 1e-3
    {
        return None;
    }
    let reach = radius / tan;
    let enter = (corner.0 + back.0 * reach, corner.1 + back.1 * reach);
    let leave = (corner.0 + onward.0 * reach, corner.1 + onward.1 * reach);
    // Heading along `-back`, then turning onto `onward`. y grows downwards.
    let clockwise = -back.0 * onward.1 + back.1 * onward.0 > 0.0;
    Some(CornerArc {
        enter,
        leave,
        clockwise,
    })
}

/// Canvas-style `arcTo`: a line towards `corner`, rounded onto the way to `to`.
fn round_corner(
    builder: &mut PathBuilder,
    from: (f32, f32),
    corner: (f32, f32),
    to: (f32, f32),
    radius: f32,
) {
    match corner_arc(from, corner, to, radius) {
        Some(CornerArc {
            enter,
            leave,
            clockwise,
        }) => {
            builder.line_to(at(enter.0, enter.1));
            builder.arc_to(
                at(radius, radius),
                px(0.0),
                false,
                clockwise,
                at(leave.0, leave.1),
            );
        }
        None => builder.line_to(at(corner.0, corner.1)),
    }
}

/// An outgoing connector from the node at `(x_node, y_mid)` down to
/// `(x_target, y_bot)`. Same-lane edges get a plain vertical; cross-lane
/// edges get the classic L-bend with two refinements:
///
/// * The horizontal-ish leg is tilted `NODE_TILT_DEG` off horizontal, so
///   the line *droops* slightly as it travels from the node to the
///   target column instead of running perfectly flat. The tilt is what
///   visually separates branches that reuse a lane.
/// * The corner where the tilted leg meets the vertical drop is rounded.
///
/// The line runs all the way to `(x_node, y_mid)`, the disc centre.
/// Anything inside the disc area gets covered when the disc is painted
/// on top, so the messy convergence of multiple edges at the centre is
/// hidden by the disc rather than fanned out around it.
fn outgoing(
    builder: &mut PathBuilder,
    x_node: f32,
    x_target: f32,
    y_mid: f32,
    y_bot: f32,
    spacing: f32,
) {
    builder.move_to(at(x_node, y_mid));
    if (x_target - x_node).abs() < 0.5 {
        builder.line_to(at(x_node, y_bot));
        return;
    }

    let dx = (x_target - x_node).abs();
    // Where the tilted horizontal would intersect the target column.
    // `tan(NODE_TILT_DEG)` per pixel of horizontal travel: for 10° that's
    // ~17.6% of dx as the vertical drop, so a one-lane jump dips only a
    // couple of pixels before the corner, keeping the leg unmistakably
    // "horizontal".
    let natural_corner_y = y_mid + dx * NODE_TILT_DEG.to_radians().tan();
    // Keep the corner inside the row with room left for the arc. On wide
    // graphs (many concurrent lanes) the natural 10° tilt would overshoot
    // the row bottom and bleed into the next revision. Clamping flattens
    // the effective tilt so the line reads as a long diagonal into the
    // target column instead.
    let max_corner_y = (y_bot - spacing * CORNER_RADIUS_FRAC * 0.5).max(y_mid);
    let corner_y = natural_corner_y.min(max_corner_y);
    let radius = (spacing * CORNER_RADIUS_FRAC)
        .min((y_bot - corner_y) * 0.5)
        .max(0.0);

    round_corner(
        builder,
        (x_node, y_mid),
        (x_target, corner_y),
        (x_target, y_bot),
        radius,
    );
    builder.line_to(at(x_target, y_bot));
}

/// Smooth S-bend for a lane whose warped column changed between rows: the
/// line leaves the previous row's column at the row top and eases into this
/// row's column by mid-height, with vertical tangents at both ends. Unlike
/// the tilted node connectors there is no node to point at here. The curve
/// should read as the same vertical lane drifting sideways, not as a branch,
/// so it gets a symmetric ease instead of the tilt + corner treatment.
fn lane_shift(builder: &mut PathBuilder, x_from: f32, x_to: f32, y_top: f32, y_mid: f32) {
    builder.move_to(at(x_from, y_top));
    if (x_to - x_from).abs() < 0.5 {
        builder.line_to(at(x_from, y_mid));
        return;
    }
    // Control points at half height in each column: the standard
    // vertical-tangent cubic, so the bend enters and leaves perfectly
    // upright and adjacent shifting lanes stay parallel through the turn.
    let y_blend = (y_top + y_mid) / 2.0;
    builder.cubic_bezier_to(at(x_to, y_mid), at(x_from, y_blend), at(x_to, y_blend));
}

/// Mirror of [`outgoing`] for edges arriving at the node from above: vertical
/// drop in the source column, rounded corner, tilted approach to the node
/// centre. As with [`outgoing`], the path runs to the disc centre and the disc
/// covers the convergence.
fn incoming(
    builder: &mut PathBuilder,
    x_source: f32,
    x_node: f32,
    y_top: f32,
    y_mid: f32,
    spacing: f32,
) {
    if (x_node - x_source).abs() < 0.5 {
        builder.move_to(at(x_node, y_top));
        builder.line_to(at(x_node, y_mid));
        return;
    }

    let dx = (x_node - x_source).abs();
    let natural_corner_y = y_mid - dx * NODE_TILT_DEG.to_radians().tan();
    // Mirror of `outgoing`: keep the corner inside the row with room for the
    // arc, so wide graphs don't push the path above `y_top`.
    let min_corner_y = (y_top + spacing * CORNER_RADIUS_FRAC * 0.5).min(y_mid);
    let corner_y = natural_corner_y.max(min_corner_y);
    let radius = (spacing * CORNER_RADIUS_FRAC)
        .min((corner_y - y_top) * 0.5)
        .max(0.0);

    builder.move_to(at(x_source, y_top));
    round_corner(
        builder,
        (x_source, y_top),
        (x_source, corner_y),
        (x_node, y_mid),
        radius,
    );
    builder.line_to(at(x_node, y_mid));
}

/// How one revision row's gutter is drawn.
pub struct RevisionRow<'a> {
    pub frame_data: &'a LaneFrame,
    /// Lane -> display-column maps for this row and the previous one
    /// (`LaneFrame::display_columns`). All x positions
    /// come from these; lane *indices* (colors, dash kinds, emphasis) stay
    /// original. A lane whose column differs between the rows slides over in
    /// the top half.
    pub columns: &'a [Option<usize>],
    pub prev_columns: &'a [Option<usize>],
    pub geometry: &'a LaneGeometry,
    pub node: NodeKind,
    /// When `Some(lane)`, the incoming (top-half) strokes for that lane render
    /// thicker. Separate from `emphasized_lane_after` because a merge can reuse
    /// a lane index for an unrelated outgoing branch, and emphasizing both
    /// halves of such a lane would falsely highlight the merged-in branch when
    /// the user is hovering the new branch.
    pub emphasized_lane_before: Option<usize>,
    /// When `Some(lane)`, the outgoing (bottom-half) strokes for that lane
    /// render thicker. See `emphasized_lane_before`.
    pub emphasized_lane_after: Option<usize>,
}

/// Adds a revision row to `frame`. `x`, `top` and `height` place the row, in
/// window coordinates; `x` is its left edge.
pub fn revision_row(
    frame: &mut GraphFrame,
    x: f32,
    top: f32,
    height: f32,
    row: &RevisionRow<'_>,
    style: &RevisionGraphStyle,
) {
    let frame_data = row.frame_data;
    let geometry = row.geometry;
    let spacing = geometry.spacing;
    let bot = top + height;
    let mid = top + height / 2.0;
    let col = |lane: usize| row.columns.get(lane).copied().flatten().unwrap_or(lane);
    let prev_col = |lane: usize| {
        row.prev_columns
            .get(lane)
            .copied()
            .flatten()
            .unwrap_or_else(|| col(lane))
    };
    let lane_x = |column: usize| geometry.x(x, column);
    let x_node = lane_x(col(frame_data.node_lane));

    // Incoming edges (top half). Each lane enters at the column it occupied
    // in the previous row and (when the warp shifts it) slides into this
    // row's column by mid-height.
    for (lane, kind) in frame_data.before.iter().enumerate() {
        let Some(kind) = *kind else { continue };
        let x_top = lane_x(prev_col(lane));
        let x_here = lane_x(col(lane));
        let stroke = style.edge_stroke(kind, lane, row.emphasized_lane_before == Some(lane));
        let path = frame.path(stroke);

        if frame_data.merging_lanes.contains(&lane) {
            // This lane terminates at the node: vertical from top, then
            // either continues straight (if it shares the node's column) or
            // curves into the node from the side.
            incoming(path, x_top, x_node, top, mid, spacing);
        } else if frame_data.is_pass_through(lane) {
            // Pass-through: a single straight vertical when the column is
            // stable; an S-bend into the new column over the top half when
            // the warp moved it. (Pass-through means the lane isn't in
            // `merging_lanes` and both halves are alive, so the before/
            // after segments match: either emphasis flag is sufficient.)
            if (x_top - x_here).abs() < 0.5 {
                path.move_to(at(x_here, top));
            } else {
                lane_shift(path, x_top, x_here, top, mid);
            }
            path.line_to(at(x_here, bot));
        } else {
            // Lane terminates above the node without merging into it
            // (e.g., ended on the previous row). Just a half-line, sliding
            // if the warp shifted its column.
            lane_shift(path, x_top, x_here, top, mid);
        }
    }

    // Outgoing edges (bottom half).
    for (lane, kind) in frame_data.after.iter().enumerate() {
        let Some(kind) = *kind else { continue };
        if frame_data.is_pass_through(lane) {
            // Already drawn above as part of the incoming pass: avoid
            // double-stroking.
            continue;
        }
        // Color outgoing edges by the destination lane: a branch that
        // *opens* on lane 2 should already wear lane 2's color from the
        // moment it leaves the node, so the eye picks it up consistently
        // wherever the branch travels.
        let stroke = style.edge_stroke(kind, lane, row.emphasized_lane_after == Some(lane));
        outgoing(
            frame.path(stroke),
            x_node,
            lane_x(col(lane)),
            mid,
            bot,
            spacing,
        );
    }

    // Elided-ancestry marker: jj log's `~`. Centered under the node when its
    // lane ends here (the common trailing-boundary read); nudged right when
    // an edge continues below so the squiggle doesn't sit on the line.
    if frame_data.missing_parents > 0 {
        let lane_continues = frame_data
            .after
            .get(frame_data.node_lane)
            .copied()
            .flatten()
            .is_some();
        let squiggle_x = if lane_continues {
            x_node + NODE_RADIUS * 2.4
        } else {
            x_node
        };
        let y = (mid + NODE_RADIUS * 2.0 + 4.0).min(bot - 3.0);
        let half = ELIDED_SQUIGGLE_HALF_WIDTH;
        let amplitude = ELIDED_SQUIGGLE_AMPLITUDE;
        let path = frame.path(StrokeStyle {
            color: style.missing_color,
            width: LINE_THICKNESS,
            dashed: false,
        });
        path.move_to(at(squiggle_x - half, y));
        path.cubic_bezier_to(
            at(squiggle_x + half, y),
            at(squiggle_x - half * 0.4, y - amplitude),
            at(squiggle_x + half * 0.4, y + amplitude),
        );
    }

    // The disc wears the colour of the node's own lane so a branch reads as
    // a single continuous hue from edge to commit to edge. For merge nodes
    // (multiple parents) the lane-assignment first-parent rule means
    // `node_lane` is also the first parent's lane, so this picks the closest
    // possible match in hue.
    let lane_color = style.lane_color(frame_data.node_lane);
    let surface = style.surface;
    match row.node {
        NodeKind::WorkingCopy => {
            frame.disc(
                x_node,
                mid,
                HALO_RADIUS,
                Color {
                    a: style.accent.a * HALO_ALPHA,
                    ..style.accent
                },
                None,
            );
            frame.disc(x_node, mid, WORKING_COPY_RADIUS, style.accent, None);
            frame.disc(x_node, mid, WORKING_COPY_DOT, style.accent_foreground, None);
        }
        NodeKind::Ghost => frame.disc(
            x_node,
            mid,
            NODE_RADIUS,
            surface,
            Some((
                Color {
                    a: 0.7,
                    ..lane_color
                },
                1.25,
            )),
        ),
        NodeKind::Conflict => {
            frame.disc(
                x_node,
                mid,
                CONFLICT_RADIUS + NODE_RING,
                style.conflict,
                Some((surface, NODE_RING)),
            );
            // A `!`: a short bar over a dot, in the pane's colour.
            frame.bar(x_node, mid - 4.2, mid + 1.2, 2.0, surface);
            frame.bar(x_node, mid + 1.8, mid + 3.8, 2.0, surface);
        }
        NodeKind::Immutable => frame.disc(
            x_node,
            mid,
            IMMUTABLE_RADIUS,
            surface,
            Some((style.neutral, IMMUTABLE_RING)),
        ),
        NodeKind::Mutable => frame.disc(
            x_node,
            mid,
            NODE_RADIUS + NODE_RING,
            lane_color,
            Some((surface, NODE_RING)),
        ),
    }
}

/// Adds a row of straight lanes to `frame`. `columns` are the display columns
/// of the parent revision row (the file rows sit inside its `after`
/// snapshot, so they inherit its packing as-is).
#[allow(clippy::too_many_arguments)]
pub fn continuation_row(
    frame: &mut GraphFrame,
    x: f32,
    top: f32,
    height: f32,
    lanes: &[Option<GraphEdgeType>],
    columns: &[Option<usize>],
    geometry: &LaneGeometry,
    style: &RevisionGraphStyle,
    emphasized_lane: Option<usize>,
) {
    for (lane, kind) in lanes.iter().enumerate() {
        let Some(kind) = *kind else { continue };
        let column = columns.get(lane).copied().flatten().unwrap_or(lane);
        let lane_center = geometry.x(x, column);
        let path = frame.path(style.edge_stroke(kind, lane, emphasized_lane == Some(lane)));
        path.move_to(at(lane_center, top));
        path.line_to(at(lane_center, top + height));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rounded_corner_touches_both_legs_at_the_same_distance() {
        let arc = corner_arc((0.0, 0.0), (10.0, 0.0), (10.0, 10.0), 2.0).expect("a corner");
        // A right angle: the tangent points sit one radius from the corner.
        assert!((arc.enter.0 - 8.0).abs() < 1e-4 && arc.enter.1.abs() < 1e-4);
        assert!((arc.leave.0 - 10.0).abs() < 1e-4 && (arc.leave.1 - 2.0).abs() < 1e-4);
        // Heading right, then down, is clockwise on a y-down screen.
        assert!(arc.clockwise);
        let arc = corner_arc((10.0, 0.0), (0.0, 0.0), (0.0, 10.0), 2.0).expect("a corner");
        assert!(!arc.clockwise);
    }

    #[test]
    fn a_straight_line_has_no_corner_to_round() {
        assert!(corner_arc((0.0, 0.0), (5.0, 0.0), (10.0, 0.0), 2.0).is_none());
        assert!(corner_arc((5.0, 0.0), (5.0, 0.0), (10.0, 0.0), 2.0).is_none());
        assert!(corner_arc((0.0, 0.0), (5.0, 0.0), (5.0, 5.0), 0.0).is_none());
    }

    #[test]
    fn a_strip_puts_lanes_where_the_fixed_width_did() {
        let strip = LaneGeometry::strip(8.0);
        assert_eq!(strip.x(100.0, 0), 113.0);
        assert_eq!(strip.x(100.0, 2), 133.0);
    }

    #[test]
    fn a_column_hit_tests_by_its_centre() {
        let geometry = LaneGeometry {
            first: 16.0,
            spacing: 13.0,
        };
        assert_eq!(geometry.x(0.0, 2), 42.0);
        assert_eq!(geometry.column_at(37.0, 4), Some(2));
        assert_eq!(geometry.column_at(3.0, 4), None);
    }

    #[test]
    fn lane_colors_cycle_through_the_palette() {
        let color = |r: f32| Color {
            r,
            g: 0.2,
            b: 0.8,
            a: 1.0,
        };
        let black = crate::theme::BLACK;
        let style = RevisionGraphStyle {
            lanes: [color(0.1), color(0.2), color(0.3), color(0.4)],
            missing_color: black,
            neutral: black,
            surface: black,
            accent: black,
            accent_foreground: black,
            conflict: black,
        };
        assert_eq!(style.lane_color(0), color(0.1));
        assert_eq!(style.lane_color(5), style.lane_color(1));
        assert_ne!(style.lane_color(1), style.lane_color(2));
    }
}

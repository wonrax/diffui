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

use crate::theme::Color;

/// Horizontal width occupied by one lane in the gutter. `lane_x` centers
/// the node disc in the middle of its lane, so adjacent lanes sit at
/// `LANE_WIDTH` apart. Exposed because the revision list reserves
/// `lanes * LANE_WIDTH` of horizontal space for the gutter, and sidebar
/// layout math needs the same number to size the path column.
pub const LANE_WIDTH: f32 = 10.0;

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
}

/// Width in pixels needed to render a row with `lane_count` lanes.
pub fn lane_strip_width(lane_count: usize) -> f32 {
    lane_count as f32 * LANE_WIDTH
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

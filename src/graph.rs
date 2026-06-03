//! Lane assignment for rendering a topologically ordered DAG with one
//! commit per row and edges drawn in the gutter to the left.
//!
//! The input is a sequence of nodes already in jj's topo-grouped order
//! (descendants first; head branches grouped together). For each node we
//! assign a lane index and snapshot the lane state above and below the
//! row. A renderer turns those snapshots into edge segments — this module
//! only does the bookkeeping.
//!
//! Lane rules, matching git/jj defaults with a first-parent spine:
//!
//! * The first non-`Missing` parent inherits the row's own lane (the
//!   "continuation"). Other parents open new lanes to the right.
//! * Multiple incoming lanes targeting the same commit collapse into the
//!   leftmost of those lanes, which becomes the row's lane.
//! * A head (no incoming lanes) takes the leftmost free lane, allocating
//!   a new one if needed.
//! * `Missing` edges do not reserve a lane; they are reported per-row so
//!   the renderer can draw a stub if it wants.

use jj_lib::graph::{GraphEdge, GraphEdgeType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot<Id> {
    Empty,
    Awaiting { target: Id, kind: GraphEdgeType },
}

impl<Id> Slot<Id> {
    fn awaits(&self, id: &Id) -> bool
    where
        Id: Eq,
    {
        matches!(self, Self::Awaiting { target, .. } if target == id)
    }

    pub fn kind(&self) -> Option<GraphEdgeType> {
        match self {
            Self::Empty => None,
            Self::Awaiting { kind, .. } => Some(*kind),
        }
    }
}

/// Renderer-friendly per-row lane snapshot produced by [`LaneAssigner`]: the
/// edge kind occupying each lane just before and after this row's node, the
/// node's own lane, which incoming lanes merge into it, and how many parents
/// were missing. It drops the per-slot target id the assigner tracks
/// internally, so it's compact (one byte per lane, not a 32-byte commit id)
/// and cheap to store for every commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneFrame {
    pub before: Vec<Option<GraphEdgeType>>,
    pub after: Vec<Option<GraphEdgeType>>,
    pub node_lane: usize,
    pub merging_lanes: Vec<usize>,
    pub missing_parents: usize,
}

impl LaneFrame {
    /// Width in lanes for layout: covers all occupied lane indices plus
    /// the node lane.
    pub fn lane_count(&self) -> usize {
        self.before
            .len()
            .max(self.after.len())
            .max(self.node_lane + 1)
    }

    /// True if lane `i` runs straight through this row uninterrupted by the
    /// node — same edge kind in `before` and `after`, not the node lane, and
    /// not part of an incoming merge.
    pub fn is_pass_through(&self, lane: usize) -> bool {
        if lane == self.node_lane || self.merging_lanes.contains(&lane) {
            return false;
        }
        let before = self.before.get(lane).copied().flatten();
        let after = self.after.get(lane).copied().flatten();
        before.is_some() && before == after
    }

    /// A trivial "head with no parents" frame. Used as a placeholder for code
    /// paths that don't have real topology yet (e.g. the git loader).
    pub fn solo() -> Self {
        Self {
            before: Vec::new(),
            after: Vec::new(),
            node_lane: 0,
            merging_lanes: Vec::new(),
            missing_parents: 0,
        }
    }
}

/// Incremental lane assigner. Feed nodes in topo order (descendants first) one
/// at a time via [`LaneAssigner::push`]; it keeps the running lane state and
/// returns one compact [`LaneFrame`] per node. Because the state persists
/// across calls, it composes with a streaming loader — assign a batch, ship it,
/// keep going — instead of needing the whole graph up front.
///
/// This kills the old per-row `LaneRow<CommitId>` intermediate, which cloned a
/// 32-byte commit id into every lane of every row (~75GB across nixpkgs before
/// it even reached the store). It emits only compact frames; the sole id-bearing
/// copy is the single running `lanes` vec. Width is still capped in
/// [`allocate_lane`] — see there — because the stored frames and the sidebar
/// fold remain O(rows × width) even without the intermediate.
pub struct LaneAssigner<Id> {
    lanes: Vec<Slot<Id>>,
    /// Width cap; see [`MAX_LANES`]. Held per-assigner (not read from the const
    /// directly) so the profiler can run the identical algorithm uncapped to
    /// measure a graph's true width.
    max_lanes: usize,
}

impl<Id> Default for LaneAssigner<Id> {
    fn default() -> Self {
        Self {
            lanes: Vec::new(),
            max_lanes: MAX_LANES,
        }
    }
}

impl<Id: Clone + Eq> LaneAssigner<Id> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Uncapped assigner for width profiling — lets the lane count grow to the
    /// graph's true width instead of clamping at [`MAX_LANES`].
    #[cfg(feature = "track-alloc")]
    pub fn uncapped() -> Self {
        Self {
            lanes: Vec::new(),
            max_lanes: usize::MAX,
        }
    }

    /// Process one node and return its lane frame, advancing the running state.
    pub fn push(&mut self, id: &Id, edges: &[GraphEdge<Id>]) -> LaneFrame {
        let before = self.lanes.iter().map(Slot::kind).collect();

        let merging_lanes: Vec<usize> = self
            .lanes
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.awaits(id).then_some(i))
            .collect();

        let node_lane = if let Some(&first) = merging_lanes.first() {
            for &lane in &merging_lanes[1..] {
                self.lanes[lane] = Slot::Empty;
            }
            first
        } else {
            allocate_lane(&mut self.lanes, self.max_lanes)
        };

        self.lanes[node_lane] = Slot::Empty;

        let mut continued = false;
        let mut missing_parents = 0;
        for edge in edges {
            match edge.edge_type {
                GraphEdgeType::Missing => {
                    missing_parents += 1;
                }
                kind @ (GraphEdgeType::Direct | GraphEdgeType::Indirect) => {
                    let slot = Slot::Awaiting {
                        target: edge.target.clone(),
                        kind,
                    };
                    if !continued {
                        self.lanes[node_lane] = slot;
                        continued = true;
                    } else {
                        let lane = allocate_lane(&mut self.lanes, self.max_lanes);
                        self.lanes[lane] = slot;
                    }
                }
            }
        }

        while matches!(self.lanes.last(), Some(Slot::Empty)) {
            self.lanes.pop();
        }

        let after = self.lanes.iter().map(Slot::kind).collect();

        LaneFrame {
            before,
            after,
            node_lane,
            merging_lanes,
            missing_parents,
        }
    }
}

/// Batch convenience over [`LaneAssigner`]: assign lanes for a whole sequence
/// at once, one [`LaneFrame`] per node in input order. Used by loaders that
/// already hold the full node list (e.g. the git backend); the jj backend
/// drives the assigner directly in its load loop to avoid the intermediate.
pub fn assign_lanes<Id, I>(nodes: I) -> Vec<LaneFrame>
where
    Id: Clone + Eq,
    I: IntoIterator<Item = (Id, Vec<GraphEdge<Id>>)>,
{
    let mut assigner = LaneAssigner::new();
    nodes
        .into_iter()
        .map(|(id, edges)| assigner.push(&id, &edges))
        .collect()
}

/// Render-time cap on concurrent lanes. Per-row lane state is stored for every
/// commit — the compact `LaneFrame`s and (far heavier) the sidebar fold's
/// `RowLaneData`: ~5 width-wide Vecs per row, ~96 B per lane per row — so the
/// graph costs O(rows × width). Measured, nixpkgs runs a *median* of ~1200
/// concurrent lanes across its 1.1M rows (max 1714), so uncapped storage is
/// ~100GB. 128 is chosen so ordinary repos can show a wide graph when the
/// sidebar is dragged out (32 felt cramped); note that on nixpkgs-scale this
/// fold is still ~13GB and will OOM on load. The real fix is the segment-based
/// lane model — store each lane once as a start→end span, O(edges) ≈ ~30MB —
/// which turns this into a pure render-time concern. Beyond the cap, extra
/// branches share the last lane.
const MAX_LANES: usize = 128;

fn allocate_lane<Id>(lanes: &mut Vec<Slot<Id>>, max_lanes: usize) -> usize {
    if let Some(idx) = lanes.iter().position(|slot| matches!(slot, Slot::Empty)) {
        idx
    } else if lanes.len() < max_lanes {
        lanes.push(Slot::Empty);
        lanes.len() - 1
    } else {
        max_lanes - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct(c: char) -> GraphEdge<char> {
        GraphEdge::direct(c)
    }

    fn indirect(c: char) -> GraphEdge<char> {
        GraphEdge::indirect(c)
    }

    fn missing(c: char) -> GraphEdge<char> {
        GraphEdge::missing(c)
    }

    /// Drive the assigner over a node list, pairing each input id with the
    /// frame it produced (frames themselves drop the id). Goes through the
    /// public `assign_lanes` so the batch wrapper is covered too.
    fn run(
        nodes: impl IntoIterator<Item = (char, Vec<GraphEdge<char>>)>,
    ) -> Vec<(char, LaneFrame)> {
        let nodes: Vec<_> = nodes.into_iter().collect();
        let ids: Vec<char> = nodes.iter().map(|(id, _)| *id).collect();
        ids.into_iter().zip(assign_lanes(nodes)).collect()
    }

    fn lanes(rows: &[(char, LaneFrame)]) -> Vec<(char, usize)> {
        rows.iter()
            .map(|(id, frame)| (*id, frame.node_lane))
            .collect()
    }

    /// Render the lane state of every row as a textual sketch:
    /// `<lanes_after>  (<node_lane>)` where each lane is `.` (empty),
    /// `|`/`:`/`~` for the awaiting edge kind, and the row's own column
    /// holds the node id.
    fn sketch(rows: &[(char, LaneFrame)]) -> String {
        let mut out = String::new();
        for (id, frame) in rows {
            let width = frame.after.len().max(frame.node_lane + 1);
            let mut line = vec!['.'; width];
            for (i, slot) in frame.after.iter().enumerate() {
                line[i] = match slot {
                    None => '.',
                    Some(GraphEdgeType::Direct) => '|',
                    Some(GraphEdgeType::Indirect) => ':',
                    Some(GraphEdgeType::Missing) => '~',
                };
            }
            // Mark the node position by replacing whatever sits in node_lane.
            line[frame.node_lane] = *id;
            out.push_str(&format!(
                "{}  ({})\n",
                line.iter().collect::<String>(),
                frame.node_lane
            ));
        }
        out
    }

    #[test]
    fn linear_history_stays_in_lane_zero() {
        // C -> B -> A
        let rows = run([
            ('C', vec![direct('B')]),
            ('B', vec![direct('A')]),
            ('A', vec![]),
        ]);
        assert_eq!(lanes(&rows), vec![('C', 0), ('B', 0), ('A', 0)]);
    }

    #[test]
    fn merge_keeps_first_parent_on_node_lane() {
        // E merges C (first) and D. After E we expect lane 0 = C, lane 1 = D.
        // Topo order picks the second parent's branch first.
        let rows = run([
            ('F', vec![direct('E')]),
            ('E', vec![direct('C'), direct('D')]),
            ('D', vec![direct('B')]),
            ('B', vec![direct('A')]),
            ('C', vec![direct('A')]),
            ('A', vec![]),
        ]);
        assert_eq!(
            lanes(&rows),
            vec![('F', 0), ('E', 0), ('D', 1), ('B', 1), ('C', 0), ('A', 0),],
        );
        // After E two lanes stay open. The frame drops which commit each
        // awaits (that's the memory win); the targets are pinned anyway by C
        // and D landing on lanes 0 and 1 in the `lanes` assertion above.
        assert_eq!(
            rows[1].1.after,
            vec![Some(GraphEdgeType::Direct), Some(GraphEdgeType::Direct)]
        );
    }

    #[test]
    fn mega_merge_pushes_working_copy_to_lane_one() {
        // `jj log -r ::mm` style. mm has trunk-tip T as first parent and
        // working copy @ as second parent. trunk continues on lane 0; @
        // lives on lane 1 until it folds back into trunk at A.
        //
        //   mm
        //   ├─╮
        //   T │   <- trunk tip
        //   │ @
        //   │ │
        //   A     <- shared root, where @ joins back
        let rows = run([
            ('M', vec![direct('T'), direct('W')]), // M = mega merge, W = @
            ('T', vec![direct('A')]),
            ('W', vec![direct('A')]),
            ('A', vec![]),
        ]);
        assert_eq!(lanes(&rows), vec![('M', 0), ('T', 0), ('W', 1), ('A', 0)],);
        // After A is processed lane 1 should have collapsed.
        assert!(rows[3].1.after.is_empty());
        // A merges two lanes (0 and 1) into 0.
        assert_eq!(rows[3].1.merging_lanes, vec![0, 1]);
    }

    #[test]
    fn side_branch_takes_a_new_lane_and_releases_it_when_it_ends() {
        // D --> B
        //  \--> C --> A
        //  B's parent A is the same as C's eventual parent.
        let rows = run([
            ('D', vec![direct('B'), direct('C')]),
            ('C', vec![direct('A')]),
            ('B', vec![direct('A')]),
            ('A', vec![]),
        ]);
        assert_eq!(lanes(&rows), vec![('D', 0), ('C', 1), ('B', 0), ('A', 0)],);
        assert!(rows[3].1.after.is_empty());
    }

    #[test]
    fn missing_parent_does_not_reserve_a_lane() {
        let rows = run([('B', vec![missing('X')]), ('A', vec![])]);
        assert_eq!(rows[0].1.node_lane, 0);
        assert_eq!(rows[0].1.missing_parents, 1);
        assert!(rows[0].1.after.is_empty());
        // A is treated as a fresh head and takes lane 0 again.
        assert_eq!(rows[1].1.node_lane, 0);
    }

    #[test]
    fn indirect_edge_is_distinguishable_from_direct_in_slot() {
        let rows = run([('C', vec![indirect('A')]), ('A', vec![])]);
        // The frame keeps the edge KIND (so indirect strokes still render
        // dashed) even though it drops the 'A' target the assigner matched on.
        assert_eq!(rows[0].1.after[0], Some(GraphEdgeType::Indirect));
    }

    #[test]
    fn sketch_renders_a_simple_dag_for_eyeballing() {
        // Same setup as `mega_merge_pushes_working_copy_to_lane_one`.
        let rows = run([
            ('M', vec![direct('T'), direct('W')]),
            ('T', vec![direct('A')]),
            ('W', vec![direct('A')]),
            ('A', vec![]),
        ]);
        let expected = "\
M|  (0)
T|  (0)
|W  (1)
A  (0)
";
        assert_eq!(sketch(&rows), expected);
    }
}

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

use std::hash::Hash;

use jj_lib::graph::{GraphEdge, GraphEdgeType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRow<Id> {
    pub id: Id,
    pub node_lane: usize,
    /// Lane state immediately before this row's node is processed.
    /// Equal to the previous row's `lanes_after`. Empty for the first row.
    pub lanes_before: Vec<Slot<Id>>,
    /// Lane state immediately after this row's node is processed.
    pub lanes_after: Vec<Slot<Id>>,
    /// Indices into `lanes_before` of lanes that targeted this row's node
    /// and therefore terminate at it. Always contains `node_lane` plus any
    /// extra lanes that collapse into it.
    pub merging_lanes: Vec<usize>,
    /// Parent edges classified as `Missing` — they have no lane and the
    /// renderer may draw a stub at the row.
    pub missing_parents: usize,
}

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

/// Renderer-friendly subset of [`LaneRow`]: drops the per-slot target id and
/// keeps only what's needed to draw one row of the gutter. Cheap to clone and
/// safe to attach to per-revision summaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneFrame {
    pub before: Vec<Option<GraphEdgeType>>,
    pub after: Vec<Option<GraphEdgeType>>,
    pub node_lane: usize,
    pub merging_lanes: Vec<usize>,
    pub missing_parents: usize,
}

impl LaneFrame {
    pub fn from_lane_row<Id>(row: &LaneRow<Id>) -> Self {
        Self {
            before: row.lanes_before.iter().map(Slot::kind).collect(),
            after: row.lanes_after.iter().map(Slot::kind).collect(),
            node_lane: row.node_lane,
            merging_lanes: row.merging_lanes.clone(),
            missing_parents: row.missing_parents,
        }
    }

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

pub fn assign_lanes<Id, I>(nodes: I) -> Vec<LaneRow<Id>>
where
    Id: Clone + Eq + Hash,
    I: IntoIterator<Item = (Id, Vec<GraphEdge<Id>>)>,
{
    let mut rows = Vec::new();
    let mut lanes: Vec<Slot<Id>> = Vec::new();

    for (id, edges) in nodes {
        let lanes_before = lanes.clone();

        let merging_lanes: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.awaits(&id).then_some(i))
            .collect();

        let node_lane = if let Some(&first) = merging_lanes.first() {
            for &lane in &merging_lanes[1..] {
                lanes[lane] = Slot::Empty;
            }
            first
        } else {
            allocate_lane(&mut lanes)
        };

        lanes[node_lane] = Slot::Empty;

        let mut continued = false;
        let mut missing_parents = 0;
        for edge in &edges {
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
                        lanes[node_lane] = slot;
                        continued = true;
                    } else {
                        let lane = allocate_lane(&mut lanes);
                        lanes[lane] = slot;
                    }
                }
            }
        }

        while matches!(lanes.last(), Some(Slot::Empty)) {
            lanes.pop();
        }

        rows.push(LaneRow {
            id,
            node_lane,
            lanes_before,
            lanes_after: lanes.clone(),
            merging_lanes,
            missing_parents,
        });
    }

    rows
}

fn allocate_lane<Id>(lanes: &mut Vec<Slot<Id>>) -> usize {
    if let Some(idx) = lanes.iter().position(|slot| matches!(slot, Slot::Empty)) {
        idx
    } else {
        lanes.push(Slot::Empty);
        lanes.len() - 1
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

    fn lanes(rows: &[LaneRow<char>]) -> Vec<(char, usize)> {
        rows.iter().map(|row| (row.id, row.node_lane)).collect()
    }

    /// Render the lane state of every row as a textual sketch:
    /// `<node_lane>: <lanes_after>` where each lane is `.` (empty),
    /// `D`/`I`/`M` for the awaiting edge type, and the row's own column
    /// has the node id in uppercase.
    fn sketch(rows: &[LaneRow<char>]) -> String {
        let mut out = String::new();
        for row in rows {
            let width = row.lanes_after.len().max(row.node_lane + 1);
            let mut line = vec!['.'; width];
            for (i, slot) in row.lanes_after.iter().enumerate() {
                line[i] = match slot {
                    Slot::Empty => '.',
                    Slot::Awaiting {
                        kind: GraphEdgeType::Direct,
                        ..
                    } => '|',
                    Slot::Awaiting {
                        kind: GraphEdgeType::Indirect,
                        ..
                    } => ':',
                    Slot::Awaiting {
                        kind: GraphEdgeType::Missing,
                        ..
                    } => '~',
                };
            }
            // Mark the node position by replacing whatever sits in node_lane.
            line[row.node_lane] = row.id;
            out.push_str(&format!(
                "{}  ({})\n",
                line.iter().collect::<String>(),
                row.node_lane
            ));
        }
        out
    }

    #[test]
    fn linear_history_stays_in_lane_zero() {
        // C -> B -> A
        let rows = assign_lanes([
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
        let rows = assign_lanes([
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
        // After E: lane 0 awaits C, lane 1 awaits D.
        assert_eq!(rows[1].lanes_after.len(), 2);
        assert!(matches!(
            rows[1].lanes_after[0],
            Slot::Awaiting { target: 'C', .. }
        ));
        assert!(matches!(
            rows[1].lanes_after[1],
            Slot::Awaiting { target: 'D', .. }
        ));
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
        let rows = assign_lanes([
            ('M', vec![direct('T'), direct('W')]), // M = mega merge, W = @
            ('T', vec![direct('A')]),
            ('W', vec![direct('A')]),
            ('A', vec![]),
        ]);
        assert_eq!(lanes(&rows), vec![('M', 0), ('T', 0), ('W', 1), ('A', 0)],);
        // After A is processed lane 1 should have collapsed.
        assert!(rows[3].lanes_after.is_empty());
        // A merges two lanes (0 and 1) into 0.
        assert_eq!(rows[3].merging_lanes, vec![0, 1]);
    }

    #[test]
    fn side_branch_takes_a_new_lane_and_releases_it_when_it_ends() {
        // D --> B
        //  \--> C --> A
        //  B's parent A is the same as C's eventual parent.
        let rows = assign_lanes([
            ('D', vec![direct('B'), direct('C')]),
            ('C', vec![direct('A')]),
            ('B', vec![direct('A')]),
            ('A', vec![]),
        ]);
        assert_eq!(lanes(&rows), vec![('D', 0), ('C', 1), ('B', 0), ('A', 0)],);
        assert!(rows[3].lanes_after.is_empty());
    }

    #[test]
    fn missing_parent_does_not_reserve_a_lane() {
        let rows = assign_lanes([('B', vec![missing('X')]), ('A', vec![])]);
        assert_eq!(rows[0].node_lane, 0);
        assert_eq!(rows[0].missing_parents, 1);
        assert!(rows[0].lanes_after.is_empty());
        // A is treated as a fresh head and takes lane 0 again.
        assert_eq!(rows[1].node_lane, 0);
    }

    #[test]
    fn indirect_edge_is_distinguishable_from_direct_in_slot() {
        let rows = assign_lanes([('C', vec![indirect('A')]), ('A', vec![])]);
        assert!(matches!(
            rows[0].lanes_after[0],
            Slot::Awaiting {
                target: 'A',
                kind: GraphEdgeType::Indirect
            },
        ));
    }

    #[test]
    fn sketch_renders_a_simple_dag_for_eyeballing() {
        // Same setup as `mega_merge_pushes_working_copy_to_lane_one`.
        let rows = assign_lanes([
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

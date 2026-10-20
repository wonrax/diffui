//! The graph a draft would leave behind, drawn in place of the loaded one.
//!
//! The repository actor runs the draft in a throwaway transaction and walks
//! the log revset over the result in jj's own order ([`PostOpGraph`]), so the
//! preview lays rows out exactly as the log will after the operation lands.
//! [`PreviewGraph::arrange`] then maps that walk back onto the rows on screen:
//! untouched rows reuse the loaded data, rewritten ones carry their post-op
//! summary, and the commits the operation moves or abandons leave a ghost row
//! where they used to be. The lanes are assigned over that combined sequence,
//! so the edges are the ones the operation will produce.

use std::collections::{BTreeMap, HashMap};

use jj_lib::graph::GraphEdge;

use crate::graph::LaneAssigner;
use crate::graph_layout::{GraphLayout, GraphLayoutBuilder};
use crate::model::{CommitStore, CommitStoreBuilder, CommitSummary, RowView};

/// What the operation does to a commit the post-op log shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostOpRole {
    /// The same commit as before.
    Unchanged,
    /// A new version of a loaded change that stays where it was in the
    /// history: a descendant carried along, a squash destination.
    Rewritten,
    /// A new version of a change the operation moves.
    Moved,
    /// A change that didn't exist before: a merge, a new change, a copy.
    Created,
}

/// One row of the post-op log, in walk order.
#[derive(Debug, Clone)]
pub struct PostOpRow {
    /// Hex id of the pre-op commit this row continues: the commit itself when
    /// unchanged, its predecessor when rewritten or moved. `None` for
    /// created commits.
    pub origin: Option<String>,
    pub role: PostOpRole,
    /// Edges to the rows below, by row index. Missing edges keep a target
    /// outside the row range so the lane assigner can tell them apart.
    pub edges: Vec<GraphEdge<usize>>,
    /// The post-op data, whenever the loaded row can't stand in for it
    /// (anything rewritten or created, a ref or `@` that moved, or a commit
    /// the loaded revset didn't show). Rewritten and moved rows keep their
    /// origin's commit id here, so selections and draft sources still match.
    pub summary: Option<CommitSummary>,
    /// Conflicted after the operation and not before.
    pub new_conflict: bool,
}

/// The log revset walked over a draft's simulated result.
#[derive(Debug, Clone, Default)]
pub struct PostOpGraph {
    pub rows: Vec<PostOpRow>,
    /// Hex ids of pre-op commits the operation abandons outright: a squash
    /// source, an emptied commit, a working copy left behind.
    pub abandoned: Vec<String>,
}

/// How a preview row relates to the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewRole {
    Unchanged,
    Rewritten,
    /// Where a moved commit lands.
    Moved,
    Created,
    /// Where a moved commit used to be.
    MovedFrom,
    /// A commit the operation abandons, where it used to be.
    Abandoned,
}

impl PreviewRole {
    /// A row that only marks where something was.
    pub fn is_ghost(self) -> bool {
        matches!(self, Self::MovedFrom | Self::Abandoned)
    }

    /// A row that stands for a loaded commit staying in place, so pointing
    /// at it can arm the draft's destination.
    pub fn is_target(self) -> bool {
        matches!(self, Self::Unchanged | Self::Rewritten)
    }
}

/// Where a preview row's commit data lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSource {
    /// A row of the loaded store.
    Loaded(usize),
    /// A row of [`PreviewGraph::extra`].
    Extra(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewRow {
    pub role: PreviewRole,
    pub source: RowSource,
    /// The loaded row this row continues, when the loaded graph shows it.
    pub origin: Option<usize>,
    pub new_conflict: bool,
}

/// A [`PostOpGraph`] laid over the loaded rows: what the sidebar draws while
/// a draft is active.
#[derive(Debug, Clone)]
pub struct PreviewGraph {
    pub rows: Vec<PreviewRow>,
    pub layout: GraphLayout,
    /// Post-op data for the rows the loaded store can't supply.
    pub extra: CommitStore,
    /// Loaded row → the display row standing for it.
    by_loaded: HashMap<usize, usize>,
}

/// Where a ghost row goes: under a post-op row, under an earlier ghost, or
/// at the very top.
#[derive(Debug, Clone, Copy)]
enum Anchor {
    Top,
    Row(usize),
    Ghost(usize),
}

impl PreviewGraph {
    /// Lay `post` over the `loaded` rows. `None` when `post` was walked
    /// against a different graph than the one loaded (an unchanged commit it
    /// counts on isn't there), so the caller keeps the plain graph.
    pub fn arrange(post: &PostOpGraph, loaded: &CommitStore) -> Option<Self> {
        let find = |hex: &str| loaded.find_by_commit_id(hex).map(|row| row.index());

        let mut extra = CommitStoreBuilder::with_capacity(
            post.rows.iter().filter(|row| row.summary.is_some()).count(),
        );
        let mut extra_len = 0;
        let mut post_rows: Vec<PreviewRow> = Vec::with_capacity(post.rows.len());
        for row in &post.rows {
            let origin = row.origin.as_deref().and_then(find);
            let source = match &row.summary {
                Some(summary) => {
                    extra.push(summary.clone());
                    extra_len += 1;
                    RowSource::Extra(extra_len - 1)
                }
                None => RowSource::Loaded(origin?),
            };
            post_rows.push(PreviewRow {
                role: match row.role {
                    PostOpRole::Unchanged => PreviewRole::Unchanged,
                    PostOpRole::Rewritten => PreviewRole::Rewritten,
                    PostOpRole::Moved => PreviewRole::Moved,
                    PostOpRole::Created => PreviewRole::Created,
                },
                source,
                origin,
                new_conflict: row.new_conflict,
            });
        }
        let extra = extra.finish();

        // Ghosts, in loaded order: each moved commit's old row, and every
        // abandoned one.
        let mut ghosts: Vec<(usize, PreviewRole)> = post_rows
            .iter()
            .filter(|row| row.role == PreviewRole::Moved)
            .filter_map(|row| row.origin.map(|index| (index, PreviewRole::MovedFrom)))
            .chain(
                post.abandoned
                    .iter()
                    .filter_map(|hex| find(hex))
                    .map(|index| (index, PreviewRole::Abandoned)),
            )
            .collect();
        ghosts.sort_unstable_by_key(|(index, _)| *index);
        ghosts.dedup_by_key(|(index, _)| *index);

        // A ghost sits right under the nearest loaded row above it that is
        // still in place (or is itself a ghost), so it reads as "this used to
        // be here".
        let mut placed: BTreeMap<usize, Anchor> = BTreeMap::new();
        for (index, row) in post_rows.iter().enumerate() {
            if row.role.is_target()
                && let Some(origin) = row.origin
            {
                placed.insert(origin, Anchor::Row(index));
            }
        }
        let mut under_top: Vec<usize> = Vec::new();
        let mut under_row: Vec<Vec<usize>> = vec![Vec::new(); post_rows.len()];
        let mut under_ghost: Vec<Vec<usize>> = vec![Vec::new(); ghosts.len()];
        for (ghost, &(index, _)) in ghosts.iter().enumerate() {
            let anchor = placed
                .range(..index)
                .next_back()
                .map(|(_, anchor)| *anchor)
                .unwrap_or(Anchor::Top);
            match anchor {
                Anchor::Top => under_top.push(ghost),
                Anchor::Row(row) => under_row[row].push(ghost),
                Anchor::Ghost(other) => under_ghost[other].push(ghost),
            }
            placed.insert(index, Anchor::Ghost(ghost));
        }

        #[derive(Clone, Copy)]
        enum Node {
            Row(usize),
            Ghost(usize),
        }
        fn push_ghost(order: &mut Vec<Node>, under_ghost: &[Vec<usize>], ghost: usize) {
            order.push(Node::Ghost(ghost));
            for &next in &under_ghost[ghost] {
                push_ghost(order, under_ghost, next);
            }
        }
        let mut order: Vec<Node> = Vec::with_capacity(post_rows.len() + ghosts.len());
        for &ghost in &under_top {
            push_ghost(&mut order, &under_ghost, ghost);
        }
        for (index, ghosts_under) in under_row.iter().enumerate() {
            order.push(Node::Row(index));
            for &ghost in ghosts_under {
                push_ghost(&mut order, &under_ghost, ghost);
            }
        }

        let bookmarks_of = |source: RowSource| -> &[String] {
            match source {
                RowSource::Loaded(index) => loaded.row(index).bookmarks(),
                RowSource::Extra(index) => extra.row(index).bookmarks(),
            }
        };
        let ghost_base = post_rows.len();
        let mut lanes: LaneAssigner<usize> = LaneAssigner::new();
        let mut layout = GraphLayoutBuilder::new();
        let mut rows: Vec<PreviewRow> = Vec::with_capacity(order.len());
        for node in order {
            match node {
                Node::Row(index) => {
                    let row = post_rows[index];
                    let frame = lanes.push(&index, &post.rows[index].edges);
                    layout.push(&frame, bookmarks_of(row.source));
                    rows.push(row);
                }
                Node::Ghost(ghost) => {
                    let (origin, role) = ghosts[ghost];
                    let frame = lanes.push(&(ghost_base + ghost), &[]);
                    layout.push(&frame, &[]);
                    rows.push(PreviewRow {
                        role,
                        source: RowSource::Loaded(origin),
                        origin: Some(origin),
                        new_conflict: false,
                    });
                }
            }
        }

        // A loaded row is best stood for by its in-place row, then by where
        // it lands, then by its ghost.
        let rank = |role: PreviewRole| match role {
            PreviewRole::Unchanged | PreviewRole::Rewritten => 0,
            PreviewRole::Moved | PreviewRole::Created => 1,
            PreviewRole::MovedFrom | PreviewRole::Abandoned => 2,
        };
        let mut by_loaded: HashMap<usize, usize> = HashMap::new();
        for (display, row) in rows.iter().enumerate() {
            let Some(origin) = row.origin else { continue };
            let better = by_loaded
                .get(&origin)
                .is_none_or(|&current| rank(row.role) < rank(rows[current].role));
            if better {
                by_loaded.insert(origin, display);
            }
        }

        Some(Self {
            rows,
            layout: layout.finish(),
            extra,
            by_loaded,
        })
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The commit data for display row `display`.
    pub fn commit<'a>(&'a self, loaded: &'a CommitStore, display: usize) -> RowView<'a> {
        match self.rows[display].source {
            RowSource::Loaded(index) => loaded.row(index),
            RowSource::Extra(index) => self.extra.row(index),
        }
    }

    /// The loaded row that pointing at display row `display` aims at: only
    /// rows that stay in place can be a destination.
    pub fn target_at(&self, display: usize) -> Option<usize> {
        let row = self.rows.get(display)?;
        row.role.is_target().then_some(row.origin).flatten()
    }

    /// The loaded row display row `display` continues, whatever its role.
    pub fn origin_at(&self, display: usize) -> Option<usize> {
        self.rows.get(display)?.origin
    }

    /// The display row standing for loaded row `loaded`.
    pub fn display_of(&self, loaded: usize) -> Option<usize> {
        self.by_loaded.get(&loaded).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, bookmarks: &[&str]) -> CommitSummary {
        CommitSummary {
            change_id: format!("change-{id}"),
            commit_id: id.to_owned(),
            shortest_change_id_len: Some(1),
            description: id.to_owned(),
            author: "a".to_owned(),
            has_description: true,
            is_empty: None,
            has_conflict: false,
            is_divergent: false,
            is_hidden: false,
            change_offset: None,
            is_working_copy: false,
            is_immutable: false,
            on_trunk: false,
            bookmarks: bookmarks.iter().map(|name| (*name).to_owned()).collect(),
            parent_ids: Vec::new(),
        }
    }

    fn loaded(ids: &[&str]) -> CommitStore {
        let mut builder = CommitStoreBuilder::with_capacity(ids.len());
        for id in ids {
            builder.push(summary(id, &[]));
        }
        builder.finish()
    }

    fn same(id: &str, edges: Vec<GraphEdge<usize>>) -> PostOpRow {
        PostOpRow {
            origin: Some(id.to_owned()),
            role: PostOpRole::Unchanged,
            edges,
            summary: None,
            new_conflict: false,
        }
    }

    fn rewritten(id: &str, role: PostOpRole, edges: Vec<GraphEdge<usize>>) -> PostOpRow {
        PostOpRow {
            origin: Some(id.to_owned()),
            role,
            edges,
            summary: Some(summary(id, &[])),
            new_conflict: false,
        }
    }

    fn roles(graph: &PreviewGraph) -> Vec<(PreviewRole, Option<usize>)> {
        graph
            .rows
            .iter()
            .map(|row| (row.role, row.origin))
            .collect()
    }

    /// Loaded: c, b, a, t (a stack c→b→a on t, plus t). Moving `b` alone
    /// (`-r`) onto `t`: c follows a, b lands on t, and b's ghost stays under
    /// c where it used to be.
    #[test]
    fn a_moved_commit_leaves_a_ghost_under_its_old_neighbour() {
        let loaded = loaded(&["c", "b", "a", "t"]);
        // Post-op walk order: c', a, b', t.
        let post = PostOpGraph {
            rows: vec![
                rewritten("c", PostOpRole::Rewritten, vec![GraphEdge::direct(1)]),
                same("a", vec![GraphEdge::direct(3)]),
                rewritten("b", PostOpRole::Moved, vec![GraphEdge::direct(3)]),
                same("t", vec![]),
            ],
            abandoned: Vec::new(),
        };
        let graph = PreviewGraph::arrange(&post, &loaded).expect("arranges");
        assert_eq!(
            roles(&graph),
            vec![
                (PreviewRole::Rewritten, Some(0)),
                (PreviewRole::MovedFrom, Some(1)),
                (PreviewRole::Unchanged, Some(2)),
                (PreviewRole::Moved, Some(1)),
                (PreviewRole::Unchanged, Some(3)),
            ]
        );
        assert_eq!(graph.layout.len(), 5);
        // Only rows in place can be aimed at.
        assert_eq!(graph.target_at(0), Some(0));
        assert_eq!(graph.target_at(1), None);
        assert_eq!(graph.target_at(3), None);
        assert_eq!(graph.target_at(4), Some(3));
        // `b` is stood for by where it lands, not by its ghost.
        assert_eq!(graph.display_of(1), Some(3));
        assert_eq!(graph.display_of(3), Some(4));
        assert_eq!(graph.origin_at(1), Some(1));
        // Rewritten rows read the post-op data, unchanged ones the store.
        assert_eq!(graph.rows[0].source, RowSource::Extra(0));
        assert_eq!(graph.rows[2].source, RowSource::Loaded(2));
        assert_eq!(graph.commit(&loaded, 3).commit_id(), "b");
    }

    /// A whole stack moving away leaves its ghosts in their old order, the
    /// first one at the top when nothing stayed above it.
    #[test]
    fn consecutive_ghosts_keep_their_order() {
        let loaded = loaded(&["s2", "s1", "x", "t"]);
        let post = PostOpGraph {
            rows: vec![
                same("x", vec![GraphEdge::direct(3)]),
                rewritten("s2", PostOpRole::Moved, vec![GraphEdge::direct(2)]),
                rewritten("s1", PostOpRole::Moved, vec![GraphEdge::direct(3)]),
                same("t", vec![]),
            ],
            abandoned: Vec::new(),
        };
        let graph = PreviewGraph::arrange(&post, &loaded).expect("arranges");
        assert_eq!(
            roles(&graph),
            vec![
                (PreviewRole::MovedFrom, Some(0)),
                (PreviewRole::MovedFrom, Some(1)),
                (PreviewRole::Unchanged, Some(2)),
                (PreviewRole::Moved, Some(0)),
                (PreviewRole::Moved, Some(1)),
                (PreviewRole::Unchanged, Some(3)),
            ]
        );
    }

    /// A squash: the source is abandoned (a ghost), the destination is
    /// rewritten in place, and a moved bookmark comes from the summary.
    #[test]
    fn an_abandoned_commit_is_a_ghost_and_new_commits_have_no_origin() {
        let loaded = loaded(&["src", "dst"]);
        let mut merge = rewritten("m", PostOpRole::Created, vec![GraphEdge::direct(1)]);
        merge.origin = None;
        merge.new_conflict = true;
        let mut destination = rewritten("dst", PostOpRole::Rewritten, vec![]);
        destination.summary = Some(summary("dst", &["main"]));
        let post = PostOpGraph {
            rows: vec![merge, destination],
            abandoned: vec!["src".to_owned(), "gone".to_owned()],
        };
        let graph = PreviewGraph::arrange(&post, &loaded).expect("arranges");
        assert_eq!(
            roles(&graph),
            vec![
                (PreviewRole::Abandoned, Some(0)),
                (PreviewRole::Created, None),
                (PreviewRole::Rewritten, Some(1)),
            ]
        );
        assert!(graph.rows[1].new_conflict);
        assert_eq!(graph.commit(&loaded, 2).bookmarks(), ["main".to_owned()]);
        assert_eq!(graph.target_at(1), None);
    }

    /// Walked against another graph: an unchanged commit the loaded store
    /// doesn't have means the mapping can't be trusted.
    #[test]
    fn a_stale_walk_does_not_arrange() {
        let loaded = loaded(&["a"]);
        let post = PostOpGraph {
            rows: vec![same("b", vec![])],
            abandoned: Vec::new(),
        };
        assert!(PreviewGraph::arrange(&post, &loaded).is_none());
    }

    /// Edges are the post-op ones: after the move, `c` runs straight to `a`
    /// past the ghost, which sits off to the side with no edges of its own.
    #[test]
    fn edges_follow_the_post_op_graph() {
        let loaded = loaded(&["c", "b", "a"]);
        let post = PostOpGraph {
            rows: vec![
                rewritten("b", PostOpRole::Moved, vec![]),
                rewritten("c", PostOpRole::Rewritten, vec![GraphEdge::direct(2)]),
                same("a", vec![]),
            ],
            abandoned: Vec::new(),
        };
        let graph = PreviewGraph::arrange(&post, &loaded).expect("arranges");
        assert_eq!(graph.rows[2].role, PreviewRole::MovedFrom);
        let ghost = graph.layout.frame(2, usize::MAX);
        assert_eq!(ghost.node_lane, 1, "the ghost steps aside for c's edge");
        assert!(ghost.is_pass_through(0));
    }
}

//! Compact storage for the per-row graph lane state.
//!
//! The renderer needs, for every visible row, a [`LaneFrame`] (the edge kinds
//! entering/leaving each lane) plus a [`RowLaneData`] (bookmark labels and
//! hover segment-ids per lane). Storing those per row is `O(rows × width)` — on
//! nixpkgs (~1.1M rows, ~1.2k concurrent lanes) the fold alone reaches ~100GB.
//!
//! But a lane is almost always *constant* down its length: a branch that runs
//! through 100k rows has the same kind/label/segment in every one of them. So
//! instead of a value per row we keep, per column, a run-length list — one
//! entry per *change*. Total storage is `O(edges)` (~tens of MB), independent
//! of how tall the history is. [`GraphLayout::frame`] / [`GraphLayout::fold`]
//! rebuild a single row on demand by binary-searching each column's runs, so
//! only the handful of on-screen rows ever pay reconstruction cost.
//!
//! The runs are filled from the *exact* values the old per-row fold produced
//! (see [`LaneFoldState`]), so reconstruction is lossless — the `reconstruct_*`
//! tests assert byte-identical output against the reference fold.

use jj_lib::graph::GraphEdgeType;

use crate::graph::LaneFrame;

/// Per-commit lane state for the graph gutter: the bookmark labels and hover
/// segment-ids on each lane, captured at the pre-trim (`*`) and post-trim
/// (`continuation_*`) points of the top-down fold. File rows under an expanded
/// commit inherit its `continuation_*` snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowLaneData {
    pub labels: Vec<Vec<String>>,
    pub segments_before: Vec<Option<usize>>,
    pub segments_after: Vec<Option<usize>>,
    pub continuation_labels: Vec<Vec<String>>,
    pub continuation_segments: Vec<Option<usize>>,
}

/// Running state of the top-down lane fold. `advance` consumes one row's
/// [`LaneFrame`] + bookmarks and returns that row's [`RowLaneData`], mutating
/// the carried label/segment state for the next row. This is the single source
/// of truth for the fold; both the batch [`GraphLayoutBuilder`] and the
/// streaming loader (carrying one across `CommitsBatch` messages) drive it
/// through [`GraphLayout::push`], so they can never diverge.
#[derive(Default, Clone, Debug)]
pub struct LaneFoldState {
    current_labels: Vec<Vec<String>>,
    current_segments: Vec<Option<usize>>,
    next_segment_id: usize,
}

impl LaneFoldState {
    pub fn advance(&mut self, lane_frame: &LaneFrame, bookmarks: &[String]) -> RowLaneData {
        let lane_count = lane_frame.lane_count();
        if self.current_labels.len() < lane_count {
            self.current_labels.resize(lane_count, Vec::new());
        }
        if self.current_segments.len() < lane_count {
            self.current_segments.resize(lane_count, None);
        }
        if !bookmarks.is_empty() {
            self.current_labels[lane_frame.node_lane] = bookmarks.to_vec();
        }
        let segments_before = self.current_segments.clone();
        advance_lane_segments(
            &mut self.current_segments,
            &mut self.next_segment_id,
            lane_frame,
        );
        let labels = self.current_labels.clone();
        let segments_after = self.current_segments.clone();
        clear_split_lane_labels(&mut self.current_labels, lane_frame);
        trim_lane_state(
            &mut self.current_segments,
            &mut self.current_labels,
            lane_frame,
        );
        RowLaneData {
            labels,
            segments_before,
            segments_after,
            continuation_labels: self.current_labels.clone(),
            continuation_segments: self.current_segments.clone(),
        }
    }
}

/// Advance per-lane segment ids for one revision row, allocating fresh ids for
/// newly-alive lanes and for lanes whose index is reused by a brand-new
/// outgoing branch at a merge (the "split lane" case — without the reset, hover
/// emphasis would leak across the merged-in branch and the new one sharing a
/// lane index). Labels are handled separately so the merge row itself still
/// shows the merged-in branch's tooltip.
fn advance_lane_segments(
    current_segments: &mut Vec<Option<usize>>,
    next_segment_id: &mut usize,
    lane_frame: &LaneFrame,
) {
    let lane_count = lane_frame.lane_count();
    if current_segments.len() < lane_count {
        current_segments.resize(lane_count, None);
    }
    for (lane, slot) in current_segments.iter_mut().enumerate().take(lane_count) {
        let before = lane_frame.before.get(lane).copied().flatten();
        let after = lane_frame.after.get(lane).copied().flatten();
        let alive = before.is_some() || after.is_some() || lane == lane_frame.node_lane;
        if alive && slot.is_none() {
            *slot = Some(*next_segment_id);
            *next_segment_id += 1;
        }
    }
    for &lane in &lane_frame.merging_lanes {
        if lane == lane_frame.node_lane {
            continue;
        }
        if lane_frame.after.get(lane).copied().flatten().is_none() {
            continue;
        }
        if let Some(slot) = current_segments.get_mut(lane) {
            *slot = Some(*next_segment_id);
            *next_segment_id += 1;
        }
    }
}

/// Clear the merged-in branch's labels on split lanes so its name doesn't carry
/// onto the new branch below the merge. Called after the revision row's label
/// snapshot so the merge row itself still shows the merged-in tooltip.
fn clear_split_lane_labels(current_labels: &mut [Vec<String>], lane_frame: &LaneFrame) {
    for &lane in &lane_frame.merging_lanes {
        if lane == lane_frame.node_lane {
            continue;
        }
        if lane_frame.after.get(lane).copied().flatten().is_none() {
            continue;
        }
        if let Some(labels) = current_labels.get_mut(lane) {
            labels.clear();
        }
    }
}

/// Drop labels and segment ids for lanes that don't survive into the row's
/// `after` snapshot, so a terminated lane doesn't keep a stale tooltip or
/// emphasis-id for the file rows / commits below.
fn trim_lane_state(
    current_segments: &mut [Option<usize>],
    current_labels: &mut [Vec<String>],
    lane_frame: &LaneFrame,
) {
    for (lane, slot) in current_segments.iter_mut().enumerate() {
        let alive = lane_frame.after.get(lane).copied().flatten().is_some();
        if !alive {
            *slot = None;
        }
    }
    for (lane, labels) in current_labels.iter_mut().enumerate() {
        let alive = lane_frame.after.get(lane).copied().flatten().is_some();
        if !alive {
            labels.clear();
        }
    }
}

/// Run-length list for one column's per-row value. `runs` holds `(start_row,
/// value)` sorted by row; a value applies from its `start_row` until the next
/// run begins (or forever after the last). Rows before the first run read as
/// `None` (the column hadn't appeared yet).
#[derive(Debug, Clone, Default)]
struct RunList<T> {
    runs: Vec<(u32, T)>,
}

impl<T: Clone + PartialEq> RunList<T> {
    /// Append `value` for `row`. Rows must arrive in increasing order. A value
    /// equal to the current tail extends the existing run for free.
    fn push(&mut self, row: u32, value: T) {
        if self.runs.last().is_some_and(|(_, last)| *last == value) {
            return;
        }
        self.runs.push((row, value));
    }

    fn get(&self, row: u32) -> Option<&T> {
        let idx = self.runs.partition_point(|(start, _)| *start <= row);
        (idx > 0).then(|| &self.runs[idx - 1].1)
    }
}

/// Compact, reconstruct-on-demand replacement for `Vec<LaneFrame>` +
/// `Vec<RowLaneData>`. Per-column run lists for the varying-down-a-lane fields,
/// plus small per-row arrays for the rest. Memory is `O(edges)`, not
/// `O(rows × width)`.
#[derive(Debug, Clone, Default)]
pub struct GraphLayout {
    row_count: usize,
    node_lane: Vec<u32>,
    missing_parents: Vec<u8>,
    /// Per-row merging-lane indices, flattened. Row `r` occupies
    /// `merging_flat[merging_off[r]..merging_off[r + 1]]`.
    merging_flat: Vec<u32>,
    merging_off: Vec<u32>,
    before_kind: Vec<RunList<Option<GraphEdgeType>>>,
    after_kind: Vec<RunList<Option<GraphEdgeType>>>,
    seg_before: Vec<RunList<Option<u32>>>,
    seg_after: Vec<RunList<Option<u32>>>,
    labels: Vec<RunList<Vec<String>>>,
}

impl GraphLayout {
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.row_count
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Rebuild row `index`'s [`LaneFrame`], drawing at most `width_cap` lanes
    /// (lanes past the cap are clipped — the data itself is uncapped).
    pub fn frame(&self, index: usize, width_cap: usize) -> LaneFrame {
        let row = index as u32;
        let cols = self.before_kind.len().min(width_cap);
        let mut before = Vec::with_capacity(cols);
        let mut after = Vec::with_capacity(cols);
        for col in 0..cols {
            before.push(self.before_kind[col].get(row).copied().flatten());
            after.push(self.after_kind[col].get(row).copied().flatten());
        }
        trim_trailing_none(&mut before);
        trim_trailing_none(&mut after);
        let merging = &self.merging_flat
            [self.merging_off[index] as usize..self.merging_off[index + 1] as usize];
        let merging_lanes = merging
            .iter()
            .map(|&lane| lane as usize)
            .filter(|&lane| lane < width_cap)
            .collect();
        LaneFrame {
            before,
            after,
            node_lane: self.node_lane[index] as usize,
            merging_lanes,
            missing_parents: self.missing_parents[index] as usize,
        }
    }

    /// Rebuild row `index`'s [`RowLaneData`]. The arrays are sized to the row's
    /// own lane width (capped at `width_cap`) — the renderer only reads lanes
    /// `0..frame.lane_count()`, so trailing high-water padding is dropped.
    pub fn fold(&self, index: usize, width_cap: usize) -> RowLaneData {
        let row = index as u32;
        let frame = self.frame(index, width_cap);
        let width = frame.lane_count().min(width_cap);

        let labels: Vec<Vec<String>> = (0..width)
            .map(|col| {
                self.labels
                    .get(col)
                    .and_then(|runs| runs.get(row))
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        let segments_before = (0..width)
            .map(|col| seg_at(&self.seg_before, col, row))
            .collect();
        let segments_after: Vec<Option<usize>> = (0..width)
            .map(|col| seg_at(&self.seg_after, col, row))
            .collect();

        // `continuation_*` is the post-split-clear, post-trim state file rows
        // inherit — derivable from this row's pre-trim snapshots + frame.
        let mut continuation_labels = labels.clone();
        let mut continuation_segments = segments_after.clone();
        for &lane in &frame.merging_lanes {
            if lane == frame.node_lane {
                continue;
            }
            if frame.after.get(lane).copied().flatten().is_none() {
                continue;
            }
            if let Some(slot) = continuation_labels.get_mut(lane) {
                slot.clear();
            }
        }
        for lane in 0..width {
            if frame.after.get(lane).copied().flatten().is_none() {
                if let Some(slot) = continuation_labels.get_mut(lane) {
                    slot.clear();
                }
                if let Some(slot) = continuation_segments.get_mut(lane) {
                    *slot = None;
                }
            }
        }

        RowLaneData {
            labels,
            segments_before,
            segments_after,
            continuation_labels,
            continuation_segments,
        }
    }

    /// Append one row in graph order: drive the carried `fold` for this row's
    /// `frame` + `bookmarks`, then run-length encode the result. Holds no more
    /// than one row plus the fold state, so it composes with the streaming
    /// loader (each `CommitsBatch` appends) and never materializes the dense
    /// per-row arrays. The batch [`GraphLayoutBuilder`] is a thin wrapper over
    /// this so the two paths can't diverge.
    pub fn push(&mut self, frame: &LaneFrame, bookmarks: &[String], fold: &mut LaneFoldState) {
        let row = self.row_count as u32;
        let data = fold.advance(frame, bookmarks);
        self.ensure_columns(frame.lane_count());

        // Push every existing column (including ones that just went dead, so
        // their run closes with a `None`). `RunList::push` no-ops on no change,
        // so a quiet lane costs one comparison.
        let cols = self.before_kind.len();
        for col in 0..cols {
            self.before_kind[col].push(row, frame.before.get(col).copied().flatten());
            self.after_kind[col].push(row, frame.after.get(col).copied().flatten());
            self.seg_before[col].push(
                row,
                data.segments_before.get(col).copied().flatten().map(seg_id),
            );
            self.seg_after[col].push(
                row,
                data.segments_after.get(col).copied().flatten().map(seg_id),
            );
            self.labels[col].push(row, data.labels.get(col).cloned().unwrap_or_default());
        }

        // `merging_off` needs a leading 0 so row 0's slice is
        // `merging_flat[0..merging_off[1]]`; seed it on the first row so the
        // offsets stay valid for reconstruction *during* a streaming load, not
        // just after a final `finish`.
        if self.merging_off.is_empty() {
            self.merging_off.push(0);
        }
        self.node_lane.push(frame.node_lane as u32);
        self.missing_parents
            .push(frame.missing_parents.min(u8::MAX as usize) as u8);
        for &lane in &frame.merging_lanes {
            self.merging_flat.push(lane as u32);
        }
        self.merging_off.push(self.merging_flat.len() as u32);
        self.row_count += 1;
    }

    fn ensure_columns(&mut self, count: usize) {
        while self.before_kind.len() < count {
            self.before_kind.push(RunList::default());
            self.after_kind.push(RunList::default());
            self.seg_before.push(RunList::default());
            self.seg_after.push(RunList::default());
            self.labels.push(RunList::default());
        }
    }
}

fn seg_at(columns: &[RunList<Option<u32>>], col: usize, row: u32) -> Option<usize> {
    columns
        .get(col)
        .and_then(|runs| runs.get(row))
        .copied()
        .flatten()
        .map(|id| id as usize)
}

fn trim_trailing_none<T>(values: &mut Vec<Option<T>>) {
    while matches!(values.last(), Some(None)) {
        values.pop();
    }
}

/// Builds a [`GraphLayout`] incrementally: feed it one row's [`LaneFrame`] +
/// bookmarks at a time (in graph order). It drives the fold and run-length
/// encodes the result, never holding more than one row plus the carried fold
/// state, so it composes with a streaming loader and never materializes the
/// dense per-row arrays.
#[derive(Default)]
pub struct GraphLayoutBuilder {
    layout: GraphLayout,
    fold: LaneFoldState,
}

impl GraphLayoutBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, frame: &LaneFrame, bookmarks: &[String]) {
        self.layout.push(frame, bookmarks, &mut self.fold);
    }

    pub fn finish(self) -> GraphLayout {
        self.layout
    }
}

fn seg_id(id: usize) -> u32 {
    id as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::assign_lanes;
    use jj_lib::graph::GraphEdge;

    fn direct(c: char) -> GraphEdge<char> {
        GraphEdge::direct(c)
    }
    fn indirect(c: char) -> GraphEdge<char> {
        GraphEdge::indirect(c)
    }
    fn missing(c: char) -> GraphEdge<char> {
        GraphEdge::missing(c)
    }

    /// Reference fold over a frame list (the `CommitStore`-free path), so the
    /// reconstruction can be checked without building a store.
    fn fold_frames(frames: &[LaneFrame], bookmarks: &[Vec<String>]) -> Vec<RowLaneData> {
        let mut state = LaneFoldState::default();
        frames
            .iter()
            .zip(bookmarks)
            .map(|(frame, bm)| state.advance(frame, bm))
            .collect()
    }

    fn build(frames: &[LaneFrame], bookmarks: &[Vec<String>]) -> GraphLayout {
        let mut builder = GraphLayoutBuilder::new();
        for (frame, bm) in frames.iter().zip(bookmarks) {
            builder.push(frame, bm);
        }
        builder.finish()
    }

    /// Compare reconstruction against the reference fold for every row, trimming
    /// the reference's `RowLaneData` arrays to each row's lane width (the
    /// renderer never reads past it, and the dense fold leaves high-water
    /// padding there).
    fn assert_reconstructs(frames: &[LaneFrame], bookmarks: &[Vec<String>]) {
        let reference = fold_frames(frames, bookmarks);
        let layout = build(frames, bookmarks);
        assert_eq!(layout.len(), frames.len());
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(&layout.frame(i, usize::MAX), frame, "frame {i}");
            let width = frame.lane_count();
            let got = layout.fold(i, usize::MAX);
            let want = &reference[i];
            assert_eq!(
                truncate(&got.labels, width),
                truncate(&want.labels, width),
                "labels {i}"
            );
            assert_eq!(
                truncate(&got.segments_before, width),
                truncate(&want.segments_before, width),
                "segments_before {i}"
            );
            assert_eq!(
                truncate(&got.segments_after, width),
                truncate(&want.segments_after, width),
                "segments_after {i}"
            );
            assert_eq!(
                truncate(&got.continuation_labels, width),
                truncate(&want.continuation_labels, width),
                "continuation_labels {i}"
            );
            assert_eq!(
                truncate(&got.continuation_segments, width),
                truncate(&want.continuation_segments, width),
                "continuation_segments {i}"
            );
        }
    }

    fn truncate<T: Clone>(values: &[T], width: usize) -> Vec<T> {
        values.iter().take(width).cloned().collect()
    }

    fn no_bookmarks(n: usize) -> Vec<Vec<String>> {
        vec![Vec::new(); n]
    }

    #[test]
    fn reconstructs_linear_history() {
        let frames = assign_lanes([
            ('C', vec![direct('B')]),
            ('B', vec![direct('A')]),
            ('A', vec![]),
        ]);
        assert_reconstructs(&frames, &no_bookmarks(frames.len()));
    }

    #[test]
    fn reconstructs_merge_and_split_lanes() {
        let frames = assign_lanes([
            ('F', vec![direct('E')]),
            ('E', vec![direct('C'), direct('D')]),
            ('D', vec![direct('B')]),
            ('B', vec![direct('A')]),
            ('C', vec![direct('A')]),
            ('A', vec![]),
        ]);
        assert_reconstructs(&frames, &no_bookmarks(frames.len()));
    }

    #[test]
    fn reconstructs_side_branch_lane_reuse() {
        let frames = assign_lanes([
            ('D', vec![direct('B'), direct('C')]),
            ('C', vec![direct('A')]),
            ('B', vec![direct('A')]),
            ('A', vec![]),
        ]);
        assert_reconstructs(&frames, &no_bookmarks(frames.len()));
    }

    #[test]
    fn reconstructs_missing_and_indirect_edges() {
        let frames = assign_lanes([
            ('D', vec![missing('X'), direct('C')]),
            ('C', vec![indirect('A')]),
            ('B', vec![missing('Y')]),
            ('A', vec![]),
        ]);
        assert_reconstructs(&frames, &no_bookmarks(frames.len()));
    }

    #[test]
    fn reconstructs_with_bookmarks_on_lanes() {
        let frames = assign_lanes([
            ('M', vec![direct('T'), direct('W')]),
            ('T', vec![direct('A')]),
            ('W', vec![direct('A')]),
            ('A', vec![]),
        ]);
        // Bookmarks latch onto a lane and propagate down until it ends; put one
        // on the merge (reused lane) and one mid-branch to exercise the label
        // run-length encoding and the split-lane clearing.
        let bookmarks = vec![
            vec!["main".to_owned()],
            vec![],
            vec!["feature".to_owned()],
            vec![],
        ];
        assert_reconstructs(&frames, &bookmarks);
    }

    #[test]
    fn reconstructs_second_bookmark_overwrites_same_lane() {
        // Two bookmarks land on lane 0 at different rows — the label must change
        // mid-segment, which the run-length list has to capture.
        let frames = assign_lanes([
            ('C', vec![direct('B')]),
            ('B', vec![direct('A')]),
            ('A', vec![]),
        ]);
        let bookmarks = vec![
            vec!["tip".to_owned()],
            vec!["middle".to_owned()],
            vec!["root".to_owned()],
        ];
        assert_reconstructs(&frames, &bookmarks);
    }

    #[test]
    fn width_cap_clips_lanes() {
        let frames = assign_lanes([
            ('D', vec![direct('B'), direct('C')]),
            ('C', vec![direct('A')]),
            ('B', vec![direct('A')]),
            ('A', vec![]),
        ]);
        let layout = build(&frames, &no_bookmarks(frames.len()));
        // Row 0 (D) opens a second lane; capping at 1 lane drops it.
        let capped = layout.frame(0, 1);
        assert!(capped.before.len() <= 1);
        assert!(capped.after.len() <= 1);
        assert!(capped.merging_lanes.iter().all(|&l| l < 1));
    }

    #[test]
    fn partial_layout_reconstructs_rows_pushed_so_far() {
        // Streaming paints rows before the walk finishes, so `frame`/`fold`
        // must reconstruct a row the moment it's pushed — not only after a
        // final `finish`. This is what seeding the `merging_off` leading-0 on
        // the first push (vs the old finish-time insert) buys. Drive the
        // merge/split topology with bookmarks so the label/segment run-lists
        // are exercised mid-stream too.
        let frames = assign_lanes([
            ('M', vec![direct('T'), direct('W')]),
            ('T', vec![direct('A')]),
            ('W', vec![direct('A')]),
            ('A', vec![]),
        ]);
        let bookmarks = vec![
            vec!["main".to_owned()],
            vec![],
            vec!["feature".to_owned()],
            vec![],
        ];
        let full = build(&frames, &bookmarks);

        let mut layout = GraphLayout::default();
        let mut fold = LaneFoldState::default();
        for (k, (frame, bm)) in frames.iter().zip(&bookmarks).enumerate() {
            layout.push(frame, bm, &mut fold);
            // Every row pushed so far must already match the finished build.
            for i in 0..=k {
                assert_eq!(
                    layout.frame(i, usize::MAX),
                    full.frame(i, usize::MAX),
                    "frame {i} after pushing through row {k}"
                );
                assert_eq!(
                    layout.fold(i, usize::MAX),
                    full.fold(i, usize::MAX),
                    "fold {i} after pushing through row {k}"
                );
            }
        }
        assert_eq!(layout.len(), frames.len());
    }

    #[test]
    fn lane_reuse_at_merge_starts_a_new_segment() {
        // Two children of a merge commit M share the lane, then M brings in a
        // feature on a reused index. The merged-in branch (above M) and the
        // brand-new feature branch (below M) must end up with different segment
        // ids — otherwise hovering one would emphasize both.
        let mut current_segments: Vec<Option<usize>> = Vec::new();
        let mut current_labels: Vec<Vec<String>> = Vec::new();
        let mut next_segment_id: usize = 0;

        let direct = Some(GraphEdgeType::Direct);

        let frame_a0 = LaneFrame {
            before: vec![],
            after: vec![direct],
            node_lane: 0,
            merging_lanes: vec![],
            missing_parents: 0,
        };
        advance_lane_segments(&mut current_segments, &mut next_segment_id, &frame_a0);
        let a0_seg = current_segments[0];
        trim_lane_state(&mut current_segments, &mut current_labels, &frame_a0);

        let frame_b0 = LaneFrame {
            before: vec![direct, None],
            after: vec![direct, direct],
            node_lane: 1,
            merging_lanes: vec![],
            missing_parents: 0,
        };
        if current_labels.len() < frame_b0.lane_count() {
            current_labels.resize(frame_b0.lane_count(), Vec::new());
        }
        advance_lane_segments(&mut current_segments, &mut next_segment_id, &frame_b0);
        let b0_seg = current_segments[1];
        trim_lane_state(&mut current_segments, &mut current_labels, &frame_b0);

        let frame_m = LaneFrame {
            before: vec![direct, direct],
            after: vec![direct, direct],
            node_lane: 0,
            merging_lanes: vec![0, 1],
            missing_parents: 0,
        };
        let m_before = current_segments.clone();
        advance_lane_segments(&mut current_segments, &mut next_segment_id, &frame_m);
        let m_after = current_segments.clone();
        trim_lane_state(&mut current_segments, &mut current_labels, &frame_m);

        // The before-half on the reused lane carries the merged-in branch
        // (B0)'s segment; the after-half is a fresh id — so hovering either
        // branch only emphasizes its own half at M.
        assert_eq!(m_before[1], b0_seg);
        assert_ne!(m_after[1], b0_seg);
        // Lane 0 continues across the merge — same id from A0 onward.
        assert_eq!(m_before[0], a0_seg);
        assert_eq!(m_after[0], a0_seg);
    }

    #[test]
    fn pass_through_lane_keeps_its_segment_id() {
        // An unrelated lane that just passes through a commit keeps its id.
        let mut current_segments: Vec<Option<usize>> = Vec::new();
        let mut next_segment_id: usize = 0;

        let direct = Some(GraphEdgeType::Direct);

        let frame_a = LaneFrame {
            before: vec![direct, direct],
            after: vec![direct, direct],
            node_lane: 0,
            merging_lanes: vec![],
            missing_parents: 0,
        };
        current_segments.extend([Some(10), Some(11)]);
        let lane_1_before = current_segments[1];
        advance_lane_segments(&mut current_segments, &mut next_segment_id, &frame_a);
        assert_eq!(current_segments[1], lane_1_before);
    }
}

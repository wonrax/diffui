# Handoff — large-repo performance & memory re-architecture

Status as of 2026-05-29. This branch makes diffui usable on huge repos
(benchmark: **nixpkgs ~1.1M commits**; the everyday bench is **bun ~43k commits**,
a jj clone at `~/code/bun`). It contains several finished fixes plus the
groundwork for a load re-architecture that's only partly done — the remaining
phases are spelled out at the bottom.

> Working agreements: this repo is **jj** (colocated git). Use `jj`, not raw
> `git`, for VCS ops. Project builds must be prefixed with `nix develop -c`
> (e.g. `nix develop -c cargo clippy`); `jj`/`rg` run directly. Avoid
> `unwrap()`/panics — propagate with `?` + `wrap_err`/`context`. Keep clippy
> clean.

---

## ⚠️ KNOWN REGRESSION — still OOMs on nixpkgs (TOP PRIORITY, fix before shipping)

Tested 2026-05-29 by the owner after everything below landed: nixpkgs still
**OOMs at the loading screen, after a while, once all commits have loaded.** So
the crash moved from the *transient load peak* (fixed) to **live memory in
post-load processing** — different bug, same symptom.

**Diagnosis: removing `MAX_LANES` uncapped the LIVE per-row lane storage.** The
cap was load-bearing for *live* memory, not just the transient intermediate I
removed — the original note even said it "bounds assign_lanes AND the fold."
nixpkgs' `all()` has ~1000 genuinely-concurrent lanes, so with no cap:
- `CommitStore.lanes: Vec<LaneFrame>` — each row's `before`/`after` is now
  ~1000 entries wide instead of ≤32, and
- the sidebar fold (`sidebar::compute_lane_fold` → `RowLaneData`, built over
  **all** commits in `Diffui::rebuild_sidebar_index` right after the load swap)
  balloons the same way.

≈ 1.1M rows × ~1000 lanes × several bytes ≈ **many GB live → OOM**. The "after a
while, after load" timing points straight at `rebuild_sidebar_index`
(`compute_lane_fold`), which runs once the store is swapped in.

This is the tension behind the "grow gutter to fit" decision (below): **dense
per-row storage of a 1000-wide graph is inherently multi-GB.** To keep uncapped
topology we must stop storing dense full-width rows. Options when resuming:
1. **Bound the STORED width, generously** (e.g. 64–128 vs the old 32) and
   clip/merge beyond. Smallest change; shifts "grow to fit" → "bound + clip
   overflow." Bounds memory, keeps far more topology than before.
2. **Sparse per-row storage** (store only occupied lane indices, not a dense
   vector up to the max width) **+ lazy fold** (compute `RowLaneData` only for
   on-screen rows, not all 1.1M in `rebuild_sidebar_index`). Keeps full
   topology; bigger change. Pairs with the arena-packing (phase C).

**Until this is resolved, the uncapped lanes are not shippable** — re-add a
bound or go sparse+lazy. The mimalloc + transient-peak + f64 wins still stand.

---

## What's landed on this branch

All of the following compiles, is clippy-clean, and passes the 37 tests
(`nix develop -c cargo test`). None of it is GUI-verified — verify visually.

### 1. f64 scroll/row precision (`src/revision_list.rs`)
The sidebar's row geometry and scroll offset were `f32`. A ~1M-row list is
~50M px tall, past f32's exact-integer ceiling (2^24 ≈ 16.7M), so the draw
path (`flat * ROW_H`) and the hit-test path (`y / ROW_H`) rounded to different
rows — a click near the bottom of nixpkgs selected the neighbouring commit.

Fix: `State::vertical_offset` + the pure geometry fns
(`rows_content_height` / `row_top_at` / `row_at_offset_in`, and the
`content_height` / `row_top` / `row_at_offset` methods) are now `f64` (exact to
2^53, far past any repo). Row-height **constants stay `f32`** (whole px) and are
cast inside the geometry fns. We narrow back to `f32` **only** at the
render/scrollbar boundary, e.g. `screen_y = bounds.y + (row_top_local -
visible_top) as f32` — the subtraction cancels the large magnitude first, so the
leftover is viewport-small and f32-exact. The `scrollbar` module stays f32
(purely visual thumb; offsets it returns are widened to f64 before storage).
Regression test: `revision_list::tests::row_geometry_exact_at_large_indices`.

Why not integers? f64 already gives integer-exactness where it matters **and**
keeps sub-pixel trackpad scroll (deltas are fractional, `delta * 0.65`); pure
integer pixels would need a separate fractional-scroll accumulator.

### 2. Uncapped, streaming-ready lane assignment (`src/graph.rs`)
The old `assign_lanes` returned `Vec<LaneRow<CommitId>>` — every lane slot
cloned a 32-byte `CommitId`, and it kept one such row per commit. On nixpkgs'
~1000-wide graph that intermediate was ~75GB, which is why a `MAX_LANES = 32`
cap existed (it bounded the intermediate, but corrupted topology by merging
overflow branches into the last lane).

Replaced with a stateful **`LaneAssigner<Id>`**: `push(&id, &edges) -> LaneFrame`
keeps the running lane state across calls (so it composes with a streaming
loader) and emits the compact `LaneFrame` (1 byte/lane, no commit ids)
directly. `LaneRow` and `LaneFrame::from_lane_row` are gone. **`MAX_LANES` is
removed** — width is unbounded; the renderer grows the gutter to fit (decision
below). A batch `assign_lanes(nodes) -> Vec<LaneFrame>` wrapper remains for the
git loader; the **jj loader drives the assigner inline in its load loop**
(`src/jj.rs`), which also dropped the old `nodes.iter().map(clone)` feed and the
`Vec<LaneFrame>` intermediate.

Effect: bun's load peak dropped **126 MB → 42 MB**; nixpkgs' win is far larger
(its intermediate was multi-GB).

### 3. mimalloc global allocator (`src/main.rs`, `Cargo.toml`)
macOS's system malloc parks a load's transient high-water mark and keeps RSS
pinned near the peak long after the working set shrinks. mimalloc returns freed
memory to the OS far more eagerly, so RSS tracks the live set. Wired as
`#[global_allocator]` when **not** profiling (see below).

### 4. Memory profiler (`track-alloc` feature)
A counting `#[global_allocator]` (`src/main.rs` `track_alloc` module:
`CURRENT`/`PEAK` atomics) + `CommitStore::heap_bytes()` (`src/backend.rs`) + an
`#[ignore]`d test `jj::mem_profile::profile_load_memory`. Run it:

```sh
DIFFUI_PROFILE_REPO=/path/to/repo \
  nix develop -c cargo test --features track-alloc profile_load_memory -- --ignored --nocapture
# defaults to ~/code/bun if the env var is unset
```

It prints transient peak vs live (logical bytes — true RSS runs higher due to
allocator rounding/fragmentation; the peak/live **ratio** is the signal). Bun
measured **7.28× peak/live before the lane rewrite, 2.09× after**. The
`track-alloc` counting allocator overrides mimalloc, so mimalloc's effect is
*not* visible here — it's an RSS-level (OS reclaim) win; check Activity Monitor.

---

## The core diagnosis (why RSS was 6.24 GB vs Fork's 2.69 GB)

Most of the footprint was **transient load peak the allocator never returned**,
not live data. The live `CommitStore` is ~400–470 B/commit (bun: ~20 MB for
43k). The load spiked to several× that — dominated by the `LaneRow<CommitId>`
intermediate (fix #2) plus the `nodes` / `tree_ids` / `single_parents` maps all
coexisting — then freed it, but the system allocator held the peak. Fixes #2
(kill the intermediate) and #3 (return memory to OS) attack exactly this;
streaming (below) finishes it by stopping the transients from coexisting.

---

## Locked design decisions

- **Load model: stream all in the background.** Render the newest ~few-thousand
  commits in <1s, stream the rest while the UI stays interactive. Everything
  ends up in memory (~ a couple GB for nixpkgs after these fixes). *Not*
  windowing/eviction — search stays simple and covers all history once the
  stream catches up.
- **Wide graphs: grow the gutter to fit.** No lane cap. **⚠️ This is what's
  OOMing — see the regression section. Dense uncapped per-row storage of a
  ~1000-wide graph doesn't fit in memory, so this decision likely has to move
  toward "bound stored width + clip overflow" or sparse+lazy storage.** nixpkgs
  will also visually have a very wide gutter in those regions, pushing commit
  text rightward.
- **Commit search: a mode inside the command palette.** See phase below.

---

## Remaining phases (not started)

Ordered by value. Each is independent enough to land on its own.

### A. Streaming load  *(biggest felt win + finishes the memory story)*
Today the loader is all-or-nothing (`src/jj.rs::load_jj_commits`,
`src/backend.rs::run_backend`): it walks the **entire** revset into a `Vec` of
nodes (the "~20s before the progress bar" — `TopoGroupedGraphIterator` must
traverse the whole graph to topo-sort), then a sequential 1.1M-iteration
`get_commit_async` loop, then one atomic swap into the UI.

Goal: pull from the graph iterator in **batches** (don't `.collect()` it all),
drive the `LaneAssigner` per batch (state already persists — that's why it's a
struct), and ship each batch to the UI which **appends** to the store. First
paint <1s; the rest fills in.

Structural work required:
- Make `CommitStore` **appendable** (today it's built by a consuming
  `CommitStoreBuilder` and is immutable after `finish()`). Either give
  `CommitStore` `push`/`extend` directly with a persistent author-interner, or
  keep the builder alive and emit snapshots. The author interner + text arena
  must persist across batches.
- Make the **sidebar index incremental** (`src/main.rs::rebuild_sidebar_index`
  recomputes the lane fold + `sidebar_prefix_lens` over *all* commits today —
  O(n) per batch = O(n²)). The lane fold is a forward pass (incrementalizable);
  the jj loader already stores per-commit shortest-prefix in the store
  (`shortest_change_id_len`).
- Deliver batches via an iced subscription/channel (see `watch_repository` in
  `src/main.rs` for the `iced::stream::channel` pattern) and a new
  `Message::CommitsBatch(version, …)` that appends. The ordering guarantee from
  the revset is **descendants-first topological** (newest first), which is
  exactly the top-down order streaming needs.
- Buffered concurrency for `get_commit_async` would further cut load time.

### B. Commit-search split  *(fixes palette 5fps + the Enter UX)*
Today `palette::recompute_matches` builds a fuzzy haystack for **all 1.1M
commits on every keystroke** (after a 120ms debounce) — that's the lag.

- Drop commits from the keystroke path (`Mode::Mixed` →
  commands/bookmarks/files only; those sets are tiny). See
  `palette::push_revision_candidates`.
- Add a **commit-search mode** in the same palette widget that only runs on
  **Enter** (a prefix or an explicit submit), fuzzy-matching the commit set and
  showing results in a column. Reuse the existing recents + scoring + the
  reveal path.
- Reveal: `jump_to_revision_ref` → `revision_selection` → `find_by_change_id` →
  `load_diff` → `DiffLoaded` → `find_selected_commit_index` (linear scan of the
  store) → bump `revision_reveal_token` → sidebar scrolls. **Constraint:** the
  target commit must be in the store for reveal to work — fine under stream-all,
  but if a match is in the not-yet-streamed tail, wait for the stream (or drive
  it to completion).

### C. Arena-pack lanes  *(more live-memory savings — may be unnecessary)*
`CommitStore.lanes: Vec<LaneFrame>` does 2–3 small heap allocs per row
(`before`/`after`/`merging_lanes`). Pack them into the struct-of-arrays store
like the text arena: one flat `Vec` for all edge bytes + per-commit offset
spans. Cuts live lane memory and per-row malloc churn/fragmentation.
**Gate on the nixpkgs RSS reading** after A+#2+#3 — if RSS is already good, skip.

---

## Verify on nixpkgs (owner's machine — GUI not runnable here)
1. **Memory:** reload nixpkgs, watch Activity Monitor. mimalloc + the lane
   rewrite should drop RSS well below 6.24 GB. That number decides whether
   phase C is worth doing.
2. **Wide graphs:** scroll to regions with many parallel branches — confirm the
   "grow gutter to fit" behavior is acceptable, or we switch to clip-overflow.
3. **Click accuracy:** scroll to the very bottom, click commits — should select
   exactly what's under the cursor (the f64 fix).

---

## Continuing at home (jj)
```sh
jj git fetch
jj new <this-branch>        # or `jj edit <this-branch>` to keep editing it
nix develop -c cargo test   # sanity
```
Full background lives in the assistant's project memory, but this file is the
source of truth for the branch.

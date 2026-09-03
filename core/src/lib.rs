//! `diffui-core`: the headless, UI-agnostic core of diffui.
//!
//! This crate owns the jj/git diffing, the commit graph, and — as the
//! extraction proceeds — the diff data model and a `Session` orchestration
//! engine. It has **no `iced` dependency**, so alternative frontends
//! (electron, swiftui, web, …) can build on it.

pub mod diff_parse;
pub mod git;
pub mod github;
pub mod graph;
pub mod graph_layout;
pub mod jj;
pub mod model;
pub mod mutations;
pub mod repo;
pub mod repository;
pub mod session;
pub mod source_browse;
pub mod syntax;
#[cfg(feature = "watcher")]
pub mod watcher;

// Curated flat surface a frontend builds against.
pub use diff_parse::{DiffStreamParser, format_hunk_header, parse_unified_diff};
pub use model::*;
pub use mutations::{
    Destination, DraftKind, DraftSimulation, DraftSource, MergePreview, MutationOp,
    MutationOutcome, OpDraft, PlacementKind, RebasePreview, RebaseSourceMode, SquashTarget,
};
pub use repo::{
    Capabilities, Command, Event, JobId, OpenSpec, Payload, PreviewRequest, RepoError, RepoHandle,
    RepoId, canonical_selection, rewritten_targets,
};
pub use repository::{FetchTarget, Repository, RepositorySnapshot, Vcs, prepare_repository};
pub use session::{
    ColdBatchFold, ColdCursor, Effect, LoadStatus, RefreshOrigin, Session, coalesce_refresh,
    fold_cold_batch,
};
pub use source_browse::{
    SourceEntry, SourceEntryStatus, SourceFileLoad, SourceTreeRow, build_source_file,
    list_ignored_dir, sort_source_entries, source_tree_rows,
};

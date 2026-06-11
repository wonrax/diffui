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
pub mod repository;
pub mod session;
pub mod source;
pub mod syntax;
#[cfg(feature = "watcher")]
pub mod watcher;

// Curated flat surface a frontend builds against.
pub use diff_parse::{DiffStreamParser, format_hunk_header, parse_unified_diff};
pub use model::*;
pub use mutations::{MutationOp, MutationOutcome};
pub use repository::{FetchTarget, Repository, RepositorySnapshot, Vcs, prepare_repository};
pub use session::{
    ColdBatchFold, ColdCursor, LoadStatus, LoadVersion, MutationQueue, QueueAction, RefreshOrigin,
    Session, coalesce_refresh, fold_cold_batch,
};
pub use source::{DiffSource, DiffTarget, Mutable, RepoSource, RevisionGraph, SourceHandle};
pub use source::{
    compute_empty_status, fetch, highlight_file, load_backend, load_diff, load_repository_snapshot,
    load_revision_details, read_op_head, undo,
};

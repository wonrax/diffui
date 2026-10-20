//! `diffui-core`: the headless, UI-agnostic core of diffui.
//!
//! This crate owns the jj/git diffing, the commit graph, and — as the
//! extraction proceeds — the diff data model and a `Session` orchestration
//! engine. It has **no `iced` dependency**, so alternative frontends
//! (electron, swiftui, web, …) can build on it.

pub mod askpass;
pub mod cli;
pub mod completion;
pub mod diff_parse;
pub mod git;
pub mod github;
pub mod graph;
pub mod graph_layout;
pub mod history;
pub mod jj;
pub mod model;
pub mod mutations;
pub mod plan;
pub mod partial;
pub mod preview_graph;
pub mod push;
pub mod repo;
pub mod repository;
pub mod resolve;
pub mod revset;
pub mod session;
pub mod source_browse;
pub mod syntax;
#[cfg(feature = "watcher")]
pub mod watcher;

// Curated flat surface a frontend builds against.
pub use cli::{CliCommand, CliInvocation, JjCli, ParsedCommand};
pub use diff_parse::{DiffStreamParser, format_hunk_header, parse_unified_diff};
pub use model::*;
pub use mutations::{
    Destination, DraftKind, DraftSimulation, DraftSource, InsertPreview, MergePreview, MutationOp,
    MutationOutcome, OpDraft, PlacementKind, RebasePreview, RebaseSourceMode, SquashPreview,
    SquashTarget,
};
pub use partial::{DiffSelection, FileSelection, HunkPick, HunkRef, LineRef};
pub use preview_graph::{PostOpGraph, PreviewGraph, PreviewRole, PreviewRow, RowSource};
pub use push::{PushAllow, PushCheck, PushRejection, PushRequest, PushTarget, RejectedCommit};
pub use repo::{
    Capabilities, Command, Event, JobId, OpenSpec, Payload, PreviewRequest, RepoError, RepoHandle,
    RepoId, canonical_selection, rewritten_targets,
};
pub use repository::{FetchTarget, Repository, RepositorySnapshot, Vcs, prepare_repository};
pub use resolve::{ConflictFile, ConflictResolution, ConflictTerm, MergeToolInvocation};
pub use revset::{
    RevsetCompleter, RevsetError, RevsetExpect, RevsetMatch, RevsetQuery, RevsetSymbols,
};
pub use session::{
    ColdBatchFold, ColdCursor, Effect, LoadStatus, RefreshOrigin, Session, coalesce_refresh,
    fold_cold_batch,
};
pub use source_browse::{
    SourceEntry, SourceEntryStatus, SourceFileLoad, SourceTreeRow, build_source_file,
    list_ignored_dir, sort_source_entries, source_tree_rows,
};

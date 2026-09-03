//! The jj backend, split by concern: config layering ([`settings`]), the
//! working-copy lock prologue ([`workspace`]), the revset walk ([`walk`]),
//! diffs ([`diff`]), source browsing ([`browse`]), history mutation
//! ([`mutate`]), draft simulation ([`preview`]) and git remotes ([`remote`]).
//!
//! jj-lib's stores and workspaces are `!Send`, so everything below runs on the
//! repository actor's own thread ([`crate::repo`]) under a current-thread
//! runtime — that thread is the only place a `Workspace` is ever held.

pub(crate) mod browse;
pub(crate) mod diff;
pub(crate) mod mutate;
pub(crate) mod preview;
pub(crate) mod remote;
pub(crate) mod settings;
pub(crate) mod walk;
pub(crate) mod workspace;

pub use walk::jj_log_revset;
/// Driving the walk from outside the actor, against a workspace it opens
/// itself, is only ever the `track-alloc` memory profile: everything in the app
/// goes through [`walk_jj_with_repo`] on the repo the actor already holds.
#[cfg(feature = "track-alloc")]
pub use walk::walk_jj_commits;
pub use workspace::read_jj_op_head;

pub(crate) use browse::{list_jj_source_tree, read_jj_file_pair_inner, read_jj_source_file};
pub(crate) use diff::{diff_jj_with_repo, jj_revision_details};
pub(crate) use mutate::{ImmutableRewriteError, apply_mutation, check_bookmark_move_backwards};
pub(crate) use preview::{preview_merge, preview_rebase};
pub(crate) use remote::fetch_jj;
pub(crate) use walk::{WorkspaceView, compute_jj_empty_status, walk_jj_with_repo};
pub(crate) use workspace::{
    SnapshotContext, load_workspace, read_op_head_with, resolve_revision, snapshot_working_copy,
};

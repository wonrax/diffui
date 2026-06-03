//! Revision-context-menu mutations — `new`, `edit`, `abandon`.
//!
//! Only the op types and the off-runtime dispatch live here. The jj-lib
//! execution is [`crate::jj::apply_mutation`], which reuses the working-copy
//! snapshot/checkout machinery in `jj.rs` so a mutation that moves `@` also
//! updates the files on disk (and doesn't lose uncommitted work).

use crate::backend::RevisionSelection;
use crate::repository::Repository;

/// A revision-context-menu mutation. The target is a `RevisionSelection` so the
/// working-copy case (which has no stable change-id) stays representable.
#[derive(Debug, Clone)]
pub enum MutationOp {
    /// Create a new child commit of `parent` and move `@` onto it (`jj new`).
    New { parent: RevisionSelection },
    /// Move the working copy to `target` (`jj edit`).
    Edit { target: RevisionSelection },
    /// Discard `target`, re-parenting its descendants onto its parents
    /// (`jj abandon`).
    Abandon { target: RevisionSelection },
}

/// User-facing summary of a successful mutation — becomes the status message.
#[derive(Debug, Clone)]
pub struct MutationOutcome {
    pub message: String,
}

/// Run `op` off the iced runtime. jj-lib holds `!Send` state, so the work runs
/// on `spawn_blocking + block_on`, mirroring the read path in `jj.rs`.
pub async fn run_mutation(
    repository: Repository,
    op: MutationOp,
) -> Result<MutationOutcome, String> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(crate::jj::apply_mutation(repository, op)))
        .await
        .map_err(|e| format!("mutation task panicked: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

//! Per-job cancellation.
//!
//! A long job (a million-row walk, a pull-request download, an empty-status
//! sweep) polls its flag at a natural boundary — a batch, a file, a chunk — and
//! stops. It then reports [`Payload::Cancelled`](super::Payload::Cancelled),
//! never its success event, so a caller's inflight slot is cleared by exactly
//! one terminal event either way.
//!
//! For any of that to happen the cancel has to reach the flag *while* the job
//! is running. That is why an actor reads its command channel on a separate
//! future from the one dispatching commands: a `Cancel` sitting behind the
//! command it names on a FIFO channel would only ever be seen after that
//! command had already finished.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::JobId;

/// A job's stop switch, shared between whoever sets it and the loop that polls
/// it. Cloning shares the flag.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// The live flags of an actor's in-flight jobs, plus the cancels that arrived
/// before the job they name got its turn.
#[derive(Debug, Default)]
pub struct CancelRegistry {
    flags: HashMap<JobId, CancelFlag>,
    /// Cancels for jobs still queued behind the one running. A caller that
    /// supersedes its own work sends the replacement and the cancel back to
    /// back, so this is the common case, not the race.
    early: HashSet<JobId>,
    /// The highest job this actor has started. Job ids are minted process-wide
    /// and monotonic, so an early cancel at or below this names a job that has
    /// already run and can be dropped instead of held forever.
    highest_begun: JobId,
}

impl CancelRegistry {
    /// Register `job` and hand back its flag, already raised if a cancel for it
    /// arrived first. The flag lives until [`finish`](Self::finish).
    pub fn begin(&mut self, job: JobId) -> CancelFlag {
        let flag = CancelFlag::default();
        if self.early.remove(&job) {
            flag.cancel();
        }
        self.highest_begun = self.highest_begun.max(job);
        self.flags.insert(job, flag.clone());
        flag
    }

    /// Raise `job`'s flag, or record it for when the job starts.
    pub fn cancel(&mut self, job: JobId) {
        match self.flags.get(&job) {
            Some(flag) => flag.cancel(),
            // Nothing older than the newest job we've started can still be
            // waiting, so a cancel down there is simply late.
            None if job > self.highest_begun => {
                self.early.insert(job);
            }
            None => {}
        }
    }

    pub fn finish(&mut self, job: JobId) {
        self.flags.remove(&job);
    }
}

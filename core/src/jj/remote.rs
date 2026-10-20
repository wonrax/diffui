use jj_lib::git::{GitProgress, GitSidebandLineTerminator, GitSubprocessCallback};

use crate::model::LoadProgress;

/// Scale for mapping git's normalized [`GitProgress::overall`] (0..1) onto the
/// integer `(loaded, total)` the activity bar reads. Arbitrary granularity.
pub(super) const GIT_PROGRESS_SCALE: usize = 1000;

/// [`GitSubprocessCallback`] for push/fetch: captures the remote/local sideband
/// lines git emits — shown in the activity's expanded row (a fetch's progress
/// summary, or a push's GitHub "create a pull request" hint + URL) — and mirrors
/// git's transfer progress onto the activity's [`LoadProgress`] so the toolbar
/// shows a determinate bar while the transfer runs.
pub(super) struct CollectingCallback {
    pub(super) lines: Vec<String>,
    progress: LoadProgress,
}

impl CollectingCallback {
    pub(super) fn new(progress: LoadProgress) -> Self {
        Self {
            lines: Vec::new(),
            progress,
        }
    }
}

impl GitSubprocessCallback for CollectingCallback {
    fn needs_progress(&self) -> bool {
        true
    }

    fn progress(&mut self, progress: &GitProgress) -> std::io::Result<()> {
        // git reports a running fraction; mirror it onto the activity's integer
        // (loaded, total). Only set the total once there's real progress so the
        // bar stays indeterminate (pulsing) until the transfer starts, rather
        // than flashing a 0/0 determinate bar (see `Activity::determinate`).
        let overall = progress.overall();
        if overall > 0.0 {
            self.progress.set_total(GIT_PROGRESS_SCALE);
            let loaded = (overall * GIT_PROGRESS_SCALE as f32) as usize;
            self.progress.set_loaded(loaded.min(GIT_PROGRESS_SCALE));
        }
        Ok(())
    }

    fn local_sideband(
        &mut self,
        message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        collect_sideband_lines(&mut self.lines, message);
        Ok(())
    }

    fn remote_sideband(
        &mut self,
        message: &[u8],
        _term: Option<GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        collect_sideband_lines(&mut self.lines, message);
        Ok(())
    }
}

/// Split git sideband output into individual lines (git interleaves `\r` for
/// progress redraws and `\n` for real lines), dropping blanks, so each entry is
/// one display line in the activity's expanded output.
pub(super) fn collect_sideband_lines(lines: &mut Vec<String>, message: &[u8]) {
    let text = String::from_utf8_lossy(message);
    for piece in text.split(['\n', '\r']) {
        let piece = piece.trim_end();
        if !piece.is_empty() {
            lines.push(piece.to_owned());
        }
    }
}

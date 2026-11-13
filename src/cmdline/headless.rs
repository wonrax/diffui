//! Running a jj line without the palette: what a declared command's
//! `run = "jj …"` and `diffui ctl jj` do. The line takes the palette's path
//! (parse, resolve its revsets on the repository, plan, lower), so it does
//! exactly what typing it would, on the tab it was started for whatever is on
//! screen when its revsets come back. A mutation gets the activity row, the
//! immutable confirm and the queue; a view opens; a passthrough streams to
//! its activity row.
//!
//! What differs from the palette: nothing is typed, so a rebase or squash
//! runs as written instead of arming target mode for a pick.

use std::collections::HashMap;
use std::fmt;

use diffui_core::JobId;
use diffui_core::cli::{ParseError, ParsedCommand};
use diffui_core::revset::{RevsetError, RevsetMatch, RevsetQuery};

use super::{PREFIX, PlanState, Resolution};
use crate::{Cx, Diffui, TabId};

/// One headless line, for as long as its revsets resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct LineId(u64);

/// Why a line didn't run, in the words the palette's footer would use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineError(pub(crate) String);

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Told what became of a line once it has run (`Ok` with what it did, in
/// words) or been refused.
pub(crate) type LineReply = Box<dyn FnOnce(&mut Diffui, Result<String, LineError>)>;

/// Whether a planned rebase or squash arms target mode (the palette, where
/// the user is there to pick) or runs as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arm {
    Drafts,
    Direct,
}

/// A line waiting on its revsets.
pub(crate) struct LineRun {
    id: LineId,
    parsed: Result<ParsedCommand, ParseError>,
    resolved: HashMap<String, Resolution>,
    in_flight: Vec<(JobId, String)>,
    reply: Option<LineReply>,
}

impl fmt::Debug for LineRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LineRun")
            .field("id", &self.id)
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

/// The lines a tab is running headless.
#[derive(Debug, Default)]
pub(crate) struct HeadlessLines {
    next: u64,
    runs: Vec<LineRun>,
}

impl Diffui {
    /// Run the jj command `line` (with or without its leading `jj`) on
    /// `tab`, the way the palette would. `Err` when it can't start: no such
    /// tab, or a line that doesn't parse. Otherwise `reply` hears how it went
    /// once its revsets have resolved.
    pub(crate) fn run_jj_line(
        &mut self,
        tab: TabId,
        line: &str,
        reply: Option<LineReply>,
        cx: &mut Cx,
    ) -> Result<LineId, LineError> {
        if self.tab(tab).is_none() {
            return Err(LineError("No such tab".to_owned()));
        }
        let line = line.trim();
        let line = match line.strip_prefix(PREFIX.trim_end()) {
            Some(rest) if rest.is_empty() || rest.starts_with(' ') => format!("jj{rest}"),
            _ => format!("{PREFIX}{line}"),
        };
        let parsed = self.cli_for(tab).parse(&line);
        if let Err(error) = &parsed {
            return Err(LineError(super::sentence(
                &error.message,
                error.hint.as_deref(),
            )));
        }
        let mut wanted: Vec<String> = Vec::new();
        for slot in parsed.iter().flat_map(|parsed| &parsed.revsets) {
            if !wanted.contains(&slot.revset) {
                wanted.push(slot.revset.clone());
            }
        }
        let Some(state) = self.tab_mut(tab) else {
            return Err(LineError("No such tab".to_owned()));
        };
        let id = LineId(state.cmdline.headless.next);
        state.cmdline.headless.next += 1;
        let mut in_flight = Vec::new();
        let mut tasks = Vec::new();
        for revset in wanted {
            let (job, effects) = state.session.resolve_revset(RevsetQuery::new(&revset));
            state
                .jobs
                .issue(job, crate::Pending::LineRevset(id, revset.clone()));
            in_flight.push((job, revset));
            tasks.push(effects);
        }
        let waiting = !in_flight.is_empty();
        state.cmdline.headless.runs.push(LineRun {
            id,
            parsed,
            resolved: HashMap::new(),
            in_flight,
            reply,
        });
        for effects in tasks {
            self.run_effects(tab, effects);
        }
        if !waiting {
            self.finish_line(tab, id, cx);
        }
        Ok(id)
    }

    /// One of a headless line's revsets resolved (or was refused).
    pub(crate) fn line_revset_resolved(
        &mut self,
        tab: TabId,
        id: LineId,
        job: JobId,
        revset: &str,
        result: Result<RevsetMatch, RevsetError>,
        cx: &mut Cx,
    ) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(run) = state
            .cmdline
            .headless
            .runs
            .iter_mut()
            .find(|run| run.id == id)
        else {
            return;
        };
        run.in_flight.retain(|(waiting, _)| *waiting != job);
        let set = result
            .as_ref()
            .map(|found| found.commits.iter().cloned().collect())
            .unwrap_or_default();
        run.resolved
            .insert(revset.to_owned(), Resolution { result, set });
        if run.in_flight.is_empty() {
            self.finish_line(tab, id, cx);
        }
    }

    /// Every revset is in: plan the line on its tab and run it, or say why
    /// not.
    fn finish_line(&mut self, tab: TabId, id: LineId, cx: &mut Cx) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(index) = state
            .cmdline
            .headless
            .runs
            .iter()
            .position(|run| run.id == id)
        else {
            return;
        };
        let mut run = state.cmdline.headless.runs.remove(index);
        let Some(state) = self.tab(tab) else {
            return;
        };
        let plan = self.plan_line(state, &run.parsed, &run.resolved);
        let outcome = match plan.state {
            PlanState::Ready(planned) => {
                let words = plan.words.to_string();
                self.execute(tab, planned, Arm::Direct, cx);
                Ok(words)
            }
            PlanState::Blocked { reason, .. } => Err(LineError(reason)),
            PlanState::Pending => Err(LineError("A revset didn't resolve".to_owned())),
        };
        if let Err(error) = &outcome {
            tracing::info!("jj line refused: {error}");
        }
        if let Some(reply) = run.reply.take() {
            reply(self, outcome);
        }
    }
}

//! Which graph the sidebar draws: the loaded one, or the post-op graph a
//! draft would leave behind ([`diffui_core::PreviewGraph`]). While a preview
//! is up, the list's rows are its display rows, so every index the list
//! reports goes through here on its way back to a loaded row.

use std::rc::Rc;

use diffui_core::{JobId, PreviewGraph};

use crate::revision_list::{self, DropSpot};
use crate::{Diffui, Message, TabId, TabState};
use iced::Task;

/// A preview laid over the rows of one graph load. Stale (and not drawn)
/// once the session's graph is replaced.
#[derive(Debug, Clone)]
pub(crate) struct Laid {
    pub(crate) version: Option<JobId>,
    pub(crate) graph: Rc<PreviewGraph>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GraphPreviewState {
    /// The preview of a draft just applied, held while its mutation and the
    /// reload after it run, so the sidebar goes from the preview straight to
    /// the real result instead of flashing the old graph in between.
    pub(crate) applied: Option<Laid>,
    /// What the sidebar drew after the last update, and the loaded row it was
    /// kept still around: a swap scrolls so that row stays put.
    drawn: Option<Rc<PreviewGraph>>,
    drawn_version: Option<JobId>,
    anchor: Option<usize>,
}

impl TabState {
    /// The preview the sidebar draws, if any.
    pub(crate) fn shown_preview(&self) -> Option<&Rc<PreviewGraph>> {
        let version = self.session.graph_version();
        let current = |laid: &Laid| laid.version == version;
        if let Some(draft) = self.op_draft() {
            return draft
                .graph
                .as_ref()
                .filter(|laid| current(laid))
                .map(|laid| &laid.graph);
        }
        self.graph_preview
            .applied
            .as_ref()
            .filter(|laid| current(laid) && self.session.busy())
            .map(|laid| &laid.graph)
    }

    /// The loaded row that pointing at list row `row` aims at. Only rows that
    /// stay in place can be a destination; ghosts, landed and created rows
    /// answer `None`.
    pub(crate) fn target_row(&self, row: usize) -> Option<usize> {
        match self.shown_preview() {
            Some(preview) => preview.target_at(row),
            None => (row < self.session.commits.len()).then_some(row),
        }
    }

    /// The loaded row list row `row` continues, whatever its role.
    pub(crate) fn origin_row(&self, row: usize) -> Option<usize> {
        match self.shown_preview() {
            Some(preview) => preview.origin_at(row),
            None => (row < self.session.commits.len()).then_some(row),
        }
    }

    /// The list row standing for loaded row `loaded`.
    pub(crate) fn list_row(&self, loaded: usize) -> Option<usize> {
        match self.shown_preview() {
            Some(preview) => preview.display_of(loaded),
            None => Some(loaded),
        }
    }

    /// A drop spot the list reported, in loaded rows. A gap only exists
    /// between two loaded rows that are neighbours there too.
    pub(crate) fn loaded_spot(&self, spot: DropSpot) -> Option<DropSpot> {
        match spot {
            DropSpot::OnRow(row) => self.target_row(row).map(DropSpot::OnRow),
            DropSpot::Gap { above, below } => {
                let above = self.target_row(above)?;
                let below = self.target_row(below)?;
                (below == above + 1).then_some(DropSpot::Gap { above, below })
            }
        }
    }
}

impl Diffui {
    /// Hold the active draft's preview through the mutation it is about to
    /// start. Only a preview of exactly what is being applied qualifies: one
    /// that has landed, not one still waiting on a newer candidate.
    pub(crate) fn hold_applied_preview(&mut self) {
        let tab = self.active_mut();
        let laid = tab.op_draft().and_then(|draft| {
            matches!(draft.preview, crate::DraftPreviewState::Ready(_))
                .then(|| draft.graph.clone())
                .flatten()
        });
        tab.graph_preview.applied = laid;
    }

    /// Run after every update: cancel a draft simulation nobody is waiting
    /// for any more (the draft ended, or its destination was dropped), so the
    /// repository stops spending time on it.
    pub(crate) fn cancel_unwanted_preview(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();
        let ids: Vec<TabId> = self.tabs.iter().map(|tab| tab.id).collect();
        for id in ids {
            let Some(tab) = self.tab_mut(id) else {
                continue;
            };
            let wanted = tab
                .op_draft()
                .is_some_and(|draft| matches!(draft.preview, crate::DraftPreviewState::Loading));
            if wanted {
                continue;
            }
            let Some(job) = tab.jobs.preview.take() else {
                continue;
            };
            tasks.push(self.run_effects(
                id,
                vec![diffui_core::Effect::Send(diffui_core::Command::Cancel {
                    job,
                })],
            ));
        }
        Task::batch(tasks)
    }

    /// Run after every update: when the sidebar is about to draw a different
    /// graph than last time (a preview landed, the draft ended, the applied
    /// result arrived), scroll so the row the user is working on stays where
    /// it was. Without this the rows above it changing count makes the whole
    /// list jump under the cursor.
    pub(crate) fn keep_graph_anchor(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let tab = self.active_mut();
        if tab
            .graph_preview
            .applied
            .as_ref()
            .is_some_and(|laid| laid.version != tab.session.graph_version() || !tab.session.busy())
            && tab.op_draft().is_none()
        {
            tab.graph_preview.applied = None;
        }
        let version = tab.session.graph_version();
        let shown = tab.shown_preview().cloned();
        let anchor = tab
            .op_draft()
            .and_then(|draft| match draft.hover_spot {
                Some(DropSpot::OnRow(row)) => Some(row),
                Some(DropSpot::Gap { below, .. }) => Some(below),
                None => draft.draft.candidate,
            })
            .or(tab.graph_preview.anchor)
            .or(tab.session.selected_commit_index);
        let has_draft = tab.op_draft().is_some();
        let state = &mut tab.graph_preview;
        let same = match (&shown, &state.drawn) {
            (Some(now), Some(before)) => Rc::ptr_eq(now, before),
            (None, None) => true,
            _ => false,
        };
        let before = std::mem::replace(&mut state.drawn, shown.clone());
        let before_version = std::mem::replace(&mut state.drawn_version, version);
        state.anchor = if has_draft { anchor } else { None };
        if same || before_version != version {
            return;
        }
        let Some(anchor) = anchor else { return };
        let row_in = |graph: &Option<Rc<PreviewGraph>>| match graph {
            Some(graph) => graph.display_of(anchor),
            None => Some(anchor),
        };
        let (Some(old), Some(new)) = (row_in(&before), row_in(&shown)) else {
            return;
        };
        if old == new {
            return;
        }
        let shift = (new as f64 - old as f64) * f64::from(revision_list::REVISION_ROW_HEIGHT);
        tab.sidebar_scroll_offset = (tab.sidebar_scroll_offset + shift).max(0.0);
        self.scroll_restore_token = self.scroll_restore_token.wrapping_add(1);
    }
}

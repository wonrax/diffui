//! Whole-revision operations layered on the draft / target-mode machinery:
//! `jj commit`, `jj new -A/-B`, `jj parallelize`, placed and batch
//! `jj duplicate`, batch rebase / squash / merge from the sidebar's marked
//! rows, divergent-change resolution and author resets.
//!
//! Everything here lowers to a [`mutations::MutationOp`] and runs through
//! [`Diffui::start_mutation_op`], or enters target mode through
//! [`Diffui::start_draft_with`], so labels, guards and previews stay shared
//! with the original ops.

use std::time::Duration;

use diffui_core::{DraftKind, PlacementKind, RevisionSelection};
use iced::Task;

use crate::message::{Message, UiEvent};
use crate::modes::{ModeKind, TabMode};
use crate::{Diffui, PreviewRequest};
use crate::{DraftPreviewState, DraftUi, mutations};

impl Diffui {
    /// Enter target mode for `kind` over `sources`, optionally with a
    /// destination already armed (`armed`: the candidate row and the
    /// placement it lands with) so Enter applies straight away.
    ///
    /// A [`DraftKind::New`] draft has no sources; every other kind needs at
    /// least one source that is still in the loaded graph.
    pub(crate) fn start_draft_with(
        &mut self,
        kind: DraftKind,
        sources: Vec<RevisionSelection>,
        armed: Option<(RevisionSelection, PlacementKind)>,
    ) -> Task<Message> {
        // jj-only, like every mutation.
        if !self.active().session.capabilities.mutate {
            return Task::none();
        }
        let sources: Vec<diffui_core::DraftSource> = sources
            .iter()
            .filter_map(|source| self.draft_source_for(source))
            .fold(Vec::new(), |mut kept, source| {
                if !kept
                    .iter()
                    .any(|seen: &diffui_core::DraftSource| seen.commit_id == source.commit_id)
                {
                    kept.push(source);
                }
                kept
            });
        if sources.is_empty() && kind != DraftKind::New {
            return Task::none();
        }
        // Target mode's plain keys and a half-written description can't both
        // own the keyboard. An unsaved edit wins and says so, the same way it
        // blocks a revision or tab switch; a clean editor just closes.
        if let Some(editor) = self.active_mut().description_editor_mut() {
            if editor.is_dirty() || editor.saving_activity.is_some() {
                editor.switch_blocked = true;
                return Task::none();
            }
            self.active_mut().pop_mode(ModeKind::Description);
        }
        let mut draft = diffui_core::OpDraft::with_sources(kind, sources);
        if let Some((target, placement)) = armed {
            draft.placement = placement;
            let target_id = self.draft_source_for(&target).map(|row| row.commit_id);
            draft.candidate = target_id
                .filter(|id| draft.target_valid(id))
                .and_then(|id| {
                    self.active()
                        .session
                        .commits
                        .iter()
                        .position(|row| row.commit_id() == id)
                });
        }
        // Two marked-row languages at once would be unreadable — the draft's
        // source wash takes over from the multi-select marks.
        let target = self.active_mut();
        target.revision_multi_selection.clear();
        target.push_mode(TabMode::Draft(DraftUi::new(draft)));
        self.pop_mode(ModeKind::ActivityPopover);
        self.pop_mode(ModeKind::Menu);
        self.kick_draft_preview()
    }

    /// The parallelize draft's preview depends on its sources alone, so it
    /// skips the destination plumbing [`Diffui::kick_draft_preview`] does for
    /// the other kinds. Debounced and version-guarded the same way.
    pub(crate) fn kick_parallelize_preview(&mut self) -> Task<Message> {
        let Some(ui) = self.active_mut().op_draft_mut() else {
            return Task::none();
        };
        if ui.draft.sources.len() < 2 {
            ui.preview = DraftPreviewState::Idle;
            ui.preview_request = None;
            ui.moved_highlight.clear();
            return Task::none();
        }
        let sources: Vec<RevisionSelection> = ui
            .draft
            .sources
            .iter()
            .map(|source| source.selection.clone())
            .collect();
        ui.preview_version = ui.preview_version.wrapping_add(1);
        let version = ui.preview_version;
        ui.preview = DraftPreviewState::Loading;
        // The request is destination-shaped for rebases; parallelize ignores
        // the slot (see `parked_preview_request`).
        ui.preview_request = Some(PreviewRequest {
            kind: ui.draft.kind,
            destination: mutations::Destination::Onto(sources[0].clone()),
            sources,
        });
        Task::perform(tokio::time::sleep(Duration::from_millis(250)), move |()| {
            Message::Ui(UiEvent::DraftPreviewKick(version))
        })
    }
}

/// Lower a parked preview request of one of this module's draft kinds into
/// the core's request. `None` for kinds that have no simulation.
pub(crate) fn parked_preview_request(
    request: PreviewRequest,
) -> Option<diffui_core::PreviewRequest> {
    match request.kind {
        DraftKind::Parallelize => Some(diffui_core::PreviewRequest::Parallelize {
            targets: request.sources,
        }),
        _ => None,
    }
}

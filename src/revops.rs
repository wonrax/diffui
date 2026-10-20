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

// ── Intents ─────────────────────────────────────────────────────────────────

/// This module's intents, carried by one [`crate::Action::Revops`] variant.
#[derive(Debug, Clone)]
pub(crate) enum RevopsAction {
    /// "Commit…": open the description editor on `@` in commit mode.
    BeginCommit,
    /// Enter target mode over several sources and/or with a destination
    /// already armed — the batch and "insert new change" entry points.
    StartDraft {
        kind: DraftKind,
        sources: Vec<RevisionSelection>,
        armed: Option<(RevisionSelection, PlacementKind)>,
    },
    /// Mark / unmark a row for a batch action — the keyboard's ⌘-click.
    ToggleMark(RevisionSelection),
}

impl Diffui {
    pub(crate) fn perform_revops(&mut self, action: RevopsAction) -> Task<Message> {
        match action {
            RevopsAction::BeginCommit => self.begin_commit(),
            RevopsAction::StartDraft {
                kind,
                sources,
                armed,
            } => self.start_draft_with(kind, sources, armed),
            RevopsAction::ToggleMark(selection) => {
                self.toggle_mark(&selection);
                Task::none()
            }
        }
    }

    fn toggle_mark(&mut self, selection: &RevisionSelection) {
        let Some(commit_id) = self
            .draft_source_for(selection)
            .map(|source| source.commit_id)
        else {
            return;
        };
        let marks = &mut self.active_mut().revision_multi_selection;
        match marks.iter().position(|id| *id == commit_id) {
            Some(found) => {
                marks.remove(found);
            }
            None => marks.push(commit_id),
        }
    }

    /// `jj commit`: the description editor opens on `@` in commit mode, and
    /// saving describes `@` and starts a new empty change on top in one
    /// operation. Browses to `@` first when it isn't on screen; the editor
    /// then opens when its diff lands (`open_pending_description_edit`).
    fn begin_commit(&mut self) -> Task<Message> {
        if !self.active().session.capabilities.mutate {
            return Task::none();
        }
        let target = RevisionSelection::WorkingCopy;
        if self.active().session.selected_revision == target {
            let task = self.open_description_editor();
            if let Some(editor) = self.active_mut().description_editor_mut()
                && editor.target == target
            {
                editor.commit = true;
            }
            return task;
        }
        if let Some(editor) = self.active_mut().description_editor_mut()
            && (editor.is_dirty() || editor.saving_activity.is_some())
        {
            editor.switch_blocked = true;
            return Task::none();
        }
        let tab = self.active_mut();
        tab.pending_description_edit = Some(target.clone());
        tab.pending_commit = true;
        self.select_revision(target)
    }
}

/// The op a description save runs: a plain describe, or `jj commit` when the
/// editor was opened in commit mode.
pub(crate) fn description_save_op(
    editor: &crate::DescriptionEditor,
) -> (mutations::MutationOp, &'static str) {
    let description = editor.text().trim_end().to_owned();
    if editor.commit {
        (mutations::MutationOp::Commit { description }, "Commit")
    } else {
        (
            mutations::MutationOp::Describe {
                target: editor.target.clone(),
                description,
            },
            "Update description",
        )
    }
}

/// Whether a mutation consumes the sidebar's marked rows: a batch op run on
/// more than one revision, or on exactly the marked set (a one-row batch
/// from a `selection.*` command).
pub(crate) fn consumes_marks(ui: &Diffui, op: &mutations::MutationOp) -> bool {
    use mutations::MutationOp;
    let targets = match op {
        MutationOp::Abandon { targets }
        | MutationOp::DuplicateTo { targets, .. }
        | MutationOp::Parallelize { targets }
        | MutationOp::Metaedit { targets, .. } => targets,
        MutationOp::Merge { parents } => parents,
        MutationOp::Rebase { sources, .. } => sources,
        MutationOp::Squash { from, .. } => from,
        _ => return false,
    };
    targets.len() > 1 || {
        let marked = marked_selections(ui);
        !marked.is_empty() && marked == *targets
    }
}

// ── Command predicates and builders ─────────────────────────────────────────

use crate::action::Action;
use crate::commands::{Availability, CommandArg, can_mutate, require, target_revision};

/// The sidebar's marked rows still in the loaded graph, in mark order, with
/// the working-copy row addressed as [`RevisionSelection::WorkingCopy`] so a
/// mutation never acts on a pre-snapshot `@`.
pub(crate) fn marked_selections(ui: &Diffui) -> Vec<RevisionSelection> {
    let commits = &ui.active().session.commits;
    let working_copy = commits.working_copy().map(|row| row.commit_id().to_owned());
    ui.active()
        .revision_multi_selection
        .iter()
        .filter(|id| commits.find_by_commit_id(id).is_some())
        .map(|id| diffui_core::canonical_selection(id, working_copy.as_deref()))
        .collect()
}

/// The batch a `selection.*` command acts on: what its menu row carried, or
/// — from the palette or a key — the live marks.
pub(crate) fn batch(ui: &Diffui, arg: &CommandArg) -> Vec<RevisionSelection> {
    match arg {
        CommandArg::Revisions(targets) => targets.clone(),
        _ => marked_selections(ui),
    }
}

fn batch_enabled(ui: &Diffui, arg: &CommandArg, minimum: usize) -> Availability {
    let targets = batch(ui, arg);
    require(!targets.is_empty(), "No revisions marked")?;
    require(targets.len() >= minimum, "Mark two or more revisions")?;
    can_mutate(ui, &CommandArg::Revisions(targets))
}

pub(crate) fn batch_of_one(ui: &Diffui, arg: &CommandArg) -> Availability {
    batch_enabled(ui, arg, 1)
}

pub(crate) fn batch_of_two(ui: &Diffui, arg: &CommandArg) -> Availability {
    batch_enabled(ui, arg, 2)
}

/// A draft over the batch.
pub(crate) fn batch_draft(ui: &Diffui, arg: &CommandArg, kind: DraftKind) -> Option<Action> {
    let sources = batch(ui, arg);
    (!sources.is_empty()).then_some(Action::Revops(RevopsAction::StartDraft {
        kind,
        sources,
        armed: None,
    }))
}

/// The loaded row a command's target names.
fn target_row<'a>(ui: &'a Diffui, arg: &CommandArg) -> Option<diffui_core::RowView<'a>> {
    let commits = &ui.active().session.commits;
    match target_revision(ui, arg)? {
        RevisionSelection::WorkingCopy => commits.working_copy(),
        RevisionSelection::Commit(hex) => commits.find_by_commit_id(&hex),
    }
}

/// "Commit…" is offered for the working copy only — `jj commit` has no `-r`.
pub(crate) fn can_commit(ui: &Diffui, arg: &CommandArg) -> Availability {
    can_mutate(ui, arg)?;
    require(
        match arg {
            CommandArg::None => ui.active().session.commits.working_copy().is_some(),
            _ => target_row(ui, arg).is_some_and(|row| row.is_working_copy()),
        },
        "Only the working copy commits",
    )
}

/// The divergent-change verbs only make sense on a row that is one copy of
/// a divergent change.
pub(crate) fn is_divergent_target(ui: &Diffui, arg: &CommandArg) -> Availability {
    can_mutate(ui, arg)?;
    require(
        target_row(ui, arg).is_some_and(|row| row.is_divergent()),
        "Change isn't divergent",
    )
}

/// How many copies of `row`'s change the loaded graph shows.
fn loaded_copies(ui: &Diffui, change_id: &str) -> usize {
    ui.active()
        .session
        .commits
        .iter()
        .filter(|row| row.change_id() == change_id && !row.is_hidden())
        .count()
}

/// A `New` draft armed on the command's target with `placement`.
pub(crate) fn new_change_at(
    ui: &Diffui,
    arg: CommandArg,
    placement: PlacementKind,
) -> Option<Action> {
    target_revision(ui, &arg).map(|target| {
        Action::Revops(RevopsAction::StartDraft {
            kind: DraftKind::New,
            sources: Vec::new(),
            armed: Some((target, placement)),
        })
    })
}

/// Insert a new change at `placement` of the command's target right away.
pub(crate) fn insert_new_change(
    ui: &Diffui,
    arg: CommandArg,
    placement: PlacementKind,
) -> Option<Action> {
    target_revision(ui, &arg).map(|target| {
        Action::Mutate(mutations::MutationOp::NewAt {
            destination: mutations::placed(target, placement),
        })
    })
}

/// `jj metaedit` on the command's target — or on every row of a batch.
pub(crate) fn metaedit(
    ui: &Diffui,
    arg: CommandArg,
    update_author: bool,
    update_author_timestamp: bool,
) -> Option<Action> {
    let targets = match arg {
        CommandArg::Revisions(targets) => targets,
        other => vec![target_revision(ui, &other)?],
    };
    Some(Action::Mutate(mutations::MutationOp::Metaedit {
        targets,
        update_author,
        update_author_timestamp,
    }))
}

// ── Menus ───────────────────────────────────────────────────────────────────

use crate::menu::MenuEntry;

impl Diffui {
    /// The context menu over a marked set: every batch verb, each naming the
    /// set it acts on.
    pub(crate) fn batch_menu_tree(&self, marked: Vec<RevisionSelection>) -> Vec<MenuEntry> {
        let n = marked.len();
        let set = || CommandArg::Revisions(marked.clone());
        let mut items = vec![
            MenuEntry::Submenu {
                label: format!("Rebase {n} revisions"),
                items: vec![
                    self.menu_row("Onto\u{2026}", "selection.rebase.start", set()),
                    self.menu_row(
                        "With descendants onto\u{2026}",
                        "selection.rebase.descendants.start",
                        set(),
                    ),
                ],
            },
            self.menu_row(
                format!("Squash {n} revisions into\u{2026}"),
                "selection.squash.start",
                set(),
            ),
            MenuEntry::Submenu {
                label: format!("Duplicate {n} revisions"),
                items: vec![
                    self.menu_row("Alongside themselves", "selection.duplicate", set()),
                    self.menu_row("Onto\u{2026}", "selection.duplicate.start", set()),
                ],
            },
        ];
        if n >= 2 {
            items.push(self.menu_row(
                format!("New change on all {n} (merge)"),
                "selection.new-merge",
                set(),
            ));
            items.push(self.menu_row(
                format!("Parallelize {n} revisions\u{2026}"),
                "selection.parallelize",
                set(),
            ));
        }
        items.push(MenuEntry::Submenu {
            label: "Author".to_owned(),
            items: vec![
                self.menu_row("Reset author to me", "selection.reset-author", set()),
                self.menu_row(
                    "Update author date to now",
                    "selection.update-author-timestamp",
                    set(),
                ),
            ],
        });
        items.push(MenuEntry::Separator);
        items.push(self.menu_row(format!("Abandon {n} revisions"), "revision.abandon", set()));
        items.push(MenuEntry::Separator);
        if n == 2 {
            items.push(self.menu_item("compare.marked", CommandArg::None));
        }
        items.push(self.menu_item("selection.clear", CommandArg::None));
        items
    }

    /// The single-revision rows this module adds to the revision context
    /// menu: commit (on `@`), the "insert new change" submenu, and the
    /// divergent-change and author submenus.
    pub(crate) fn revops_menu_rows(&self, selection: &RevisionSelection) -> Vec<MenuEntry> {
        let on = || CommandArg::Revision(selection.clone());
        let mut rows = Vec::new();
        if can_commit(self, &on()).is_ok() {
            rows.push(self.menu_item("revision.commit", on()));
        }
        rows.push(MenuEntry::Submenu {
            label: "Insert new change".to_owned(),
            items: vec![
                self.menu_row("After this revision", "revision.new-after", on()),
                self.menu_row("Before this revision", "revision.new-before", on()),
                self.menu_row("Somewhere else\u{2026}", "revision.new.start", on()),
            ],
        });
        rows
    }

    /// Rows after the history-surgery group: duplicate placement, author
    /// metadata and — on a divergent row — its resolution.
    pub(crate) fn revops_surgery_rows(&self, selection: &RevisionSelection) -> Vec<MenuEntry> {
        let on = || CommandArg::Revision(selection.clone());
        let mut rows = vec![
            MenuEntry::Submenu {
                label: "Duplicate".to_owned(),
                items: vec![
                    self.menu_row("Alongside itself", "revision.duplicate", on()),
                    self.menu_row("Onto\u{2026}", "revision.duplicate.start", on()),
                ],
            },
            MenuEntry::Submenu {
                label: "Author".to_owned(),
                items: vec![
                    self.menu_item("revision.reset-author", on()),
                    self.menu_item("revision.update-author-timestamp", on()),
                ],
            },
        ];
        if let Some(row) = target_row(self, &on())
            && row.is_divergent()
        {
            let copies = loaded_copies(self, row.change_id());
            let others = copies.saturating_sub(1);
            let keep = match others {
                0 => "Keep this copy, abandon the others".to_owned(),
                1 => "Keep this copy, abandon the other one".to_owned(),
                n => format!("Keep this copy, abandon the other {n}"),
            };
            rows.push(MenuEntry::Submenu {
                label: "Divergent change".to_owned(),
                items: vec![
                    MenuEntry::Disabled {
                        label: "Several visible commits share this change id".to_owned(),
                        reason: None,
                    },
                    MenuEntry::Disabled {
                        label: "(usually the same change rewritten concurrently)".to_owned(),
                        reason: None,
                    },
                    MenuEntry::Separator,
                    self.menu_row(keep, "revision.divergent.keep", on()),
                    self.menu_item("revision.divergent.abandon", on()),
                ],
            });
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::tests::{app, push_tab};
    use diffui_core::{CommitStoreBuilder, CommitSummary};

    fn row(id: &str) -> CommitSummary {
        CommitSummary {
            change_id: format!("{id}{id}{id}{id}"),
            commit_id: id.repeat(8),
            shortest_change_id_len: None,
            description: String::new(),
            author: String::new(),
            has_description: false,
            is_empty: None,
            has_conflict: false,
            is_divergent: false,
            is_hidden: false,
            change_offset: None,
            is_working_copy: false,
            is_immutable: false,
            on_trunk: false,
            bookmarks: Vec::new(),
            parent_ids: Vec::new(),
        }
    }

    /// A jj-capable app over `@` = "w", then "a", "b", "c".
    fn loaded(rows: Vec<CommitSummary>) -> Diffui {
        let mut ui = app();
        push_tab(&mut ui, "/tmp/revops");
        let tab = ui.active_mut();
        tab.session.capabilities.mutate = true;
        let mut builder = CommitStoreBuilder::with_capacity(rows.len());
        for row in rows {
            builder.push(row);
        }
        tab.session.commits = builder.finish();
        ui
    }

    fn graph() -> Vec<CommitSummary> {
        let mut wc = row("w");
        wc.is_working_copy = true;
        vec![wc, row("a"), row("b"), row("c")]
    }

    fn labels(entries: &[MenuEntry]) -> Vec<String> {
        let mut out = Vec::new();
        for entry in entries {
            match entry {
                MenuEntry::Item { label, .. } | MenuEntry::Disabled { label, .. } => {
                    out.push(label.clone())
                }
                MenuEntry::Submenu { label, items } => {
                    out.push(label.clone());
                    out.extend(labels(items));
                }
                MenuEntry::Separator => {}
            }
        }
        out
    }

    #[test]
    fn marks_address_the_working_copy_as_such_and_skip_vanished_rows() {
        let mut ui = loaded(graph());
        ui.active_mut().revision_multi_selection =
            vec!["w".repeat(8), "gone".to_owned(), "b".repeat(8)];
        assert_eq!(
            marked_selections(&ui),
            vec![
                RevisionSelection::WorkingCopy,
                RevisionSelection::Commit("b".repeat(8)),
            ]
        );
    }

    #[test]
    fn the_batch_menu_offers_set_verbs_by_size() {
        let ui = loaded(graph());
        let two = ui.batch_menu_tree(vec![
            RevisionSelection::Commit("a".repeat(8)),
            RevisionSelection::Commit("b".repeat(8)),
        ]);
        let two = labels(&two);
        for expected in [
            "Rebase 2 revisions",
            "Squash 2 revisions into\u{2026}",
            "Duplicate 2 revisions",
            "New change on all 2 (merge)",
            "Parallelize 2 revisions\u{2026}",
            "Abandon 2 revisions",
        ] {
            assert!(
                two.iter().any(|label| label == expected),
                "{expected} in {two:?}"
            );
        }
        let one = labels(&ui.batch_menu_tree(vec![RevisionSelection::Commit("a".repeat(8))]));
        assert!(!one.iter().any(|label| label.starts_with("Parallelize")));
        assert!(!one.iter().any(|label| label.starts_with("New change on")));
    }

    #[test]
    fn parallelize_from_the_marks_enters_a_two_source_draft() {
        // Entering the draft kicks the debounced preview, whose timer needs a
        // runtime to be created in.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a test runtime");
        let _context = runtime.enter();
        let mut ui = loaded(graph());
        ui.active_mut().revision_multi_selection = vec!["a".repeat(8), "b".repeat(8)];
        let _ = ui.run_command("selection.parallelize", CommandArg::None);
        let draft = &ui.active().op_draft().expect("target mode is on").draft;
        assert_eq!(draft.kind, DraftKind::Parallelize);
        assert_eq!(draft.sources.len(), 2);
        assert!(
            ui.active().revision_multi_selection.is_empty(),
            "the draft wash replaces the marks"
        );
        assert!(matches!(
            draft.op_from_sources(),
            Some(mutations::MutationOp::Parallelize { .. })
        ));
    }

    #[test]
    fn insert_new_change_somewhere_else_arms_the_clicked_row() {
        let mut ui = loaded(graph());
        let b = RevisionSelection::Commit("b".repeat(8));
        let _ = ui.run_command("revision.new.start", CommandArg::Revision(b));
        let draft = &ui.active().op_draft().expect("target mode is on").draft;
        assert_eq!(draft.kind, DraftKind::New);
        assert!(draft.sources.is_empty());
        assert_eq!(draft.candidate, Some(2), "armed on b's row");
    }

    #[test]
    fn divergent_rows_get_the_resolution_submenu() {
        let mut rows = graph();
        rows[1].is_divergent = true;
        rows[2].is_divergent = true;
        rows[2].change_id = rows[1].change_id.clone();
        let ui = loaded(rows);
        let divergent = labels(&ui.revops_surgery_rows(&RevisionSelection::Commit("a".repeat(8))));
        assert!(
            divergent
                .iter()
                .any(|label| label == "Keep this copy, abandon the other one"),
            "{divergent:?}"
        );
        let plain = labels(&ui.revops_surgery_rows(&RevisionSelection::Commit("c".repeat(8))));
        assert!(!plain.iter().any(|label| label == "Divergent change"));
    }

    #[test]
    fn commit_is_offered_on_the_working_copy_only() {
        let ui = loaded(graph());
        assert!(can_commit(&ui, &CommandArg::None).is_ok());
        assert!(can_commit(&ui, &CommandArg::Revision(RevisionSelection::WorkingCopy)).is_ok());
        assert_eq!(
            can_commit(
                &ui,
                &CommandArg::Revision(RevisionSelection::Commit("a".repeat(8)))
            ),
            Err("Only the working copy commits")
        );
    }
}

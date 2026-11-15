//! The app's side of review notes: each jj tab's threads and where they sit,
//! kept current from the store (watched) and the repository (re-located when
//! the graph moves), and what the user does to them.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use diffui_core::{DiffFileStatus, DiffLineKind, hunk_conflict_region};
use futures::StreamExt as _;

use super::locate::{Located, Place, locate};
use super::store::{self, Role, Side, Store, Thread};
use crate::diff_view::{DocSpot, LineAt};
use crate::modes::ModeKind;
use crate::{Action, Cx, Diffui, MainView, TabId, TabState, Vcs};

/// What the user does to notes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NotesMsg {
    /// Show a thread whole, or fold it back to one line.
    Toggle(String),
    /// Open the reply field under a thread.
    StartReply(String),
    ReplyEdited(String),
    SendReply,
    CancelReply,
    Resolve(Vec<String>),
    Reopen(Vec<String>),
    /// Ask before deleting these; [`NotesMsg::Delete`] does it.
    AskDelete(Vec<String>),
    Delete(Vec<String>),
    CancelDelete,
    /// Show or hide resolved threads in the diff.
    ToggleResolved,
    /// Move to the next (`1`) or previous (`-1`) open note or conflict of the
    /// diff.
    Step(isize),
    /// Show a thread in its change's diff.
    Open(String),
    /// Open the Notes view on one change's notes.
    ShowChange(String),
    /// The Notes view's filter.
    Filter(Filter),
    /// Every change's notes again, after [`NotesMsg::ShowChange`].
    AllChanges,
    Author(Option<String>),
    Select(String),
    SelectAll(bool),
}

/// Which notes the Notes view lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Filter {
    /// Every open note on a live change.
    #[default]
    Open,
    /// The agent wrote last: yours to read.
    WaitingOnYou,
    /// You replied: the agent's turn.
    WaitingOnAgent,
    Resolved,
    /// Its lines are gone.
    Outdated,
    /// Its change was abandoned.
    Archived,
}

impl Filter {
    pub(crate) const ALL: [Filter; 6] = [
        Filter::Open,
        Filter::WaitingOnYou,
        Filter::WaitingOnAgent,
        Filter::Resolved,
        Filter::Outdated,
        Filter::Archived,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Filter::Open => "Open",
            Filter::WaitingOnYou => "Waiting on you",
            Filter::WaitingOnAgent => "Waiting on agent",
            Filter::Resolved => "Resolved",
            Filter::Outdated => "Outdated",
            Filter::Archived => "Archived",
        }
    }

    pub(crate) fn admits(self, note: &Note) -> bool {
        let thread = &note.thread;
        let live = !note.located.archived();
        match self {
            Filter::Open => live && thread.resolved.is_none(),
            Filter::WaitingOnYou => live && thread.waiting_on_user(),
            Filter::WaitingOnAgent => live && thread.waiting_on_agent(),
            Filter::Resolved => live && thread.resolved.is_some(),
            Filter::Outdated => live && note.located.outdated(),
            Filter::Archived => !live,
        }
    }
}

/// A thread and where it sits now.
#[derive(Debug, Clone)]
pub(crate) struct Note {
    pub(crate) thread: Thread,
    pub(crate) located: Located,
}

impl Note {
    /// Counts in the log and the file list: open, on a live change.
    pub(crate) fn counts(&self) -> bool {
        self.thread.resolved.is_none() && !self.located.archived()
    }
}

/// The reply being written: the [`crate::modes::TabMode::NoteReply`] mode.
#[derive(Debug, Clone)]
pub(crate) struct NoteReply {
    pub(crate) id: String,
    pub(crate) draft: String,
}

/// The Notes view's own state.
#[derive(Debug, Default)]
pub(crate) struct ViewState {
    pub(crate) filter: Filter,
    pub(crate) author: Option<String>,
    /// Only this change's notes, from a log row's count.
    pub(crate) change: Option<String>,
    pub(crate) selected: HashSet<String>,
    /// Asked to delete these; waiting for the answer.
    pub(crate) confirm_delete: Option<Vec<String>>,
}

/// The store watch: dropped with the tab.
struct Watch {
    _watcher: notify::RecommendedWatcher,
    _task: gpui_kit::Task<()>,
}

/// One tab's notes.
#[derive(Default)]
pub(crate) struct TabNotes {
    pub(crate) store: Option<Store>,
    /// Every thread of the repository and where it sits, as last loaded.
    pub(crate) notes: Rc<Vec<Note>>,
    /// The operation the threads were located at.
    located_at: Option<String>,
    /// Something changed since the last load.
    stale: bool,
    loading: bool,
    watch: Option<Watch>,
    /// Threads shown whole.
    pub(crate) expanded: HashSet<String>,
    /// Resolved threads are left out of the diff.
    pub(crate) hide_resolved: bool,
    /// The spot next/previous last moved to, by the diff it was in.
    step: Option<(String, DocSpot)>,
    /// What the diff scrolls to, and the token that asks.
    pub(crate) reveal: Option<(DocSpot, u64)>,
    /// A thread to scroll to once its change's diff is on screen.
    pending_open: Option<String>,
    pub(crate) view: ViewState,
    /// Markdown of each message, parsed once per load.
    pub(crate) parsed: std::collections::HashMap<(String, usize), Rc<Vec<crate::markdown::Block>>>,
}

impl std::fmt::Debug for TabNotes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TabNotes")
            .field("notes", &self.notes.len())
            .field("located_at", &self.located_at)
            .finish_non_exhaustive()
    }
}

impl TabNotes {
    pub(crate) fn get(&self, id: &str) -> Option<&Note> {
        self.notes.iter().find(|note| note.thread.id == id)
    }

    /// Open notes on `change_id`.
    pub(crate) fn open_on_change(&self, change_id: &str) -> usize {
        self.notes
            .iter()
            .filter(|note| note.counts() && note.thread.change_id == change_id)
            .count()
    }

    /// Load again: the store changed.
    pub(crate) fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// Whether everything is loaded and nothing is waiting to load.
    #[cfg(test)]
    pub(crate) fn settled(&self) -> bool {
        !self.loading && !self.stale && self.located_at.is_some()
    }
}

/// Whether the active tab can have notes: a jj repository.
pub(crate) fn available(ui: &Diffui) -> bool {
    ui.active()
        .repository
        .as_ref()
        .is_some_and(|repository| repository.vcs == Vcs::Jj)
}

/// The change the diff on screen shows, when it shows one change.
pub(crate) fn shown_change(tab: &TabState) -> Option<&str> {
    if tab.session.document_base != diffui_core::DiffBase::Parents {
        return None;
    }
    tab.session.revision_details.as_ref()?.change_id.as_deref()
}

/// Where `note` goes in the diff on screen, or `None` when it isn't on its
/// change, or is hidden.
pub(crate) fn spot_of(tab: &TabState, note: &Note) -> Option<DocSpot> {
    if shown_change(tab)? != note.thread.change_id || note.located.archived() {
        return None;
    }
    if tab.notes.hide_resolved && note.thread.resolved.is_some() {
        return None;
    }
    let files = &tab.session.document.files;
    let Some(anchor) = &note.thread.anchor else {
        return Some(DocSpot::Top);
    };
    let Some(file) = files.iter().position(|file| {
        file.path == anchor.path || file.old_path.as_deref() == Some(&anchor.path)
    }) else {
        return Some(DocSpot::Top);
    };
    let (Place::Lines { end, .. }, Some(lines)) = (&note.located.place, &anchor.lines) else {
        return Some(DocSpot::File(file));
    };
    for (hunk, view) in files[file].hunks.iter().enumerate() {
        for (line, row) in view.lines.iter().enumerate() {
            let number = match lines.side {
                Side::New if row.kind != DiffLineKind::Deletion => row.new_line,
                Side::Old if row.kind != DiffLineKind::Addition => row.old_line,
                _ => None,
            };
            if number == Some(*end) {
                return Some(DocSpot::Line(LineAt { file, hunk, line }));
            }
        }
    }
    Some(DocSpot::File(file))
}

/// The notes the diff on screen shows, each with its spot, in document order.
pub(crate) fn diff_notes(tab: &TabState) -> Vec<(DocSpot, &Note)> {
    let mut placed: Vec<(DocSpot, &Note)> = tab
        .notes
        .notes
        .iter()
        .filter_map(|note| Some((spot_of(tab, note)?, note)))
        .collect();
    placed.sort_by(|(a, a_note), (b, b_note)| {
        a.cmp(b)
            .then_with(|| first_at(&a_note.thread).cmp(&first_at(&b_note.thread)))
    });
    placed
}

fn first_at(thread: &Thread) -> u64 {
    thread.messages.first().map_or(0, |message| message.at)
}

/// What a spot of the diff is, for next/previous and the overview strip.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SpotKind {
    Note { id: String, label: String },
    Conflict { label: String },
}

/// The diff's important spots in document order: open notes and conflicts.
pub(crate) fn spots(tab: &TabState) -> Vec<(DocSpot, SpotKind)> {
    let mut spots: Vec<(DocSpot, SpotKind)> = diff_notes(tab)
        .into_iter()
        .filter(|(_, note)| note.thread.resolved.is_none())
        .map(|(spot, note)| {
            let first = note
                .thread
                .messages
                .first()
                .map_or("", |message| message.body.lines().next().unwrap_or(""));
            let author = note
                .thread
                .messages
                .first()
                .map_or("", |m| m.author.as_str());
            (
                spot,
                SpotKind::Note {
                    id: note.thread.id.clone(),
                    label: format!("{author}: {first}"),
                },
            )
        })
        .collect();
    for (file, diff) in tab.session.document.files.iter().enumerate() {
        let mut any = false;
        for (hunk, view) in diff.hunks.iter().enumerate() {
            if hunk_conflict_region(view).is_some() {
                any = true;
                spots.push((
                    DocSpot::Line(LineAt {
                        file,
                        hunk,
                        line: 0,
                    }),
                    SpotKind::Conflict {
                        label: format!("Conflict in {}", diff.path),
                    },
                ));
            }
        }
        if !any && diff.status == DiffFileStatus::Conflicted {
            spots.push((
                DocSpot::File(file),
                SpotKind::Conflict {
                    label: format!("Conflict in {}", diff.path),
                },
            ));
        }
    }
    spots.sort_by_key(|(spot, _)| *spot);
    spots
}

/// Open notes on document file `index` of the diff on screen.
pub(crate) fn file_note_count(ui: &Diffui, index: usize) -> usize {
    let tab = ui.active();
    let Some(path) = tab.session.document.files.get(index).map(|file| &file.path) else {
        return 0;
    };
    let Some(change) = shown_change(tab) else {
        return 0;
    };
    tab.notes
        .notes
        .iter()
        .filter(|note| {
            note.counts()
                && note.thread.change_id == change
                && note
                    .thread
                    .anchor
                    .as_ref()
                    .is_some_and(|anchor| &anchor.path == path)
        })
        .count()
}

/// What a changed-files row counts ahead of its changes: the file's open
/// notes, and its conflicts (regions, or one for a file conflicted whole).
pub(crate) fn file_badges(
    ui: &Diffui,
    index: usize,
    theme: crate::theme::ThemeSpec,
) -> Vec<(&'static str, usize, crate::theme::Color)> {
    let mut badges = Vec::new();
    let notes = file_note_count(ui, index);
    if notes > 0 {
        badges.push((crate::icons::MESSAGE_SQUARE, notes, theme.accent));
    }
    if let Some(file) = ui.active().session.document.files.get(index) {
        let regions = file
            .hunks
            .iter()
            .filter(|hunk| hunk_conflict_region(hunk).is_some())
            .count();
        let conflicts = match regions {
            0 if file.status == DiffFileStatus::Conflicted => 1,
            regions => regions,
        };
        if conflicts > 0 {
            badges.push((
                crate::icons::ALERT_TRIANGLE,
                conflicts,
                theme.conflict_marker,
            ));
        }
    }
    badges
}

/// A change written to the store, off the UI thread.
enum Write {
    Reply { id: String, body: String },
    Resolve(Vec<String>),
    Reopen(Vec<String>),
    Delete(Vec<String>),
}

impl Diffui {
    /// Starts each jj tab's watch, and loads what changed. From reconcile.
    pub(crate) fn sync_notes(&mut self, cx: &mut Cx) {
        let ids: Vec<TabId> = self.tabs.iter().map(|tab| tab.id).collect();
        for tab in ids {
            let Some(state) = self.tab_mut(tab) else {
                continue;
            };
            let Some(root) = state
                .repository
                .as_ref()
                .filter(|repository| repository.vcs == Vcs::Jj)
                .map(|repository| repository.root.clone())
            else {
                continue;
            };
            if state.notes.store.is_none() {
                let Some(store) =
                    diffui_core::notes::repo_dir(&root).and_then(|dir| Store::for_repo(&dir))
                else {
                    continue;
                };
                state.notes.watch = watch(&store, tab, cx);
                state.notes.store = Some(store);
                state.notes.stale = true;
            }
            let operation = state.session.graph_operation().map(str::to_owned);
            if operation.is_some() && operation != state.notes.located_at {
                state.notes.stale = true;
            }
            if state.notes.stale && !state.notes.loading && operation.is_some() {
                self.load_notes(tab, root, operation, cx);
            }
            self.settle_pending_open(tab);
        }
    }

    fn load_notes(&mut self, tab: TabId, root: PathBuf, operation: Option<String>, cx: &mut Cx) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(store) = state.notes.store.clone() else {
            return;
        };
        state.notes.loading = true;
        state.notes.stale = false;
        let (send, receive) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("notes-load".to_owned())
            .spawn(move || {
                let threads = store.load();
                let located = diffui_core::notes::NotesRepo::open(&root, false)
                    .map(|repo| locate(&repo, &threads));
                let _ = send.send((threads, located.map_err(|error| format!("{error:#}"))));
            });
        if spawned.is_err() {
            state.notes.loading = false;
            return;
        }
        Self::run_async(
            async move { receive.await.ok() },
            cx,
            move |this, loaded, _| {
                let Some(state) = this.tab_mut(tab) else {
                    return;
                };
                state.notes.loading = false;
                match loaded {
                    Some((threads, Ok(located))) => {
                        let notes: Vec<Note> = threads
                            .into_iter()
                            .zip(located)
                            .map(|(thread, located)| Note { thread, located })
                            .collect();
                        state.notes.parsed.clear();
                        for note in &notes {
                            for (index, message) in note.thread.messages.iter().enumerate() {
                                state.notes.parsed.insert(
                                    (note.thread.id.clone(), index),
                                    Rc::new(crate::markdown::parse(&message.body)),
                                );
                            }
                        }
                        state.notes.notes = Rc::new(notes);
                        state.notes.located_at = operation;
                    }
                    Some((_, Err(error))) => {
                        tracing::warn!("couldn't locate review notes: {error}");
                        state.notes.located_at = operation;
                    }
                    None => {}
                }
            },
        );
    }

    /// The store changed under `tab`'s repository.
    fn on_notes_changed(&mut self, tab: TabId) {
        if let Some(state) = self.tab_mut(tab) {
            state.notes.mark_stale();
        }
    }

    fn write_notes(&mut self, write: Write, cx: &mut Cx) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        let Some(store) = self.active().notes.store.clone() else {
            return;
        };
        let (send, receive) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("notes-write".to_owned())
            .spawn(move || {
                let result = match write {
                    Write::Reply { id, body } => store
                        .update(&id, |thread| {
                            store::reply(thread, "you", Role::User, &body);
                            true
                        })
                        .map(|_| ()),
                    Write::Resolve(ids) => ids.iter().try_for_each(|id| {
                        store
                            .update(id, |thread| {
                                let open = thread.resolved.is_none();
                                if open {
                                    thread.resolved = Some(store::Resolved {
                                        by: "you".to_owned(),
                                        at: store::now(),
                                    });
                                }
                                open
                            })
                            .map(|_| ())
                    }),
                    Write::Reopen(ids) => ids.iter().try_for_each(|id| {
                        store
                            .update(id, |thread| thread.resolved.take().is_some())
                            .map(|_| ())
                    }),
                    Write::Delete(ids) => store.delete(&ids).map(|_| ()),
                };
                let _ = send.send(result.map_err(|error| error.to_string()));
            });
        if spawned.is_err() {
            return;
        }
        Self::run_async(
            async move { receive.await.ok() },
            cx,
            move |this, result, _| {
                if let Some(Err(error)) = result {
                    this.push_error_toast("Couldn't save the note", &error);
                }
                this.on_notes_changed(tab);
            },
        );
    }

    pub(crate) fn on_notes(&mut self, message: NotesMsg, cx: &mut Cx) {
        match message {
            NotesMsg::Toggle(id) => {
                let expanded = &mut self.active_mut().notes.expanded;
                if !expanded.remove(&id) {
                    expanded.insert(id);
                }
            }
            NotesMsg::StartReply(id) => {
                let tab = self.active_mut();
                tab.notes.expanded.insert(id.clone());
                tab.push_mode(crate::modes::TabMode::NoteReply(NoteReply {
                    id,
                    draft: String::new(),
                }));
                crate::input::focus(super::inline::REPLY_FIELD, cx);
            }
            NotesMsg::ReplyEdited(text) => {
                if let Some(reply) = self.active_mut().note_reply_mut() {
                    reply.draft = text;
                }
            }
            NotesMsg::SendReply => {
                let Some(reply) = self.active().note_reply().cloned() else {
                    return;
                };
                let body = reply.draft.trim().to_owned();
                if body.is_empty() {
                    return;
                }
                self.active_mut().pop_mode(ModeKind::NoteReply);
                self.write_notes(Write::Reply { id: reply.id, body }, cx);
            }
            NotesMsg::CancelReply => {
                self.active_mut().pop_mode(ModeKind::NoteReply);
            }
            NotesMsg::Resolve(ids) => {
                self.active_mut().notes.view.selected.clear();
                self.write_notes(Write::Resolve(ids), cx);
            }
            NotesMsg::Reopen(ids) => self.write_notes(Write::Reopen(ids), cx),
            NotesMsg::AskDelete(ids) => {
                self.active_mut().notes.view.confirm_delete = Some(ids);
            }
            NotesMsg::CancelDelete => {
                self.active_mut().notes.view.confirm_delete = None;
            }
            NotesMsg::Delete(ids) => {
                let view = &mut self.active_mut().notes.view;
                view.confirm_delete = None;
                view.selected.clear();
                self.write_notes(Write::Delete(ids), cx);
            }
            NotesMsg::ToggleResolved => {
                let notes = &mut self.active_mut().notes;
                notes.hide_resolved = !notes.hide_resolved;
            }
            NotesMsg::Step(by) => self.step_spot(by),
            NotesMsg::Open(id) => self.open_note(&id, cx),
            NotesMsg::ShowChange(change) => {
                let view = &mut self.active_mut().notes.view;
                view.change = Some(change);
                view.selected.clear();
                self.perform(Action::SetMainView(MainView::Notes), cx);
            }
            NotesMsg::Filter(filter) => {
                let view = &mut self.active_mut().notes.view;
                view.filter = filter;
                view.selected.clear();
                view.confirm_delete = None;
            }
            NotesMsg::AllChanges => {
                let view = &mut self.active_mut().notes.view;
                view.change = None;
                view.selected.clear();
            }
            NotesMsg::Author(author) => {
                let view = &mut self.active_mut().notes.view;
                view.author = author;
                view.selected.clear();
            }
            NotesMsg::Select(id) => {
                let selected = &mut self.active_mut().notes.view.selected;
                if !selected.remove(&id) {
                    selected.insert(id);
                }
            }
            NotesMsg::SelectAll(all) => {
                let ids: Vec<String> = if all {
                    super::view::listed(self.active())
                        .into_iter()
                        .map(|note| note.thread.id.clone())
                        .collect()
                } else {
                    Vec::new()
                };
                self.active_mut().notes.view.selected = ids.into_iter().collect();
            }
        }
    }

    fn step_spot(&mut self, by: isize) {
        let tab = self.active();
        let spots = spots(tab);
        if spots.is_empty() {
            return;
        }
        let key = tab.session.selected_revision.view_key();
        let current = tab
            .notes
            .step
            .as_ref()
            .filter(|(at, _)| *at == key)
            .map(|(_, spot)| *spot);
        let index = match current {
            None if by >= 0 => 0,
            None => spots.len() - 1,
            Some(spot) => {
                let after = spots.iter().position(|(at, _)| *at > spot);
                let at = spots.iter().position(|(at, _)| *at == spot);
                match (by >= 0, at) {
                    (true, Some(at)) => (at + 1) % spots.len(),
                    (true, None) => after.unwrap_or(0),
                    (false, Some(at)) => (at + spots.len() - 1) % spots.len(),
                    (false, None) => after
                        .unwrap_or(spots.len())
                        .checked_sub(1)
                        .unwrap_or(spots.len() - 1),
                }
            }
        };
        let spot = spots[index].0;
        let tab = self.active_mut();
        tab.notes.step = Some((key, spot));
        let token = tab.notes.reveal.map_or(1, |(_, token)| token + 1);
        tab.notes.reveal = Some((spot, token));
    }

    fn open_note(&mut self, id: &str, cx: &mut Cx) {
        let Some(note) = self.active().notes.get(id).cloned() else {
            return;
        };
        let Some(now) = note.located.now.clone() else {
            self.push_info_toast(
                "Its change was abandoned",
                "Archived notes have no diff to show them in.",
            );
            return;
        };
        let tab = self.active_mut();
        tab.notes.expanded.insert(note.thread.id.clone());
        tab.notes.hide_resolved &= note.thread.resolved.is_none();
        tab.notes.pending_open = Some(note.thread.id.clone());
        if tab.main_view != MainView::Diff {
            self.perform(Action::SetMainView(MainView::Diff), cx);
        }
        let selection = if self
            .active()
            .session
            .commits
            .working_copy_index()
            .is_some_and(|index| {
                self.active().session.commits.row(index).commit_id() == now.commit_id
            }) {
            diffui_core::RevisionSelection::WorkingCopy
        } else {
            diffui_core::RevisionSelection::Commit(now.commit_id)
        };
        self.jump_to_selection(selection);
        if let Some(tab) = self.active_tab_id() {
            self.settle_pending_open(tab);
        }
    }

    /// Scroll to the thread [`NotesMsg::Open`] asked for, once its diff is up.
    fn settle_pending_open(&mut self, tab: TabId) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(id) = state.notes.pending_open.clone() else {
            return;
        };
        let Some(note) = state.notes.get(&id).cloned() else {
            state.notes.pending_open = None;
            return;
        };
        if shown_change(state) != Some(note.thread.change_id.as_str())
            || state.session.document.files.is_empty() && note.thread.anchor.is_some()
        {
            return;
        }
        let Some(spot) = spot_of(state, &note) else {
            return;
        };
        state.notes.pending_open = None;
        let token = state.notes.reveal.map_or(1, |(_, token)| token + 1);
        state.notes.reveal = Some((spot, token));
        state.notes.step = Some((state.session.selected_revision.view_key(), spot));
    }
}

/// Watch `store` and tell the app when it changes. `None` when the platform
/// won't watch; the notes then refresh with the graph.
fn watch(store: &Store, tab: TabId, cx: &mut Cx) -> Option<Watch> {
    use notify::Watcher as _;
    let dir = store.threads_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let (send, mut receive) = futures::channel::mpsc::unbounded::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = send.unbounded_send(());
        }
    })
    .ok()?;
    watcher
        .watch(&dir, notify::RecursiveMode::NonRecursive)
        .ok()?;
    let task = cx.spawn(async move |this, cx| {
        while receive.next().await.is_some() {
            // A burst of events is one change.
            while receive.try_recv().is_ok() {}
            let updated = this.update(cx, |this, cx| {
                this.act(cx, |this, _| this.on_notes_changed(tab));
            });
            if updated.is_err() {
                return;
            }
        }
    });
    Some(Watch {
        _watcher: watcher,
        _task: task,
    })
}

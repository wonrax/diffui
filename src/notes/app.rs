//! The app's side of review notes: each jj tab's threads and where they sit,
//! kept current from the store (watched) and the repository (re-located when
//! the graph moves), and what the user does to them.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use diffui_core::{DiffFileStatus, DiffLineKind, hunk_conflict_region};
use futures::StreamExt as _;

use super::locate::{Located, Place, locate};
use super::store::{self, Role, Side, Source, Store, Thread};
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
    /// Open the field for a new note of the user's own.
    StartNote(NoteTarget),
    /// Copy these threads as Markdown.
    Copy(Vec<String>),
    /// The rows a drag from the "+" covers so far; `None` when it ends.
    Picking(Option<(LineAt, LineAt)>),
    /// The pointer is on this thread's card, or left it.
    Hover(String, bool),
    /// Copy the open notes on the change on screen, or all of them.
    CopyChange {
        resolved: bool,
    },
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
    /// Show this thread's change on the Notes view's right, scrolled to it.
    SelectThread(String),
    /// Show this change's threads on the Notes view's right, from the top.
    SelectChange(String),
    /// The stack scrolled to its `n`th thread: the list follows.
    StackScrolled(usize),
    /// Move to the next (`1`) or previous (`-1`) thread of the stack.
    StackStep(isize),
    /// Stack every thread of the shown change, or only the filter's.
    AllInChange(bool),
    /// The filter menu's pick: a [`Filter::name`], `author:NAME` or
    /// `everyone`.
    FilterNamed(String),
    /// Open the Notes view's filter menu.
    FilterMenu,
    Select(String),
    SelectAll(bool),
}

/// Which notes the Notes view lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Filter {
    /// Every open note on a live change.
    Open,
    /// The agent wrote last: yours to read.
    #[default]
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
    /// The ones the Notes view's head shows; the rest are in its menu.
    pub(crate) const SHOWN: [Filter; 3] = [
        Filter::WaitingOnYou,
        Filter::WaitingOnAgent,
        Filter::Resolved,
    ];

    /// The name `notes.filter` takes for it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Filter::Open => "open",
            Filter::WaitingOnYou => "waiting-on-you",
            Filter::WaitingOnAgent => "waiting-on-agent",
            Filter::Resolved => "resolved",
            Filter::Outdated => "outdated",
            Filter::Archived => "archived",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        [
            Filter::Open,
            Filter::WaitingOnYou,
            Filter::WaitingOnAgent,
            Filter::Resolved,
            Filter::Outdated,
            Filter::Archived,
        ]
        .into_iter()
        .find(|filter| filter.name() == name)
    }

    /// Its label in the Notes view's head, short enough not to need cutting
    /// at the pane's usual widths.
    pub(crate) fn short_label(self) -> &'static str {
        match self {
            Filter::WaitingOnYou => "For you",
            Filter::WaitingOnAgent => "For agent",
            other => other.label(),
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Filter::Open => "All open",
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

/// What's being written in the reply field: the
/// [`crate::modes::TabMode::NoteReply`] mode.
#[derive(Debug, Clone)]
pub(crate) struct NoteReply {
    pub(crate) to: ReplyTo,
    pub(crate) draft: String,
}

#[derive(Debug, Clone)]
pub(crate) enum ReplyTo {
    /// A reply under this thread.
    Thread(String),
    /// A note the user starts.
    New(NewNote),
}

impl NoteReply {
    /// The thread it answers, when it answers one.
    pub(crate) fn thread(&self) -> Option<&str> {
        match &self.to {
            ReplyTo::Thread(id) => Some(id),
            ReplyTo::New(_) => None,
        }
    }
}

/// A note the user is starting: on what, and where its field shows.
#[derive(Debug, Clone)]
pub(crate) struct NewNote {
    pub(crate) change: diffui_core::notes::ChangeNow,
    pub(crate) anchor: Option<super::store::Anchor>,
    pub(crate) spot: DocSpot,
    /// The rows of the diff it's on, first and last, to extend from.
    rows: Option<(LineAt, LineAt)>,
}

/// Where the user starts a note.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NoteTarget {
    /// The line under a gutter press (shift: from the line already picked).
    Line(crate::diff_view::LinePress),
    File(String),
    Change,
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
    /// The thread shown on the right.
    pub(crate) thread: Option<String>,
    /// The change the right side shows, all its threads stacked.
    pub(crate) shown: Option<String>,
    /// Every thread of the shown change, the filter set aside.
    pub(crate) all_in_change: bool,
    /// The stack, in document order: one diff "file" per thread.
    pub(crate) stack: Vec<StackItem>,
    /// Bumped when the stack is rebuilt or its excerpts land.
    pub(crate) stack_version: u64,
    /// Where the stack scrolls to, and the token that asks.
    pub(crate) reveal: Option<(DocSpot, u64)>,
    /// A reveal was just asked for: the stack's next scroll report is its
    /// answer (or, when it couldn't scroll there, wherever it stayed), not
    /// the user moving.
    pub(crate) revealing: bool,
}

/// One thread in the Notes view's stack: its excerpt as diff rows, and
/// what its file header says.
#[derive(Debug, Clone)]
pub(crate) struct StackItem {
    pub(crate) id: String,
    /// The path, or "Whole change".
    pub(crate) title: String,
    pub(crate) status: diffui_core::DiffFileStatus,
    pub(crate) additions: usize,
    pub(crate) deletions: usize,
    /// The excerpt; empty for a note without lines.
    pub(crate) hunks: Vec<diffui_core::DiffHunkView>,
    /// The file's real diff has replaced the anchored lines.
    pub(crate) loaded: bool,
    /// The excerpt's rows the thread is on, first and last: the rest is
    /// context.
    pub(crate) anchored: Option<(usize, usize)>,
}

/// The store watch: dropped with the tab.
struct Watch {
    _watcher: notify::RecommendedWatcher,
    _task: gpui_kit::Task<()>,
}

/// One tab's notes.
#[derive(Default)]
pub(crate) struct TabNotes {
    /// Where the threads live: the repository's store on disk.
    pub(crate) source: Option<std::sync::Arc<dyn Source>>,
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
    /// The rows a drag from the "+" covers, while it goes on.
    pub(crate) picking: Option<(LineAt, LineAt)>,
    /// The thread whose card the pointer is on.
    pub(crate) hovered: Option<String>,
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
            .filter(|note| note.counts() && note.thread.change_id() == change_id)
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
    if shown_change(tab)? != note.thread.change_id() || note.located.archived() {
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

/// Whether `reply`'s field is on screen: its thread, or the change a new
/// note is on, in the diff.
pub(crate) fn reply_on_screen(tab: &TabState, reply: &NoteReply) -> bool {
    if tab.main_view == MainView::Notes {
        return reply
            .thread()
            .is_some_and(|id| tab.notes.view.stack.iter().any(|item| item.id == id));
    }
    if tab.main_view != MainView::Diff {
        return false;
    }
    match &reply.to {
        ReplyTo::Thread(id) => diff_notes(tab)
            .iter()
            .any(|(_, note)| note.thread.id == *id),
        ReplyTo::New(note) => shown_change(tab) == Some(note.change.change_id.as_str()),
    }
}

/// The rows of the diff on screen `note`'s lines are on, first and last.
fn rows_of(tab: &TabState, note: &Note) -> Option<(LineAt, LineAt)> {
    let DocSpot::Line(last) = spot_of(tab, note)? else {
        return None;
    };
    let (Place::Lines { start, .. }, Some(lines)) = (
        &note.located.place,
        note.thread
            .anchor
            .as_ref()
            .and_then(|anchor| anchor.lines.as_ref()),
    ) else {
        return None;
    };
    let hunk = &tab.session.document.files[last.file].hunks[last.hunk];
    let first = hunk.lines.iter().position(|row| {
        let number = match lines.side {
            Side::New if row.kind != DiffLineKind::Deletion => row.new_line,
            Side::Old if row.kind != DiffLineKind::Addition => row.old_line,
            _ => None,
        };
        number == Some(*start)
    })?;
    Some((
        LineAt {
            line: first,
            ..last
        },
        last,
    ))
}

/// The rows the diff washes in the accent: the range a drag from the "+"
/// covers and the note being written (strong); the lines of the threads
/// opened or under the pointer (soft). Folded threads wash nothing.
pub(crate) fn highlights(tab: &TabState) -> Vec<crate::diff_view::Highlight> {
    use crate::diff_view::Highlight;
    let mut out = Vec::new();
    let composing = tab.note_reply().and_then(|reply| match &reply.to {
        ReplyTo::New(note) => note.rows,
        ReplyTo::Thread(_) => None,
    });
    for (from, to) in tab.notes.picking.into_iter().chain(composing) {
        out.push(Highlight {
            from,
            to,
            strong: true,
        });
    }
    let replying = tab.note_reply().and_then(NoteReply::thread);
    for (_, note) in diff_notes(tab) {
        let id = note.thread.id.as_str();
        let shown = tab.notes.expanded.contains(id)
            || tab.notes.hovered.as_deref() == Some(id)
            || replying == Some(id);
        if let Some((from, to)) = rows_of(tab, note).filter(|_| shown) {
            out.push(Highlight {
                from,
                to,
                strong: false,
            });
        }
    }
    out
}

/// What a spot of the diff is, for next/previous and the overview strip.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SpotKind {
    Note {
        id: String,
        label: String,
    },
    /// A pull request's review thread, read from its forge.
    Review {
        label: String,
    },
    Conflict {
        label: String,
    },
}

/// `author: first line` of a thread, for a mark's tooltip.
fn spot_label(thread: &Thread) -> String {
    let first = thread
        .messages
        .first()
        .map_or("", |message| message.body.lines().next().unwrap_or(""));
    let author = thread.messages.first().map_or("", |m| m.author.as_str());
    format!("{author}: {first}")
}

/// The diff's important spots in document order: open notes, open review
/// threads of the pull request on screen, and conflicts.
pub(crate) fn spots(ui: &Diffui) -> Vec<(DocSpot, SpotKind)> {
    let tab = ui.active();
    let mut spots: Vec<(DocSpot, SpotKind)> = diff_notes(tab)
        .into_iter()
        .filter(|(_, note)| note.thread.resolved.is_none())
        .map(|(spot, note)| {
            (
                spot,
                SpotKind::Note {
                    id: note.thread.id.clone(),
                    label: spot_label(&note.thread),
                },
            )
        })
        .collect();
    spots.extend(
        super::forge::placed(ui)
            .into_iter()
            .filter(|(_, note, _)| note.thread.resolved.is_none())
            .map(|(spot, note, _)| {
                (
                    spot,
                    SpotKind::Review {
                        label: spot_label(&note.thread),
                    },
                )
            }),
    );
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
                && note.thread.change_id() == change
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
    Create(Thread),
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
            if state.notes.source.is_none() {
                let Some(store) =
                    diffui_core::notes::repo_dir(&root).and_then(|dir| Store::for_repo(&dir))
                else {
                    continue;
                };
                let source: std::sync::Arc<dyn Source> = std::sync::Arc::new(store);
                state.notes.watch = watch(source.as_ref(), tab, cx);
                state.notes.source = Some(source);
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
        let Some(store) = state.notes.source.clone() else {
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
            move |this, loaded, cx| {
                let Some(state) = this.tab_mut(tab) else {
                    return;
                };
                let mut restack = false;
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
                        restack = state.notes.view.shown.is_some();
                        state.notes.located_at = operation;
                    }
                    Some((_, Err(error))) => {
                        tracing::warn!("couldn't locate review notes: {error}");
                        state.notes.located_at = operation;
                    }
                    None => {}
                }
                if restack && this.is_active(tab) {
                    this.restack(cx);
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
        let Some(store) = self.active().notes.source.clone() else {
            return;
        };
        let (send, receive) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("notes-write".to_owned())
            .spawn(move || {
                let result = match write {
                    Write::Reply { id, body } => store
                        .update(&id, &mut |thread| {
                            store::reply(thread, "you", Role::User, &body);
                            true
                        })
                        .map(|_| ()),
                    Write::Resolve(ids) => ids.iter().try_for_each(|id| {
                        store
                            .update(id, &mut |thread| {
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
                            .update(id, &mut |thread| thread.resolved.take().is_some())
                            .map(|_| ())
                    }),
                    Write::Delete(ids) => store.delete(&ids).map(|_| ()),
                    Write::Create(thread) => store.create(thread).map(|_| ()),
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
                    to: ReplyTo::Thread(id),
                    draft: String::new(),
                }));
                crate::input::focus(super::inline::REPLY_FIELD, cx);
            }
            NotesMsg::StartNote(target) => self.start_note(target, cx),
            NotesMsg::Picking(range) => self.active_mut().notes.picking = range,
            NotesMsg::Hover(id, on) => {
                let hovered = &mut self.active_mut().notes.hovered;
                if on {
                    *hovered = Some(id);
                } else if hovered.as_deref() == Some(id.as_str()) {
                    *hovered = None;
                }
            }
            NotesMsg::Copy(ids) => self.copy_notes(&ids, cx),
            NotesMsg::CopyChange { resolved } => {
                let tab = self.active();
                let Some(change) = shown_change(tab) else {
                    return;
                };
                let ids: Vec<String> = tab
                    .notes
                    .notes
                    .iter()
                    .filter(|note| note.thread.change_id() == change)
                    .filter(|note| resolved || note.thread.resolved.is_none())
                    .map(|note| note.thread.id.clone())
                    .collect();
                self.copy_notes(&ids, cx);
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
                let write = match reply.to {
                    ReplyTo::Thread(id) => Write::Reply { id, body },
                    ReplyTo::New(note) => Write::Create(Thread::on_change(
                        &note.change,
                        note.anchor,
                        store::Message {
                            author: "you".to_owned(),
                            role: Role::User,
                            at: store::now(),
                            body,
                        },
                    )),
                };
                self.write_notes(write, cx);
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
                self.restack(cx);
            }
            NotesMsg::SelectThread(id) => {
                let Some(change) = self
                    .active()
                    .notes
                    .get(&id)
                    .map(|note| note.thread.change_id().to_owned())
                else {
                    return;
                };
                let view = &mut self.active_mut().notes.view;
                if view.shown.as_deref() != Some(change.as_str()) {
                    view.shown = Some(change);
                    view.all_in_change = false;
                    self.restack(cx);
                }
                self.focus_thread(&id, cx);
            }
            NotesMsg::SelectChange(change) => {
                let view = &mut self.active_mut().notes.view;
                if view.shown.as_deref() != Some(change.as_str()) {
                    view.shown = Some(change);
                    view.all_in_change = false;
                    self.restack(cx);
                }
                if let Some(first) = self
                    .active()
                    .notes
                    .view
                    .stack
                    .first()
                    .map(|item| item.id.clone())
                {
                    self.focus_thread(&first, cx);
                }
            }
            NotesMsg::StackScrolled(index) => {
                let view = &mut self.active_mut().notes.view;
                if std::mem::take(&mut view.revealing) {
                    return;
                }
                if let Some(item) = view.stack.get(index) {
                    view.thread = Some(item.id.clone());
                }
            }
            NotesMsg::StackStep(by) => {
                let view = &self.active().notes.view;
                let at = view
                    .thread
                    .as_deref()
                    .and_then(|id| view.stack.iter().position(|item| item.id == id));
                let last = view.stack.len().saturating_sub(1);
                let next = match at {
                    None => 0,
                    Some(at) => (at as isize + by).clamp(0, last as isize) as usize,
                };
                if let Some(id) = view.stack.get(next).map(|item| item.id.clone()) {
                    self.focus_thread(&id, cx);
                }
            }
            NotesMsg::AllInChange(all) => {
                self.active_mut().notes.view.all_in_change = all;
                self.restack(cx);
            }
            NotesMsg::FilterNamed(name) => {
                let view = &mut self.active_mut().notes.view;
                if let Some(author) = name.strip_prefix("author:") {
                    view.author = Some(author.to_owned());
                } else if name == "everyone" {
                    view.author = None;
                } else if let Some(filter) = Filter::named(&name) {
                    view.filter = filter;
                }
                view.selected.clear();
                view.confirm_delete = None;
                self.restack(cx);
            }
            NotesMsg::FilterMenu => self.open_filter_menu(cx),
            NotesMsg::AllChanges => {
                let view = &mut self.active_mut().notes.view;
                view.change = None;
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

    /// Scroll the stack to thread `id` and make it the list's selection.
    fn focus_thread(&mut self, id: &str, cx: &mut Cx) {
        let view = &mut self.active_mut().notes.view;
        view.thread = Some(id.to_owned());
        if let Some(index) = view.stack.iter().position(|item| item.id == id) {
            let token = view.reveal.map_or(1, |(_, token)| token + 1);
            view.reveal = Some((DocSpot::File(index), token));
            view.revealing = true;
            // What the stack reports while it moves there is the reveal's
            // doing; after a beat, its reports are the user's scrolling.
            let Some(tab) = self.active_tab_id() else {
                return;
            };
            Self::after(std::time::Duration::from_millis(300), cx, move |this, _| {
                if let Some(state) = this.tab_mut(tab) {
                    state.notes.view.revealing = false;
                }
            });
        }
    }

    /// Rebuild the Notes view's stack for its change: the threads at once,
    /// each with its anchored lines, then, read off the UI thread, each
    /// file's diff around them, highlighted as the diff pane highlights it.
    pub(crate) fn restack(&mut self, cx: &mut Cx) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        let state = self.active();
        let notes: Vec<Note> = stack_notes(state).into_iter().cloned().collect();
        let stack: Vec<StackItem> = notes.iter().map(fallback_item).collect();
        let root = state
            .repository
            .as_ref()
            .map(|repository| repository.root.clone());
        let view = &mut self.active_mut().notes.view;
        view.stack = stack;
        view.stack_version = view.stack_version.wrapping_add(1);
        if view
            .thread
            .as_deref()
            .is_some_and(|id| !view.stack.iter().any(|item| item.id == id))
        {
            view.thread = view.stack.first().map(|item| item.id.clone());
        }
        self.document_version = self.document_version.wrapping_add(1);
        let Some(root) = root else {
            return;
        };
        // What to read: per thread with lines on a live change, its commit,
        // path, side and lines.
        let wanted: Vec<(String, String, String, Side, usize, usize)> = notes
            .iter()
            .filter_map(|note| {
                let now = note.located.now.as_ref()?;
                let anchor = note.thread.anchor.as_ref()?;
                let side = anchor.lines.as_ref()?.side;
                let Place::Lines { start, end } = note.located.place else {
                    return None;
                };
                Some((
                    note.thread.id.clone(),
                    now.commit_id.clone(),
                    anchor.path.clone(),
                    side,
                    start,
                    end,
                ))
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        let (send, receive) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("notes-stack".to_owned())
            .spawn(move || {
                let Ok(repo) = diffui_core::notes::NotesRepo::open(&root, false) else {
                    let _ = send.send(Vec::new());
                    return;
                };
                let mut files: std::collections::HashMap<
                    (String, String),
                    Option<diffui_core::DiffFile>,
                > = Default::default();
                let mut read = Vec::new();
                for (id, commit, path, side, start, end) in wanted {
                    let file = files
                        .entry((commit.clone(), path.clone()))
                        .or_insert_with(|| {
                            // Highlighted as the diff pane does: the
                            // language from the path, spans from each
                            // side's full source when it reads.
                            let mut file = repo.file_diff(&commit, &path)?;
                            let old_path = file.old_path.clone().unwrap_or_else(|| path.clone());
                            let old = repo.read(&commit, &old_path, Side::Old);
                            let new = repo.read(&commit, &path, Side::New);
                            diffui_core::syntax::apply_syntax_highlighting_with_sources(
                                &mut file,
                                old.as_deref(),
                                new.as_deref(),
                            );
                            Some(file)
                        })
                        .clone();
                    if let Some(file) = file
                        && let Some((hunk, anchored)) = excerpt_hunk(&file, side, start, end)
                    {
                        read.push((
                            id,
                            file.status,
                            file.additions,
                            file.deletions,
                            hunk,
                            anchored,
                        ));
                    }
                }
                let _ = send.send(read);
            });
        if spawned.is_err() {
            return;
        }
        Self::run_async(
            async move { receive.await.ok().unwrap_or_default() },
            cx,
            move |this, read, _| {
                let Some(state) = this.tab_mut(tab) else {
                    return;
                };
                let view = &mut state.notes.view;
                for (id, status, additions, deletions, hunk, anchored) in read {
                    if let Some(item) = view.stack.iter_mut().find(|item| item.id == id) {
                        item.status = status;
                        item.additions = additions;
                        item.deletions = deletions;
                        item.hunks = vec![hunk];
                        item.anchored = Some(anchored);
                        item.loaded = true;
                    }
                }
                view.stack_version = view.stack_version.wrapping_add(1);
                this.document_version = this.document_version.wrapping_add(1);
            },
        );
    }

    /// The Notes view's filter menu: the filters its head leaves out, and the
    /// authors.
    fn open_filter_menu(&mut self, cx: &mut Cx) {
        use crate::commands::CommandArg;
        use crate::menu::MenuEntry;
        let row = |this: &Self, label: String, name: String| {
            this.menu_row(label, "notes.filter", CommandArg::Text(name))
        };
        let mut tree = vec![
            row(self, "All open".to_owned(), Filter::Open.name().to_owned()),
            row(
                self,
                "Outdated".to_owned(),
                Filter::Outdated.name().to_owned(),
            ),
            row(
                self,
                "Archived".to_owned(),
                Filter::Archived.name().to_owned(),
            ),
        ];
        let mut authors: Vec<String> = self
            .active()
            .notes
            .notes
            .iter()
            .flat_map(|note| note.thread.agents().map(str::to_owned))
            .collect();
        authors.sort();
        authors.dedup();
        if !authors.is_empty() {
            tree.push(MenuEntry::Separator);
            tree.push(row(self, "Everyone".to_owned(), "everyone".to_owned()));
            for author in authors {
                tree.push(row(
                    self,
                    format!("By {author}"),
                    format!("author:{author}"),
                ));
            }
        }
        #[cfg(target_os = "macos")]
        Self::popup_tree(&tree, None, cx);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cx;
            let at = crate::geometry::Point::new(self.left_pane_width() - 24.0, 72.0);
            self.push_mode(crate::modes::Mode::Menu(crate::menu::OverlayMenu::new(
                tree,
                crate::menu::AnchorSpec::At(at),
                None,
            )));
        }
    }

    /// Copy the threads `ids` names as Markdown (see [`super::export`]).
    fn copy_notes(&mut self, ids: &[String], cx: &mut Cx) {
        let tab = self.active();
        let mut notes: Vec<&Note> = ids.iter().filter_map(|id| tab.notes.get(id)).collect();
        if notes.is_empty() {
            self.push_info_toast("No notes to copy", "");
            return;
        }
        notes.sort_by_key(|note| std::cmp::Reverse(note.thread.activity()));
        let pairs: Vec<(&Thread, &Located)> = notes
            .iter()
            .map(|note| (&note.thread, &note.located))
            .collect();
        let text = super::export::markdown(&pairs);
        let count = notes.len();
        crate::write_clipboard(text, cx);
        let noun = if count == 1 { "note" } else { "notes" };
        self.push_info_toast(format!("Copied {count} {noun} as Markdown"), "");
    }

    /// Open the field for a new note of the user's on `target` of the change
    /// on screen.
    fn start_note(&mut self, target: NoteTarget, cx: &mut Cx) {
        let tab = self.active();
        let Some(details) = tab.session.revision_details.as_ref() else {
            return;
        };
        let Some(change_id) = shown_change(tab) else {
            return;
        };
        let change = diffui_core::notes::ChangeNow {
            change_id: change_id.to_owned(),
            commit_id: details.commit_id.clone(),
            title: details.description.lines().next().unwrap_or("").to_owned(),
        };
        let files = &tab.session.document.files;
        let earlier = tab.note_reply().and_then(|reply| match &reply.to {
            ReplyTo::New(note) => note.rows,
            ReplyTo::Thread(_) => None,
        });
        let new = match target {
            NoteTarget::Change => NewNote {
                change,
                anchor: None,
                spot: DocSpot::Top,
                rows: None,
            },
            NoteTarget::File(path) => {
                let Some(file) = files.iter().position(|file| file.path == path) else {
                    return;
                };
                NewNote {
                    change,
                    anchor: Some(super::store::Anchor { path, lines: None }),
                    spot: DocSpot::File(file),
                    rows: None,
                }
            }
            NoteTarget::Line(press) => {
                let at = press.line;
                let (from, to) = match earlier {
                    Some((first, last))
                        if press.extend && (first.file, first.hunk) == (at.file, at.hunk) =>
                    {
                        (first.min(at).min(last), last.max(at).max(first))
                    }
                    _ => (at, at),
                };
                let Some(lines) = picked_lines(tab, from, to) else {
                    return;
                };
                NewNote {
                    change,
                    anchor: Some(super::store::Anchor {
                        path: files[at.file].path.clone(),
                        lines: Some(lines),
                    }),
                    spot: DocSpot::Line(to),
                    rows: Some((from, to)),
                }
            }
        };
        // Its field shows under the spot: in view.
        self.reveal_spot(new.spot);
        let draft = self
            .active()
            .note_reply()
            .filter(|reply| matches!(reply.to, ReplyTo::New(_)))
            .map(|reply| reply.draft.clone())
            .unwrap_or_default();
        self.active_mut()
            .push_mode(crate::modes::TabMode::NoteReply(NoteReply {
                to: ReplyTo::New(new),
                draft,
            }));
        crate::input::focus(super::inline::REPLY_FIELD, cx);
    }

    fn step_spot(&mut self, by: isize) {
        let spots = spots(self);
        let tab = self.active();
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
        if shown_change(state) != Some(note.thread.change_id())
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

/// The lines of one side from row `from` to row `to` of a hunk, as an
/// anchor: the side of `to` (a removed line's old side, else the new), and
/// that side's rows between.
fn picked_lines(tab: &TabState, from: LineAt, to: LineAt) -> Option<super::store::Lines> {
    let hunk = tab
        .session
        .document
        .files
        .get(to.file)?
        .hunks
        .get(to.hunk)?;
    let side = match hunk.lines.get(to.line)?.kind {
        DiffLineKind::Deletion => Side::Old,
        _ => Side::New,
    };
    let rows: Vec<&diffui_core::DiffLine> = hunk.lines[from.line..=to.line]
        .iter()
        .filter(|line| match side {
            Side::Old => line.kind != DiffLineKind::Addition && line.old_line.is_some(),
            Side::New => line.kind != DiffLineKind::Deletion && line.new_line.is_some(),
        })
        .collect();
    let first = rows.first()?;
    let start = match side {
        Side::Old => first.old_line?,
        Side::New => first.new_line?,
    };
    Some(super::store::Lines {
        side,
        start,
        text: rows.iter().map(|line| line.content.clone()).collect(),
    })
}

/// The Notes view's stack for its change: the filter's threads (or all of
/// them), the change's own notes first, then each file's by path and line.
pub(crate) fn stack_notes(tab: &TabState) -> Vec<&Note> {
    let view = &tab.notes.view;
    let Some(change) = view.shown.as_deref() else {
        return Vec::new();
    };
    let mut notes: Vec<&Note> = tab
        .notes
        .notes
        .iter()
        .filter(|note| note.thread.change_id() == change)
        .filter(|note| {
            view.all_in_change
                || (view.filter.admits(note)
                    && view
                        .author
                        .as_deref()
                        .is_none_or(|author| note.thread.agents().any(|agent| agent == author)))
        })
        .collect();
    notes.sort_by_key(|note| document_order(note));
    notes
}

/// Where a thread sits in its change: the change's own notes first, then by
/// path and line, ties by when they were started.
pub(crate) fn document_order(note: &Note) -> (Option<String>, usize, u64) {
    let line = match (&note.located.place, &note.thread.anchor) {
        (Place::Lines { start, .. }, _) => *start,
        (_, Some(anchor)) => anchor.lines.as_ref().map_or(0, |lines| lines.start),
        _ => 0,
    };
    (
        note.thread
            .anchor
            .as_ref()
            .map(|anchor| anchor.path.clone()),
        line,
        note.thread.messages.first().map_or(0, |message| message.at),
    )
}

/// A thread's stack item before its file's diff is read: its anchored lines
/// as context rows with their numbers on both sides.
fn fallback_item(note: &Note) -> StackItem {
    let anchor = note.thread.anchor.as_ref();
    let hunks = anchor
        .and_then(|anchor| anchor.lines.as_ref())
        .map(|lines| {
            let start = match note.located.place {
                Place::Lines { start, .. } => start,
                _ => lines.start,
            };
            vec![diffui_core::DiffHunkView {
                header: String::new(),
                lines: lines
                    .text
                    .iter()
                    .enumerate()
                    .map(|(offset, text)| diffui_core::DiffLine {
                        kind: DiffLineKind::Context,
                        old_line: Some(start + offset),
                        new_line: Some(start + offset),
                        content: text.clone(),
                        syntax: Vec::new(),
                        emphasis: Vec::new(),
                    })
                    .collect(),
            }]
        })
        .unwrap_or_default();
    // Only the anchored lines show until the diff reads: all of them.
    let anchored = hunks
        .first()
        .and_then(|hunk: &diffui_core::DiffHunkView| hunk.lines.len().checked_sub(1))
        .map(|last| (0, last));
    StackItem {
        id: note.thread.id.clone(),
        title: anchor.map_or_else(|| "Whole change".to_owned(), |anchor| anchor.path.clone()),
        status: DiffFileStatus::Modified,
        additions: 0,
        deletions: 0,
        hunks,
        loaded: false,
        anchored,
    }
}

/// The rows of `file`'s diff around lines `start..=end` of `side`: the hunk
/// holding them, cut to three rows of context either side, and which of the
/// excerpt's rows are those lines.
pub(crate) fn excerpt_hunk(
    file: &diffui_core::DiffFile,
    side: Side,
    start: usize,
    end: usize,
) -> Option<(diffui_core::DiffHunkView, (usize, usize))> {
    const CONTEXT: usize = 3;
    let number = |row: &diffui_core::DiffLine| match side {
        Side::New if row.kind != DiffLineKind::Deletion => row.new_line,
        Side::Old if row.kind != DiffLineKind::Addition => row.old_line,
        _ => None,
    };
    file.hunks.iter().find_map(|hunk| {
        let first = hunk
            .lines
            .iter()
            .position(|row| number(row) == Some(start))?;
        let last = hunk
            .lines
            .iter()
            .rposition(|row| number(row) == Some(end))?;
        let anchored_first = first;
        // A replaced line shows with what it replaced: the removed run in
        // front of the anchored added lines comes along, unmarked.
        let mut first = first;
        while first > 0 && hunk.lines[first - 1].kind == DiffLineKind::Deletion && side == Side::New
        {
            first -= 1;
        }
        let from = first.saturating_sub(CONTEXT);
        let to = (last + CONTEXT + 1).min(hunk.lines.len());
        let lines: Vec<diffui_core::DiffLine> = hunk.lines[from..to].to_vec();
        let range = |pick: fn(&diffui_core::DiffLine) -> Option<usize>| {
            let numbers: Vec<usize> = lines.iter().filter_map(pick).collect();
            match (numbers.first(), numbers.last()) {
                (Some(&a), Some(&b)) => a..b + 1,
                _ => 0..0,
            }
        };
        Some((
            diffui_core::DiffHunkView {
                header: diffui_core::format_hunk_header(
                    &range(|row| row.old_line),
                    &range(|row| row.new_line),
                ),
                lines,
            },
            (anchored_first - from, last - from),
        ))
    })
}

/// Watch `store` and tell the app when it changes. `None` when the platform
/// won't watch; the notes then refresh with the graph.
fn watch(source: &dyn Source, tab: TabId, cx: &mut Cx) -> Option<Watch> {
    use notify::Watcher as _;
    let dir = source.watched()?;
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

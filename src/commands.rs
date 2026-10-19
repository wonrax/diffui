//! The command registry: every addressable thing diffui can do, as data.
//!
//! One row per command, carrying a stable string id (`revision.rebase.start`),
//! the label and hint the palette and the menus show, the default chords for
//! each platform, the [`Context`]s it is valid in, an `enabled` predicate, and
//! the [`Action`] it performs. Everything that used to name an intent by
//! constructing a variant now names it by id:
//!
//!   * the palette lists the registry, filtered by [`PaletteScope`] and
//!     `enabled`;
//!   * the menus reference commands by id and take their labels and chord
//!     hints from here;
//!   * the keyboard resolves a chord to an id through [`crate::keymap`].
//!
//! `enabled` is re-checked at *pick* time, not only when the surface was
//! built. That is what keeps a menu opened before a graph reload from acting
//! on a row that reload hid.

use diffui_core::{FetchTarget, RevisionSelection};

use crate::action::Action;
use crate::bookmarks::BookmarkAction;
use crate::compare::CompareAction;
use crate::history_panel::HistoryAction;
use crate::palette::{self, ResultRef};
use crate::revops::{self, RevopsAction};
use crate::theme::ThemePreference;
use crate::{DetailField, Diffui, MainView, mutations};

/// A command's stable identity. A string so it survives in a config file and
/// in the persisted palette recents without a hand-written mapping table.
pub(crate) type CommandId = &'static str;

/// Where a command's keys are live. The mode stack decides which context the
/// keyboard is in; a command only resolves in the contexts it lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Context {
    /// No overlay: the graph, the diff, the tabs.
    Base,
    Palette,
    Find,
    Confirm,
    Description,
    /// Target mode — picking a rebase/squash/merge destination.
    Draft,
    Menu,
    OpenRepo,
    ActivityPopover,
    /// The bookmark-name prompt.
    Prompt,
    /// The keyboard-shortcut cheat sheet.
    Shortcuts,
    /// Line-select mode in the diff pane.
    Lines,
    /// The split prompt's two description editors.
    Split,
}

impl Context {
    /// Every context, for iteration in tests and config parsing.
    pub(crate) const ALL: &'static [Context] = &[
        Context::Base,
        Context::Palette,
        Context::Find,
        Context::Confirm,
        Context::Description,
        Context::Draft,
        Context::Menu,
        Context::OpenRepo,
        Context::ActivityPopover,
        Context::Prompt,
        Context::Shortcuts,
        Context::Lines,
        Context::Split,
    ];

    /// The name this context goes by in `config.toml`'s `[keys.<name>]`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Context::Base => "base",
            Context::Palette => "palette",
            Context::Find => "find",
            Context::Confirm => "confirm",
            Context::Description => "description",
            Context::Draft => "draft",
            Context::Menu => "menu",
            Context::OpenRepo => "open-repo",
            Context::ActivityPopover => "activity",
            Context::Prompt => "prompt",
            Context::Shortcuts => "shortcuts",
            Context::Lines => "lines",
            Context::Split => "split",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        Context::ALL
            .iter()
            .copied()
            .find(|context| context.name() == name)
    }
}

/// Whether — and where — a command is offered in the command palette. The
/// palette is a curated surface, not a dump of the registry: menu-only verbs
/// (`Abandon`, `Track`) would drown the useful rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteScope {
    /// Never listed; reached by chord, menu, or button only.
    Hidden,
    /// The top-level command list (`>` mode and the mixed search).
    Root,
    /// The actions column pushed from a revision / bookmark / `@` result.
    Revision,
    /// The actions column pushed from a file result.
    File,
    /// A revision verb: listed at the top level acting on the selected
    /// revision (the row names it), and in a revision result's actions
    /// column acting on that result.
    RevisionVerb,
}

/// The payload a producer hands a command: what the menu row was built for,
/// what the palette column targets. `None` means "read it from the current
/// selection", which is how the same command serves a chord and a menu row.
#[derive(Debug, Clone, Default)]
pub(crate) enum CommandArg {
    #[default]
    None,
    Revision(RevisionSelection),
    /// A batch: the sidebar's marked rows.
    Revisions(Vec<RevisionSelection>),
    /// A revision plus an optional file inside it (the browse-source rows).
    Browse {
        revision: RevisionSelection,
        path: Option<String>,
    },
    Path(String),
    /// A ready-to-paste value.
    Text(String),
    /// A field of `revision` read from the repository on demand, with what to
    /// copy if the read fails.
    Detail {
        revision: RevisionSelection,
        field: DetailField,
        fallback: String,
    },
    Bookmark {
        name: String,
        remote: Option<String>,
        to: Option<RevisionSelection>,
    },
    Fetch(FetchTarget),
    Revset(String),
    /// A palette result row.
    Result(ResultRef),
    /// A revision to push under a generated bookmark, and where to
    /// (`None`: the default push remote).
    PushRevision {
        revision: RevisionSelection,
        remote: Option<String>,
    },
}

impl CommandArg {
    fn revision(&self) -> Option<&RevisionSelection> {
        match self {
            CommandArg::Revision(revision)
            | CommandArg::Browse { revision, .. }
            | CommandArg::Detail { revision, .. }
            | CommandArg::PushRevision { revision, .. } => Some(revision),
            _ => None,
        }
    }

    fn path(&self) -> Option<&str> {
        match self {
            CommandArg::Path(path) => Some(path),
            CommandArg::Browse { path, .. } => path.as_deref(),
            _ => None,
        }
    }
}

/// One registry row.
pub(crate) struct Command {
    pub(crate) id: CommandId,
    pub(crate) label: &'static str,
    pub(crate) hint: &'static str,
    pub(crate) contexts: &'static [Context],
    /// Default chords in [`crate::keymap`]'s syntax. `config.toml` overrides
    /// them by id.
    pub(crate) chords: &'static [&'static str],
    /// Chords that replace `chords` on macOS. Only needed where the platform
    /// reports a different key: ⌥Z composes to `Ω` there, so the base letter
    /// alone would never match.
    pub(crate) mac_chords: &'static [&'static str],
    pub(crate) enabled: fn(&Diffui, &CommandArg) -> bool,
    pub(crate) build: fn(&Diffui, CommandArg) -> Option<Action>,
    pub(crate) palette: PaletteScope,
}

impl Command {
    /// The chords this command carries by default on the running platform.
    pub(crate) fn default_chords(&self) -> &'static [&'static str] {
        if cfg!(target_os = "macos") && !self.mac_chords.is_empty() {
            self.mac_chords
        } else {
            self.chords
        }
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command").field("id", &self.id).finish()
    }
}

/// Look a command up by id. `None` for an id that no longer exists — a stale
/// keymap entry or a persisted palette recent, both of which are ignored
/// rather than fatal.
pub(crate) fn command(id: &str) -> Option<&'static Command> {
    REGISTRY.iter().find(|command| command.id == id)
}

/// The commands the palette offers in `scope`, in registry order, filtered by
/// `enabled` against the arg they would run with.
pub(crate) fn palette_commands(
    ui: &Diffui,
    scope: PaletteScope,
    arg: &CommandArg,
) -> Vec<&'static Command> {
    REGISTRY
        .iter()
        .filter(|command| {
            command.palette == scope
                || (command.palette == PaletteScope::RevisionVerb
                    && matches!(scope, PaletteScope::Root | PaletteScope::Revision))
        })
        .filter(|command| (command.enabled)(ui, arg))
        .collect()
}

impl Command {
    /// The label a palette row shows. Most commands use their menu label;
    /// the revision verbs are spelled for a menu that already says what they
    /// are (`Rebase ▸ Onto…`), which reads as nothing on its own in a list.
    pub(crate) fn palette_label(&self) -> &'static str {
        match self.id {
            "revision.edit" => "Edit revision",
            "revision.rebase.start" => "Rebase onto\u{2026}",
            "revision.rebase.descendants.start" => "Rebase with descendants onto\u{2026}",
            "revision.rebase.branch.start" => "Rebase whole branch onto\u{2026}",
            "revision.squash.start" => "Squash into\u{2026}",
            "revision.squash-into-parent" => "Squash into parent",
            "revision.abandon" => "Abandon\u{2026}",
            "repo.fetch" => "Fetch all remotes",
            "bookmark.track" => "Track remote bookmark",
            "bookmark.delete" => "Delete bookmark",
            _ => self.label,
        }
    }
}

// ── Shared predicates ───────────────────────────────────────────────────────

fn lines_action(message: crate::changes::LineMsg) -> Action {
    Action::Changes(crate::changes::ChangesMsg::Lines(message))
}

fn always(_: &Diffui, _: &CommandArg) -> bool {
    true
}

/// Whether the row a command names is still in the loaded graph. A menu built
/// before a reload can otherwise act on a commit the reload hid.
fn row_present(ui: &Diffui, selection: &RevisionSelection) -> bool {
    let commits = &ui.active().session.commits;
    match selection {
        RevisionSelection::WorkingCopy => commits.working_copy().is_some(),
        RevisionSelection::Commit(hex) => commits.find_by_commit_id(hex).is_some(),
    }
}

/// A mutation is offered when the backend supports one (jj only) and every
/// revision the command names is still on screen.
pub(crate) fn can_mutate(ui: &Diffui, arg: &CommandArg) -> bool {
    if !ui.active().session.capabilities.mutate {
        return false;
    }
    match arg {
        CommandArg::Revisions(targets) => {
            !targets.is_empty() && targets.iter().all(|target| row_present(ui, target))
        }
        CommandArg::Bookmark { to: Some(to), .. } => row_present(ui, to),
        _ => match arg.revision() {
            Some(revision) => row_present(ui, revision),
            None => true,
        },
    }
}

fn can_read_graph(ui: &Diffui, _: &CommandArg) -> bool {
    ui.active().session.capabilities.graph
}

fn can_fetch(ui: &Diffui, _: &CommandArg) -> bool {
    ui.active().session.capabilities.fetch
}

/// A local repository is open, so there is a tree to browse.
fn can_browse(ui: &Diffui, arg: &CommandArg) -> bool {
    ui.active().repository.is_some()
        && arg
            .revision()
            .is_none_or(|revision| row_present(ui, revision))
}

fn has_selected_file(ui: &Diffui, _: &CommandArg) -> bool {
    let tab = ui.active();
    tab.session.document.files.get(tab.selected_file).is_some()
}

fn has_remotes(ui: &Diffui) -> bool {
    !ui.active().session.bookmarks.remotes.is_empty()
}

/// The local bookmark a bookmark command names: a menu row's, or a palette
/// bookmark result's (`main`, `main*`, `main??` — never `main@origin`).
fn local_bookmark(ui: &Diffui, arg: &CommandArg) -> Option<String> {
    let name = match arg {
        CommandArg::Bookmark { name, .. } => name.as_str(),
        CommandArg::Result(ResultRef::Bookmark(label)) => {
            crate::bookmarks::local_bookmark_label(label)?
        }
        _ => return None,
    };
    let entry = ui.active().session.bookmarks.entry(name)?;
    (!entry.local_targets.is_empty()).then(|| name.to_owned())
}

/// `(bookmark, remote)` of a remote bookmark no one tracks yet: a menu row's
/// bookmark, or a palette `name@remote` result.
fn untracked_remote_bookmark(ui: &Diffui, arg: &CommandArg) -> Option<(String, String)> {
    let (name, remote) = match arg {
        CommandArg::Bookmark {
            name,
            remote: Some(remote),
            ..
        } => (name.as_str(), remote.as_str()),
        CommandArg::Result(ResultRef::Bookmark(label)) => {
            label.trim_end_matches("??").rsplit_once('@')?
        }
        _ => return None,
    };
    let entry = ui.active().session.bookmarks.entry(name)?;
    entry
        .remotes
        .iter()
        .any(|r| r.remote == remote && !r.tracked)
        .then(|| (name.to_owned(), remote.to_owned()))
}

/// `(bookmark, remote)` for a deletion push: the menu row's remote, else the
/// first remote tracking the bookmark.
fn deletion_remote(ui: &Diffui, arg: &CommandArg) -> Option<(String, String)> {
    let name = local_bookmark(ui, arg)?;
    let entry = ui.active().session.bookmarks.entry(&name)?;
    let remote = match arg {
        CommandArg::Bookmark {
            remote: Some(remote),
            ..
        } => remote.clone(),
        _ => entry.tracked_remote()?.to_owned(),
    };
    entry.tracks(&remote).then_some((name, remote))
}

// ── Shared builders ─────────────────────────────────────────────────────────

/// The revision a command acts on: the one its producer named, the one a
/// palette result resolves to, or — for a bare chord — whatever is selected.
pub(crate) fn target_revision(ui: &Diffui, arg: &CommandArg) -> Option<RevisionSelection> {
    match arg {
        CommandArg::Result(item) => palette::revision_selection(item, ui),
        _ => match arg.revision() {
            Some(revision) => Some(revision.clone()),
            None => Some(ui.active().session.selected_revision.clone()),
        },
    }
}

/// The sidebar's marked rows that are still in the loaded graph, in mark
/// order.
fn marked_revisions(ui: &Diffui) -> Vec<RevisionSelection> {
    ui.active()
        .revision_multi_selection
        .iter()
        .map(|hex| RevisionSelection::Commit(hex.clone()))
        .filter(|selection| row_present(ui, selection))
        .collect()
}

/// Enter target mode on whichever revision the command was aimed at.
fn draft(ui: &Diffui, arg: CommandArg, kind: mutations::DraftKind) -> Option<Action> {
    target_revision(ui, &arg).map(|source| Action::StartDraft { kind, source })
}

/// The commit backing a palette result, for the copy-from-the-graph commands.
fn result_row<'a>(ui: &'a Diffui, arg: &CommandArg) -> Option<diffui_core::RowView<'a>> {
    let commits = &ui.active().session.commits;
    match arg {
        CommandArg::Result(ResultRef::Commit { commit, .. }) => commits.find_by_commit_id(commit),
        CommandArg::Result(ResultRef::Bookmark(name)) => commits.find_by_bookmark(name),
        CommandArg::Result(ResultRef::WorkingCopy) => commits.working_copy(),
        _ => {
            let selection = arg
                .revision()
                .cloned()
                .unwrap_or_else(|| ui.active().session.selected_revision.clone());
            match selection {
                RevisionSelection::WorkingCopy => commits.working_copy(),
                RevisionSelection::Commit(hex) => commits.find_by_commit_id(&hex),
            }
        }
    }
}

/// One `⌘N` tab-jump command. A macro so the nine near-identical rows stay
/// one definition, and so each still lands in the registry as a plain literal.
macro_rules! tab_select {
    ($n:literal, $id:literal, $label:literal, $chord:literal, $index:literal) => {
        Command {
            id: $id,
            label: $label,
            hint: "Activate the tab at this position",
            contexts: &[Context::Base],
            chords: &[$chord],
            mac_chords: &[],
            enabled: always,
            build: |_ui, _arg| Some(Action::SelectTabIndex($index)),
            palette: PaletteScope::Hidden,
        }
    };
}

/// The registry. Grouped by area; within a [`PaletteScope`] the order here is
/// the order the palette lists them in for an empty query.
pub(crate) static REGISTRY: &[Command] = &[
    // ── Repository ──────────────────────────────────────────────────────
    Command {
        id: "repo.refresh",
        label: "Refresh repository",
        hint: "Re-read the repository state",
        contexts: &[Context::Base],
        chords: &["cmd+r"],
        mac_chords: &[],
        enabled: can_read_graph,
        build: |_ui, _arg| Some(Action::Refresh),
        palette: PaletteScope::Root,
    },
    Command {
        id: "find.open",
        label: "Find in current diff",
        hint: "In-diff search across all files",
        // ⌘F reaches the find bar from inside the other text overlays too —
        // they own the keyboard but not this chord.
        contexts: &[
            Context::Base,
            Context::Palette,
            Context::Find,
            Context::OpenRepo,
        ],
        chords: &["cmd+f"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::OpenFind),
        palette: PaletteScope::Root,
    },
    Command {
        id: "file.next",
        label: "Select next file",
        hint: "Move between files in current diff",
        contexts: &[Context::Base],
        chords: &["j", "down"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::SelectNextFile),
        palette: PaletteScope::Root,
    },
    Command {
        id: "file.previous",
        label: "Select previous file",
        hint: "Move between files in current diff",
        contexts: &[Context::Base],
        chords: &["k", "up"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::SelectPreviousFile),
        palette: PaletteScope::Root,
    },
    Command {
        id: "theme.system",
        label: "Theme: System",
        hint: "Follow OS appearance",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::SetTheme(ThemePreference::System)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "theme.dark",
        label: "Theme: Dark",
        hint: "Set palette theme",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::SetTheme(ThemePreference::Dark)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "theme.light",
        label: "Theme: Light",
        hint: "Set palette theme",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::SetTheme(ThemePreference::Light)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "theme.contrast",
        label: "Theme: High contrast",
        hint: "Set palette theme",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::SetTheme(ThemePreference::HighContrast)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "file.copy-diff",
        label: "Copy current file diff",
        hint: "Copy the selected file's diff text",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: has_selected_file,
        build: |_ui, _arg| Some(Action::CopyFileDiff),
        palette: PaletteScope::Root,
    },
    Command {
        id: "repo.fetch",
        label: "Fetch",
        hint: "Fetch from the tracked remotes",
        contexts: &[Context::Base],
        chords: &["f"],
        mac_chords: &[],
        enabled: can_fetch,
        build: |_ui, arg| match arg {
            CommandArg::Fetch(target) => Some(Action::Fetch(target)),
            _ => Some(Action::Fetch(FetchTarget::AllRemotes)),
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "repo.undo",
        label: "Undo last operation",
        hint: "Step back one jj operation (jj undo); file saves are kept",
        // Target mode hands ⌘-chords down to the base context, so ⌘Z there
        // undoes too. A focused text field keeps ⌘Z for its own text — see
        // `modes::needs_unconsumed`.
        contexts: &[Context::Base],
        chords: &["cmd+z", "u"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |_ui, _arg| Some(Action::Undo),
        palette: PaletteScope::Root,
    },
    Command {
        id: "repo.redo",
        label: "Redo last undone operation",
        hint: "Re-apply what the last undo took back (jj redo)",
        contexts: &[Context::Base],
        // ⇧ lives in the character: ⌘⇧Z arrives as `cmd+Z`.
        chords: &["cmd+Z", "U"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |_ui, _arg| Some(Action::Redo),
        palette: PaletteScope::Root,
    },
    Command {
        id: "repo.set-revset",
        label: "Set revset",
        hint: "Filter the log with this expression",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_read_graph,
        build: |_ui, arg| match arg {
            CommandArg::Revset(expr) => Some(Action::SetRevset(expr)),
            _ => None,
        },
        palette: PaletteScope::Hidden,
    },
    // ── Palette rows targeting a revision ───────────────────────────────
    Command {
        id: "revision.select",
        label: "Jump to revision",
        hint: "Show this revision in the diff view",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, arg| match arg {
            CommandArg::Result(item) => Some(Action::JumpToResult(item)),
            CommandArg::Revision(revision) => Some(Action::SelectRevision(revision)),
            _ => None,
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "revision.copy-change-id",
        label: "Copy change-id",
        hint: "Copy the revision's change-id",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |ui, arg| result_row(ui, &arg).map(|row| Action::Copy(row.change_id().to_owned())),
        palette: PaletteScope::Revision,
    },
    Command {
        id: "revision.copy-commit-message",
        label: "Copy commit message",
        hint: "Copy the commit message",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |ui, arg| {
            result_row(ui, &arg)
                .filter(|row| row.has_description())
                .map(|row| Action::Copy(row.description().to_owned()))
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "revision.copy-author",
        label: "Copy author",
        hint: "Copy author name and email",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |ui, arg| {
            result_row(ui, &arg)
                .map(|row| row.author().to_owned())
                .filter(|author| !author.is_empty())
                .map(Action::Copy)
        },
        palette: PaletteScope::Revision,
    },
    // ── Palette rows targeting a file ───────────────────────────────────
    Command {
        id: "file.open",
        label: "Open file",
        hint: "Scroll the diff to this file",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, arg| match arg {
            CommandArg::Result(ResultRef::File(path)) | CommandArg::Path(path) => {
                Some(Action::JumpToFile(path))
            }
            _ => None,
        },
        palette: PaletteScope::File,
    },
    Command {
        id: "file.copy-path",
        label: "Copy file path",
        hint: "Copy the file's path",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, arg| match arg {
            CommandArg::Result(ResultRef::File(path)) | CommandArg::Path(path) => {
                Some(Action::Copy(path))
            }
            _ => None,
        },
        palette: PaletteScope::File,
    },
    Command {
        id: "file.copy-absolute-path",
        label: "Copy absolute path",
        hint: "Copy the file's path from the filesystem root",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| ui.active().repository.is_some(),
        build: |ui, arg| {
            let path = arg.path()?;
            let repository = ui.active().repository.as_ref()?;
            Some(Action::Copy(
                repository.root.join(path).display().to_string(),
            ))
        },
        palette: PaletteScope::Hidden,
    },
    // ── Revision verbs (the context menu, the target-mode chords) ───────
    Command {
        id: "revision.edit-description",
        label: "Edit description\u{2026}",
        hint: "Rewrite this revision's message",
        contexts: &[Context::Base],
        chords: &["d"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg).map(|target| Action::EditDescription {
                target: Some(target),
            })
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.new-child",
        label: "New child",
        hint: "Start a new change on top of this one",
        contexts: &[Context::Base],
        chords: &["n"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|parent| Action::Mutate(mutations::MutationOp::New { parent }))
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.edit",
        label: "Edit",
        hint: "Make this revision the working copy",
        contexts: &[Context::Base],
        chords: &["e"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|target| Action::Mutate(mutations::MutationOp::Edit { target }))
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.rebase.start",
        label: "Onto\u{2026}",
        hint: "Pick a destination for this revision",
        contexts: &[Context::Base],
        chords: &["r"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            draft(
                ui,
                arg,
                mutations::DraftKind::Rebase {
                    mode: mutations::RebaseSourceMode::Revisions,
                },
            )
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.rebase.descendants.start",
        label: "With descendants onto\u{2026}",
        hint: "Move this revision and everything under it",
        contexts: &[Context::Base],
        chords: &["R"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            draft(
                ui,
                arg,
                mutations::DraftKind::Rebase {
                    mode: mutations::RebaseSourceMode::WithDescendants,
                },
            )
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.rebase.branch.start",
        label: "Whole branch onto\u{2026}",
        hint: "Move the branch this revision sits on",
        contexts: &[Context::Base],
        chords: &["b"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            draft(
                ui,
                arg,
                mutations::DraftKind::Rebase {
                    mode: mutations::RebaseSourceMode::Branch,
                },
            )
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.squash.start",
        label: "Into\u{2026}",
        hint: "Squash this revision into a picked one",
        contexts: &[Context::Base],
        chords: &["s"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| draft(ui, arg, mutations::DraftKind::Squash),
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.merge.start",
        label: "Merge with\u{2026}",
        hint: "Create a merge of this revision and a picked one",
        contexts: &[Context::Base],
        chords: &["m"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| draft(ui, arg, mutations::DraftKind::Merge),
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.squash-into-parent",
        label: "Into parent",
        hint: "Squash this revision into its parent",
        contexts: &[Context::Base],
        chords: &["S"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg).map(|from| {
                Action::Mutate(mutations::MutationOp::Squash {
                    from: vec![from],
                    into: mutations::SquashTarget::Parent,
                })
            })
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.duplicate",
        label: "Duplicate",
        hint: "Copy this revision alongside itself",
        contexts: &[Context::Base],
        chords: &["y"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|target| Action::Mutate(mutations::MutationOp::Duplicate { target }))
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.absorb",
        label: "Absorb into ancestors",
        hint: "Move each change into the revision that introduced it",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|from| Action::Mutate(mutations::MutationOp::Absorb { from }))
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.abandon",
        label: "Abandon",
        hint: "Drop this revision, keeping its descendants",
        contexts: &[Context::Base],
        chords: &["a"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| match arg {
            CommandArg::Revisions(targets) if !targets.is_empty() => {
                Some(Action::Mutate(mutations::MutationOp::Abandon { targets }))
            }
            // A menu row was aimed at one row on purpose; run it.
            CommandArg::Revision(target) => Some(Action::Mutate(mutations::MutationOp::Abandon {
                targets: vec![target],
            })),
            // A single key or a palette pick is one stray press away from
            // dropping a revision, so those ask first. A bare `a` takes the
            // marked rows when there are any, as the context menu does.
            other => {
                let marked = marked_revisions(ui);
                let targets = if matches!(other, CommandArg::None) && !marked.is_empty() {
                    marked
                } else {
                    vec![target_revision(ui, &other)?]
                };
                Some(Action::ConfirmMutation(mutations::MutationOp::Abandon {
                    targets,
                }))
            }
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.browse-source",
        label: "Browse source",
        hint: "Open the source browser at this revision",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_browse,
        build: |ui, arg| {
            let path = arg.path().map(str::to_owned);
            target_revision(ui, &arg).map(|revision| Action::BrowseSource { revision, path })
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.copy-text",
        label: "Copy",
        hint: "Copy this value",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, arg| match arg {
            CommandArg::Text(text) => Some(Action::Copy(text)),
            _ => None,
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "revision.copy-detail",
        label: "Copy detail",
        hint: "Copy a field read from the repository",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, arg| match arg {
            CommandArg::Detail {
                revision,
                field,
                fallback,
            } => Some(Action::CopyDetail {
                revision,
                field,
                fallback,
            }),
            _ => None,
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "selection.clear",
        label: "Clear selection",
        hint: "Drop the sidebar's marked rows",
        contexts: &[Context::Base],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::ClearMultiSelection),
        palette: PaletteScope::Hidden,
    },
    // ── Whole-revision ops: commit, new -A/-B, duplicate placement,
    // divergent changes, author metadata, and the marked-set batch verbs ──
    Command {
        id: "revision.commit",
        label: "Commit\u{2026}",
        hint: "Describe the working copy and start a new change on top",
        contexts: &[Context::Base],
        chords: &["c"],
        mac_chords: &[],
        enabled: revops::can_commit,
        build: |_ui, _arg| Some(Action::Revops(RevopsAction::BeginCommit)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "revision.new-after",
        label: "Insert new change after",
        hint: "New change between this revision and its children",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| revops::insert_new_change(ui, arg, mutations::PlacementKind::After),
        palette: PaletteScope::Revision,
    },
    Command {
        id: "revision.new-before",
        label: "Insert new change before",
        hint: "New change between this revision and its parents",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| revops::insert_new_change(ui, arg, mutations::PlacementKind::Before),
        palette: PaletteScope::Revision,
    },
    Command {
        id: "revision.new.start",
        label: "New change at\u{2026}",
        hint: "Pick where a new change goes: onto, after or before a revision",
        contexts: &[Context::Base],
        chords: &["N"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| revops::new_change_at(ui, arg, mutations::PlacementKind::Onto),
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.duplicate.start",
        label: "Duplicate onto\u{2026}",
        hint: "Pick where a copy of this revision lands",
        contexts: &[Context::Base],
        chords: &["Y"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| draft(ui, arg, mutations::DraftKind::Duplicate),
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.divergent.keep",
        label: "Keep this divergent copy",
        hint: "Abandon the change's other visible copies, ending the divergence",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::is_divergent_target,
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|keep| Action::Mutate(mutations::MutationOp::KeepDivergent { keep }))
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "revision.divergent.abandon",
        label: "Abandon this divergent copy",
        hint: "Drop this copy of a divergent change, keeping the others",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::is_divergent_target,
        build: |ui, arg| {
            target_revision(ui, &arg).map(|target| {
                Action::Mutate(mutations::MutationOp::Abandon {
                    targets: vec![target],
                })
            })
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "revision.reset-author",
        label: "Reset author to me",
        hint: "Set the author to the configured user (jj metaedit --update-author)",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| revops::metaedit(ui, arg, true, false),
        palette: PaletteScope::Revision,
    },
    Command {
        id: "revision.update-author-timestamp",
        label: "Update author date to now",
        hint: "jj metaedit --update-author-timestamp",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| revops::metaedit(ui, arg, false, true),
        palette: PaletteScope::Revision,
    },
    Command {
        id: "revision.parallelize.start",
        label: "Parallelize with\u{2026}",
        hint: "Pick revisions to make siblings of this one (space adds the candidate)",
        contexts: &[Context::Base],
        chords: &["p"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| draft(ui, arg, mutations::DraftKind::Parallelize),
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "selection.toggle",
        label: "Mark or unmark revision",
        hint: "Add the revision to (or drop it from) the batch selection",
        contexts: &[Context::Base],
        chords: &["space"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg).map(|target| Action::Revops(RevopsAction::ToggleMark(target)))
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "selection.rebase.start",
        label: "Rebase marked revisions onto\u{2026}",
        hint: "Pick a destination for every marked revision",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_one,
        build: |ui, arg| {
            revops::batch_draft(
                ui,
                &arg,
                mutations::DraftKind::Rebase {
                    mode: mutations::RebaseSourceMode::Revisions,
                },
            )
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "selection.rebase.descendants.start",
        label: "Rebase marked revisions with descendants onto\u{2026}",
        hint: "Move every marked revision and everything under it",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_one,
        build: |ui, arg| {
            revops::batch_draft(
                ui,
                &arg,
                mutations::DraftKind::Rebase {
                    mode: mutations::RebaseSourceMode::WithDescendants,
                },
            )
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "selection.squash.start",
        label: "Squash marked revisions into\u{2026}",
        hint: "Fold every marked revision into a picked one",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_one,
        build: |ui, arg| revops::batch_draft(ui, &arg, mutations::DraftKind::Squash),
        palette: PaletteScope::Root,
    },
    Command {
        id: "selection.duplicate",
        label: "Duplicate marked revisions",
        hint: "Copy every marked revision onto its own parents",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_one,
        build: |ui, arg| {
            Some(Action::Mutate(mutations::MutationOp::DuplicateTo {
                targets: revops::batch(ui, &arg),
                destination: None,
            }))
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "selection.duplicate.start",
        label: "Duplicate marked revisions onto\u{2026}",
        hint: "Pick where copies of the marked revisions land",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_one,
        build: |ui, arg| revops::batch_draft(ui, &arg, mutations::DraftKind::Duplicate),
        palette: PaletteScope::Root,
    },
    Command {
        id: "selection.new-merge",
        label: "New change on marked revisions",
        hint: "A merge commit with every marked revision as a parent",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_two,
        build: |ui, arg| {
            Some(Action::Mutate(mutations::MutationOp::Merge {
                parents: revops::batch(ui, &arg),
            }))
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "selection.parallelize",
        label: "Parallelize marked revisions\u{2026}",
        hint: "Make the marked revisions siblings, previewing new conflicts",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_two,
        build: |ui, arg| revops::batch_draft(ui, &arg, mutations::DraftKind::Parallelize),
        palette: PaletteScope::Root,
    },
    Command {
        id: "selection.abandon",
        label: "Abandon marked revisions",
        hint: "Drop every marked revision, keeping their descendants",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_one,
        build: |ui, arg| {
            Some(Action::Mutate(mutations::MutationOp::Abandon {
                targets: revops::batch(ui, &arg),
            }))
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "selection.reset-author",
        label: "Reset author to me on marked revisions",
        hint: "jj metaedit --update-author on every marked revision",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_one,
        build: |ui, arg| {
            revops::metaedit(
                ui,
                CommandArg::Revisions(revops::batch(ui, &arg)),
                true,
                false,
            )
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "selection.update-author-timestamp",
        label: "Update author date on marked revisions",
        hint: "jj metaedit --update-author-timestamp on every marked revision",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: revops::batch_of_one,
        build: |ui, arg| {
            revops::metaedit(
                ui,
                CommandArg::Revisions(revops::batch(ui, &arg)),
                false,
                true,
            )
        },
        palette: PaletteScope::Root,
    },
    // ── Bookmarks ───────────────────────────────────────────────────────
    Command {
        id: "bookmark.move",
        label: "Move bookmark here",
        hint: "Point this bookmark at the revision",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |_ui, arg| match arg {
            CommandArg::Bookmark {
                name,
                remote,
                to: Some(to),
            } => Some(Action::Mutate(mutations::MutationOp::MoveBookmark {
                name,
                to,
                push_remote: remote,
            })),
            _ => None,
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "bookmark.delete",
        label: "Delete",
        hint: "Delete this bookmark (pushing later deletes it on tracked remotes too)",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| can_mutate(ui, arg) && local_bookmark(ui, arg).is_some(),
        build: |ui, arg| {
            local_bookmark(ui, &arg)
                .map(|name| Action::Mutate(mutations::MutationOp::DeleteBookmark { name }))
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "bookmark.track",
        label: "Track",
        hint: "jj bookmark track: follow this remote bookmark with a local one",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| can_mutate(ui, arg) && untracked_remote_bookmark(ui, arg).is_some(),
        build: |ui, arg| {
            let (name, remote) = untracked_remote_bookmark(ui, &arg)?;
            Some(Action::Mutate(mutations::MutationOp::TrackBookmark {
                name,
                remote,
            }))
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "bookmark.push",
        label: "Push",
        hint: "Push this bookmark to its tracked remote",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |_ui, arg| match arg {
            CommandArg::Bookmark {
                name,
                remote: Some(remote),
                ..
            } => Some(Action::Mutate(mutations::MutationOp::PushBookmark {
                name,
                remote,
            })),
            _ => None,
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "bookmark.create",
        label: "Create bookmark\u{2026}",
        hint: "Name a new bookmark at the revision",
        contexts: &[Context::Base],
        chords: &["B"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg).map(|at| Action::Bookmark(BookmarkAction::Create { at }))
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "revision.push-change",
        label: "Push as new bookmark",
        hint: "jj git push --change: push the revision under a generated bookmark",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| can_mutate(ui, arg) && has_remotes(ui),
        build: |ui, arg| {
            let remote = match &arg {
                CommandArg::PushRevision { remote, .. } => remote.clone(),
                _ => None,
            };
            target_revision(ui, &arg)
                .map(|at| Action::Bookmark(BookmarkAction::PushChange { at, remote }))
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "repo.push",
        label: "Push tracked bookmarks",
        hint: "jj git push: the tracked bookmarks between the remote and @",
        contexts: &[Context::Base],
        chords: &["P"],
        mac_chords: &[],
        enabled: |ui, arg| can_mutate(ui, arg) && has_remotes(ui),
        build: |_ui, _arg| Some(Action::Bookmark(BookmarkAction::PushTracked)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "bookmark.push-to",
        label: "Push bookmark",
        hint: "Push to its tracked remote, or create it on the default one",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| {
            can_mutate(ui, arg) && has_remotes(ui) && local_bookmark(ui, arg).is_some()
        },
        build: |ui, arg| {
            let name = local_bookmark(ui, &arg)?;
            let remote = match arg {
                CommandArg::Bookmark { remote, .. } => remote,
                _ => None,
            };
            Some(Action::Bookmark(BookmarkAction::Push { name, remote }))
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "bookmark.rename",
        label: "Rename\u{2026}",
        hint: "Rename this bookmark",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| can_mutate(ui, arg) && local_bookmark(ui, arg).is_some(),
        build: |ui, arg| {
            local_bookmark(ui, &arg).map(|name| Action::Bookmark(BookmarkAction::Rename { name }))
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "bookmark.delete-everywhere",
        label: "Delete here and on remote",
        hint: "Delete the bookmark and push the deletion",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| can_mutate(ui, arg) && deletion_remote(ui, arg).is_some(),
        build: |ui, arg| {
            let (name, remote) = deletion_remote(ui, &arg)?;
            Some(Action::Bookmark(BookmarkAction::DeleteEverywhere {
                name,
                remote,
            }))
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "bookmark.forget",
        label: "Forget",
        hint: "Drop the bookmark and stop tracking its remotes",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| can_mutate(ui, arg) && local_bookmark(ui, arg).is_some(),
        build: |ui, arg| {
            local_bookmark(ui, &arg)
                .map(|name| Action::Mutate(mutations::MutationOp::ForgetBookmark { name }))
        },
        palette: PaletteScope::Revision,
    },
    Command {
        id: "bookmark.untrack",
        label: "Untrack",
        hint: "Stop tracking this remote bookmark",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |_ui, arg| match arg {
            CommandArg::Bookmark {
                name,
                remote: Some(remote),
                ..
            } => Some(Action::Mutate(mutations::MutationOp::UntrackBookmark {
                name,
                remote,
            })),
            _ => None,
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "bookmark.prompt-cancel",
        label: "Cancel",
        hint: "Close the bookmark-name prompt",
        contexts: &[Context::Prompt],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::Bookmark(BookmarkAction::PromptCancel)),
        palette: PaletteScope::Hidden,
    },
    // ── Target mode ─────────────────────────────────────────────────────
    Command {
        id: "draft.candidate.next",
        label: "Next destination",
        hint: "Move the destination candidate down",
        contexts: &[Context::Draft],
        chords: &["j", "down"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::DraftCandidate(1)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "draft.candidate.previous",
        label: "Previous destination",
        hint: "Move the destination candidate up",
        contexts: &[Context::Draft],
        chords: &["k", "up"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::DraftCandidate(-1)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "draft.place.onto",
        label: "Place onto",
        hint: "Apply the draft onto the candidate",
        contexts: &[Context::Draft],
        chords: &["o"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::DraftApplyPlacement(mutations::PlacementKind::Onto)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "draft.place.after",
        label: "Place after",
        hint: "Apply the draft after the candidate",
        contexts: &[Context::Draft],
        chords: &["a"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::DraftApplyPlacement(mutations::PlacementKind::After)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "draft.place.before",
        label: "Place before",
        hint: "Apply the draft before the candidate",
        contexts: &[Context::Draft],
        chords: &["b"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| {
            Some(Action::DraftApplyPlacement(
                mutations::PlacementKind::Before,
            ))
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "draft.confirm",
        label: "Apply draft",
        hint: "Run the draft on the armed candidate",
        contexts: &[Context::Draft],
        chords: &["enter"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::DraftConfirm),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "draft.cancel",
        label: "Cancel draft",
        hint: "Leave target mode without running anything",
        contexts: &[Context::Draft],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::DraftCancel),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "draft.toggle-source",
        label: "Toggle draft source",
        hint: "Stack or un-stack the candidate as a source",
        contexts: &[Context::Draft],
        chords: &["space"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::DraftToggleSource),
        palette: PaletteScope::Hidden,
    },
    // ── Overlays ────────────────────────────────────────────────────────
    Command {
        id: "palette.toggle",
        label: "Command palette",
        hint: "Search revisions, files and commands",
        // Same reasoning as `find.open`: the overlays that own the keyboard
        // still hand ⌘K back, so the palette is always one chord away.
        contexts: &[
            Context::Base,
            Context::Palette,
            Context::Find,
            Context::OpenRepo,
        ],
        chords: &["cmd+k"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::TogglePalette),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "palette.next",
        label: "Next result",
        hint: "Highlight the next palette result",
        contexts: &[Context::Palette],
        chords: &["down"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::PaletteMove(1)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "palette.previous",
        label: "Previous result",
        hint: "Highlight the previous palette result",
        contexts: &[Context::Palette],
        chords: &["up"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::PaletteMove(-1)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "palette.actions",
        label: "Result actions",
        hint: "Push an actions column for the highlighted result",
        contexts: &[Context::Palette],
        chords: &["tab"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::PaletteActions),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "palette.back",
        label: "Back",
        hint: "Pop the rightmost palette column",
        contexts: &[Context::Palette],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::PalettePop),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "find.next",
        label: "Next match",
        hint: "Advance to the next in-diff match",
        contexts: &[Context::Find],
        chords: &["enter"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::FindNext),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "find.previous",
        label: "Previous match",
        hint: "Go back to the previous in-diff match",
        contexts: &[Context::Find],
        chords: &["shift+enter"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::FindPrevious),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "find.close",
        label: "Close find",
        hint: "Dismiss the in-diff find bar",
        contexts: &[Context::Find],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::CloseFind),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "repo.open-dialog",
        label: "Open repository\u{2026}",
        hint: "Open another repository in a new tab",
        contexts: &[Context::Base],
        chords: &["cmd+o"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::OpenRepoDialog),
        palette: PaletteScope::Root,
    },
    Command {
        id: "repo.close-dialog",
        label: "Close dialog",
        hint: "Dismiss the open-repository dialog",
        contexts: &[Context::OpenRepo],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::CloseRepoDialog),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "repo.next-folder",
        label: "Next folder",
        hint: "Highlight the next folder suggestion",
        contexts: &[Context::OpenRepo],
        chords: &["down"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::OpenRepoMove(1)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "repo.previous-folder",
        label: "Previous folder",
        hint: "Highlight the previous folder suggestion",
        contexts: &[Context::OpenRepo],
        chords: &["up"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::OpenRepoMove(-1)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "repo.complete-folder",
        label: "Complete folder",
        hint: "Complete the path into the highlighted folder",
        contexts: &[Context::OpenRepo],
        chords: &["tab"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::OpenRepoComplete),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "confirm.accept",
        label: "Confirm",
        hint: "Run the mutation the dialog is holding",
        // Deliberately chordless: the dialog gates an operation the jj CLI
        // itself refuses, so it takes a click rather than a stray Enter.
        contexts: &[Context::Confirm],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::ConfirmAccept),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "confirm.cancel",
        label: "Cancel",
        hint: "Dismiss the dialog and drop its mutation",
        contexts: &[Context::Confirm],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::ConfirmCancel),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "description.save",
        label: "Save description",
        hint: "Write the edited description back",
        contexts: &[Context::Description],
        chords: &["cmd+enter"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::SaveDescription),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "description.cancel",
        label: "Cancel edit",
        hint: "Close the description editor, discarding the edit",
        contexts: &[Context::Description],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::CancelDescription),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "split.confirm",
        label: "Split",
        hint: "Split the revision with these descriptions",
        contexts: &[Context::Split],
        chords: &["cmd+enter"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::Changes(crate::changes::ChangesMsg::SplitConfirm)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "split.cancel",
        label: "Cancel split",
        hint: "Close the split prompt",
        contexts: &[Context::Split],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::Changes(crate::changes::ChangesMsg::SplitCancel)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "menu.dismiss",
        label: "Dismiss menu",
        hint: "Close the open popup menu",
        contexts: &[Context::Menu],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::DismissMenu),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "activity.close",
        label: "Close activity log",
        hint: "Dismiss the activity popover",
        contexts: &[Context::ActivityPopover],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::CloseActivityPopover),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "activity.toggle",
        label: "Activity log",
        hint: "Show what the repository is doing",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::ToggleActivityPopover),
        palette: PaletteScope::Root,
    },
    Command {
        id: "activity.clear",
        label: "Clear activities",
        hint: "Drop the finished rows from the activity log",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::ClearActivities),
        palette: PaletteScope::Hidden,
    },
    // ── View ────────────────────────────────────────────────────────────
    Command {
        id: "view.toggle-wrap",
        label: "Toggle line wrap",
        hint: "Wrap long diff lines, or clip them at the pane edge",
        contexts: &[Context::Base],
        chords: &["alt+z"],
        // macOS composes ⌥Z into `Ω`, which is what the key event carries.
        mac_chords: &["alt+z", "alt+\u{3c9}"],
        enabled: always,
        build: |_ui, _arg| Some(Action::ToggleWrap),
        palette: PaletteScope::Root,
    },
    Command {
        id: "view.toggle-commit-details",
        label: "Toggle commit details",
        hint: "Show the description and fields above the diff, or just the title",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::ToggleCommitDetails),
        palette: PaletteScope::Root,
    },
    Command {
        id: "view.toggle-split",
        label: "Toggle side-by-side diff",
        hint: "Switch between the unified and two-column diff",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::ToggleSplit),
        palette: PaletteScope::Root,
    },
    Command {
        id: "view.diff",
        label: "Show the diff",
        hint: "Switch the main pane back to the diff",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| ui.active().main_view != MainView::Diff,
        build: |_ui, _arg| Some(Action::SetMainView(MainView::Diff)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "view.source",
        label: "Browse the source tree",
        hint: "Switch the main pane to the source browser",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| can_browse(ui, arg) && ui.active().main_view != MainView::Source,
        build: |_ui, _arg| Some(Action::SetMainView(MainView::Source)),
        palette: PaletteScope::Root,
    },
    // ── Tabs ────────────────────────────────────────────────────────────
    Command {
        id: "tab.close",
        label: "Close tab",
        hint: "Close the active tab",
        contexts: &[Context::Base],
        chords: &["cmd+w"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::CloseActiveTab),
        palette: PaletteScope::Root,
    },
    tab_select!(1, "tab.select-1", "Go to tab 1", "cmd+1", 0),
    tab_select!(2, "tab.select-2", "Go to tab 2", "cmd+2", 1),
    tab_select!(3, "tab.select-3", "Go to tab 3", "cmd+3", 2),
    tab_select!(4, "tab.select-4", "Go to tab 4", "cmd+4", 3),
    tab_select!(5, "tab.select-5", "Go to tab 5", "cmd+5", 4),
    tab_select!(6, "tab.select-6", "Go to tab 6", "cmd+6", 5),
    tab_select!(7, "tab.select-7", "Go to tab 7", "cmd+7", 6),
    tab_select!(8, "tab.select-8", "Go to tab 8", "cmd+8", 7),
    tab_select!(9, "tab.select-9", "Go to tab 9", "cmd+9", 8),
    // ── Partial changes and conflicts (the diff pane's checkboxes) ──────
    Command {
        id: "changes.toggle-file",
        label: "Select file for split / squash",
        hint: "Tick or untick the file in the change selection",
        contexts: &[Context::Base],
        chords: &["x"],
        mac_chords: &[],
        enabled: |ui, arg| crate::changes::command_file(ui, arg).is_some(),
        build: |ui, arg| {
            let index = crate::changes::command_file(ui, &arg)?;
            Some(Action::Changes(crate::changes::ChangesMsg::ToggleFile(
                index,
            )))
        },
        palette: PaletteScope::File,
    },
    Command {
        id: "changes.split",
        label: "Split selected changes\u{2026}",
        hint: "Keep the selection in this revision, move the rest to a new one",
        contexts: &[Context::Base],
        chords: &["X"],
        mac_chords: &[],
        enabled: |ui, _arg| crate::changes::has_selection(ui),
        build: |_ui, _arg| Some(Action::Changes(crate::changes::ChangesMsg::OpenSplit)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "changes.squash-into-parent",
        label: "Squash selected changes into parent",
        hint: "jj squash with just the ticked files and hunks",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::changes::has_selection(ui),
        build: |_ui, _arg| {
            Some(Action::Changes(
                crate::changes::ChangesMsg::SquashIntoParent,
            ))
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "changes.move-into",
        label: "Move selected changes into\u{2026}",
        hint: "Pick the revision that receives the ticked files and hunks",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::changes::has_selection(ui),
        build: |_ui, _arg| Some(Action::Changes(crate::changes::ChangesMsg::MoveInto)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "changes.discard",
        label: "Discard selected changes\u{2026}",
        hint: "Restore the parent's content for the ticked files and hunks",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::changes::has_selection(ui),
        build: |_ui, _arg| Some(Action::Changes(crate::changes::ChangesMsg::Discard)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "changes.clear",
        label: "Clear change selection",
        hint: "Untick every file and hunk",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::changes::has_selection(ui),
        build: |_ui, _arg| Some(Action::Changes(crate::changes::ChangesMsg::Clear)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "lines.toggle",
        label: "Select lines",
        hint: "Pick single changed lines to split, squash, move or discard",
        contexts: &[Context::Base, Context::Lines],
        chords: &["v"],
        mac_chords: &[],
        enabled: |ui, _arg| crate::changes::can_select_lines(ui),
        build: |_ui, _arg| Some(lines_action(crate::changes::LineMsg::Toggle)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "lines.exit",
        label: "Stop selecting lines",
        hint: "Leave line-select mode; the picked lines stay picked",
        contexts: &[Context::Lines],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(lines_action(crate::changes::LineMsg::Exit)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "lines.next",
        label: "Next changed line",
        hint: "Move the line cursor down",
        contexts: &[Context::Lines],
        chords: &["j", "down"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| {
            Some(lines_action(crate::changes::LineMsg::Step {
                by: 1,
                extend: false,
            }))
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "lines.previous",
        label: "Previous changed line",
        hint: "Move the line cursor up",
        contexts: &[Context::Lines],
        chords: &["k", "up"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| {
            Some(lines_action(crate::changes::LineMsg::Step {
                by: -1,
                extend: false,
            }))
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "lines.extend-next",
        label: "Extend the pick down",
        hint: "Move down and give that line the same pick",
        contexts: &[Context::Lines],
        chords: &["J", "shift+down"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| {
            Some(lines_action(crate::changes::LineMsg::Step {
                by: 1,
                extend: true,
            }))
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "lines.extend-previous",
        label: "Extend the pick up",
        hint: "Move up and give that line the same pick",
        contexts: &[Context::Lines],
        chords: &["K", "shift+up"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| {
            Some(lines_action(crate::changes::LineMsg::Step {
                by: -1,
                extend: true,
            }))
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "lines.toggle-line",
        label: "Pick or unpick line",
        hint: "Toggle the line under the cursor",
        contexts: &[Context::Lines],
        chords: &["space"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(lines_action(crate::changes::LineMsg::ToggleCursor)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "file.restore",
        label: "Restore file from parent (discard changes)\u{2026}",
        hint: "jj restore for this file",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| crate::changes::command_file(ui, arg).is_some(),
        build: |ui, arg| {
            let path = crate::changes::command_file_path(ui, &arg)?;
            Some(Action::Changes(crate::changes::ChangesMsg::RestoreFile(
                path,
            )))
        },
        palette: PaletteScope::File,
    },
    Command {
        id: "file.move-into",
        label: "Move file changes into\u{2026}",
        hint: "Pick the revision that receives this file's changes",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| crate::changes::command_file(ui, arg).is_some(),
        build: |ui, arg| {
            let path = crate::changes::command_file_path(ui, &arg)?;
            Some(Action::Changes(crate::changes::ChangesMsg::MoveFile(path)))
        },
        palette: PaletteScope::File,
    },
    Command {
        id: "file.split",
        label: "Split file into new change\u{2026}",
        hint: "Move this file's changes into a new change on top",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| crate::changes::command_file(ui, arg).is_some(),
        build: |ui, arg| {
            let path = crate::changes::command_file_path(ui, &arg)?;
            Some(Action::Changes(crate::changes::ChangesMsg::SplitFile(path)))
        },
        palette: PaletteScope::File,
    },
    Command {
        id: "conflict.take-side-1",
        label: "Resolve conflict: take side 1",
        hint: "jj resolve --tool :ours for this file",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| crate::conflicts::command_resolution(ui, arg, Some(0)).is_some(),
        build: |ui, arg| crate::conflicts::command_resolution(ui, &arg, Some(0)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "conflict.take-side-2",
        label: "Resolve conflict: take side 2",
        hint: "jj resolve --tool :theirs for this file",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| crate::conflicts::command_resolution(ui, arg, Some(1)).is_some(),
        build: |ui, arg| crate::conflicts::command_resolution(ui, &arg, Some(1)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "conflict.take-base",
        label: "Resolve conflict: take base",
        hint: "Drop both sides' changes to this file",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| crate::conflicts::command_resolution(ui, arg, None).is_some(),
        build: |ui, arg| crate::conflicts::command_resolution(ui, &arg, None),
        palette: PaletteScope::Root,
    },
    Command {
        id: "conflict.merge-tool",
        label: "Resolve conflict in merge tool",
        hint: "Run ui.merge-editor and write the result back",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, arg| crate::conflicts::command_merge_tool(ui, arg).is_some(),
        build: |ui, arg| crate::conflicts::command_merge_tool(ui, &arg),
        palette: PaletteScope::Root,
    },
    // ── History (op log, evolog, file history, annotate) ────────────────
    Command {
        id: "history.op-log",
        label: "Show operation log",
        hint: "Browse jj's operation log; restore or revert an operation",
        contexts: &[Context::Base],
        chords: &["O"],
        mac_chords: &[],
        enabled: |ui, _arg| crate::history_panel::available(ui),
        build: |_ui, _arg| Some(Action::History(HistoryAction::ShowOpLog)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "history.evolog",
        label: "Show evolution log",
        hint: "Every previous version of this revision (jj evolog)",
        contexts: &[Context::Base],
        chords: &["E"],
        mac_chords: &[],
        enabled: |ui, arg| {
            crate::history_panel::available(ui)
                && arg
                    .revision()
                    .is_none_or(|revision| row_present(ui, revision))
        },
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|revision| Action::History(HistoryAction::ShowEvolog(revision)))
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "file.history",
        label: "Show file history",
        hint: "Narrow the graph to the revisions that touched this file",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::history_panel::available(ui),
        build: |ui, arg| {
            let revision = match &arg {
                CommandArg::Browse { revision, .. } => revision.clone(),
                _ => ui.active().session.selected_revision.clone(),
            };
            let path = match arg {
                CommandArg::Result(ResultRef::File(path)) => path,
                other => other.path()?.to_owned(),
            };
            Some(Action::History(HistoryAction::FileHistory {
                revision,
                path,
            }))
        },
        palette: PaletteScope::File,
    },
    Command {
        id: "file.history.current",
        label: "Show history of the current file",
        hint: "Narrow the graph to the revisions that touched the file on screen",
        contexts: &[Context::Base],
        chords: &["H"],
        mac_chords: &[],
        enabled: |ui, _arg| {
            crate::history_panel::available(ui) && crate::history_panel::current_file(ui).is_some()
        },
        build: |_ui, _arg| Some(Action::History(HistoryAction::CurrentFileHistory)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "file.history.clear",
        label: "Clear file history filter",
        hint: "Show the whole graph again",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::history_panel::file_filter_active(ui),
        build: |_ui, _arg| Some(Action::History(HistoryAction::ClearFileFilter)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "source.toggle-annotate",
        label: "Toggle annotate (blame)",
        hint: "Show which change last touched each line of the browsed file",
        contexts: &[Context::Base],
        chords: &["A"],
        mac_chords: &[],
        enabled: |ui, _arg| crate::history_panel::can_annotate(ui),
        build: |_ui, _arg| Some(Action::History(HistoryAction::ToggleAnnotate)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "history.restore-operation",
        label: "Restore to selected operation\u{2026}",
        hint: "jj op restore: put every revision and bookmark back to that point",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| {
            ui.active().session.capabilities.mutate
                && crate::history_panel::selected_operation(ui).is_some()
        },
        build: |ui, _arg| {
            crate::history_panel::selected_operation(ui)
                .map(|id| Action::History(HistoryAction::RestoreOperation(id)))
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "history.revert-operation",
        label: "Revert selected operation\u{2026}",
        hint: "jj op revert: undo just that operation, keeping later work",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| {
            ui.active().session.capabilities.mutate
                && crate::history_panel::selected_operation(ui).is_some()
        },
        build: |ui, _arg| {
            crate::history_panel::selected_operation(ui)
                .map(|id| Action::History(HistoryAction::RevertOperation(id)))
        },
        palette: PaletteScope::Root,
    },
    // ── Diagnostics ─────────────────────────────────────────────────────
    Command {
        id: "app.reveal-log",
        label: if cfg!(target_os = "macos") {
            "Reveal log file in Finder"
        } else {
            "Reveal log file"
        },
        hint: "Show diffui's log, for bug reports and crash details",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::RevealLogFile),
        palette: PaletteScope::Root,
    },
    // ── Help ────────────────────────────────────────────────────────────
    Command {
        id: "help.shortcuts",
        label: "Keyboard shortcuts",
        hint: "List every key binding",
        contexts: &[Context::Base, Context::Shortcuts],
        chords: &["?"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::ToggleShortcuts),
        palette: PaletteScope::Root,
    },
    Command {
        id: "app.open-config",
        label: "Open settings file",
        hint: "Edit config.toml (fonts, theme, keys, revset presets)",
        contexts: &[Context::Base],
        chords: &["cmd+,"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::OpenConfigFile),
        palette: PaletteScope::Root,
    },
    Command {
        id: "shortcuts.close",
        label: "Close shortcuts",
        hint: "Dismiss the keyboard shortcut list",
        contexts: &[Context::Shortcuts],
        chords: &["esc"],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::CloseShortcuts),
        palette: PaletteScope::Hidden,
    },
    // ── Comparing two revisions ─────────────────────────────────────────
    Command {
        id: "compare.with",
        label: "Compare with…",
        hint: "Diff against this revision; click another to pick the other side",
        contexts: &[Context::Base],
        chords: &["C"],
        mac_chords: &[],
        enabled: can_compare,
        build: |ui, arg| {
            target_revision(ui, &arg).map(|from| Action::Compare(CompareAction::PickWith(from)))
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "compare.with-working-copy",
        label: "Compare with working copy",
        hint: "Diff from this revision to the working copy",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_compare,
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|from| Action::Compare(CompareAction::WithWorkingCopy(from)))
        },
        palette: PaletteScope::RevisionVerb,
    },
    Command {
        id: "compare.marked",
        label: "Compare marked revisions",
        hint: "Diff the two ⌘-clicked revisions, older to newer",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::compare::has_marked_pair(ui),
        build: |_ui, _arg| Some(Action::Compare(CompareAction::Marked)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "compare.swap",
        label: "Swap compared revisions",
        hint: "Exchange the two sides of the comparison",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::compare::comparing(ui),
        build: |_ui, _arg| Some(Action::Compare(CompareAction::Swap)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "compare.interdiff",
        label: "Toggle interdiff",
        hint: "Switch between a tree diff and jj interdiff of the two revisions",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::compare::comparing(ui),
        build: |ui, _arg| {
            let interdiff = matches!(
                ui.active().session.diff_base,
                diffui_core::DiffBase::Interdiff(_)
            );
            Some(Action::Compare(CompareAction::SetInterdiff(!interdiff)))
        },
        palette: PaletteScope::Root,
    },
    Command {
        id: "compare.exit",
        label: "Stop comparing",
        hint: "Back to the selected revision's own diff",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| crate::compare::comparing(ui),
        build: |_ui, _arg| Some(Action::Compare(CompareAction::Exit)),
        palette: PaletteScope::Root,
    },
    Command {
        id: "file.expand-diff",
        label: "Show full diff of file",
        hint: "Load the diff of the selected file, which was collapsed for size",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: |ui, _arg| {
            let tab = ui.active();
            tab.session
                .document
                .files
                .get(tab.selected_file)
                .is_some_and(|file| file.collapsed.is_some())
        },
        build: |ui, _arg| Some(Action::ExpandFile(ui.active().selected_file)),
        palette: PaletteScope::Root,
    },
];

fn can_compare(ui: &Diffui, arg: &CommandArg) -> bool {
    ui.active().session.capabilities.compare
        && arg
            .revision()
            .is_none_or(|revision| row_present(ui, revision))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_command_has_a_label_and_a_context() {
        for command in REGISTRY {
            assert!(!command.id.is_empty(), "a command has no id");
            assert!(!command.label.is_empty(), "{} has no label", command.id);
            assert!(!command.hint.is_empty(), "{} has no hint", command.id);
            assert!(
                !command.contexts.is_empty(),
                "{} is valid in no context",
                command.id
            );
        }
    }

    #[test]
    fn command_ids_are_unique() {
        let mut seen = HashSet::new();
        for command in REGISTRY {
            assert!(
                seen.insert(command.id),
                "duplicate command id {}",
                command.id
            );
        }
    }

    /// A chord that no longer parses would silently drop a binding, so the
    /// defaults are checked here rather than at load time.
    #[test]
    fn every_default_chord_parses() {
        for command in REGISTRY {
            for chord in command.chords.iter().chain(command.mac_chords) {
                assert!(
                    crate::keymap::Chord::parse(chord).is_some(),
                    "{}: chord {chord:?} does not parse",
                    command.id
                );
            }
        }
    }

    #[test]
    fn revision_verbs_have_jj_style_keys() {
        let map = crate::keymap::Keymap::build(&Default::default()).0;
        for (key, id) in [
            ("n", "revision.new-child"),
            ("e", "revision.edit"),
            ("d", "revision.edit-description"),
            ("a", "revision.abandon"),
            ("s", "revision.squash.start"),
            ("S", "revision.squash-into-parent"),
        ] {
            let chord = crate::keymap::Chord::parse(key).expect("a parsable chord");
            assert_eq!(map.resolve(Context::Base, &chord), Some(id), "{key}");
        }
    }

    /// A key or palette abandon asks first; a menu row aimed at a revision
    /// runs as picked.
    #[test]
    fn only_an_unaimed_abandon_asks_first() {
        let mut ui = crate::tabs::tests::app();
        crate::tabs::tests::push_tab(&mut ui, "/tmp/repo");
        let abandon = command("revision.abandon").expect("registered");
        assert!(matches!(
            (abandon.build)(&ui, CommandArg::None),
            Some(Action::ConfirmMutation(_))
        ));
        assert!(matches!(
            (abandon.build)(&ui, CommandArg::Revision(RevisionSelection::WorkingCopy)),
            Some(Action::Mutate(_))
        ));
    }

    /// The revision verbs are offered at the top level (on the selection) and
    /// in a revision result's actions column (on that result).
    #[test]
    fn revision_verbs_are_listed_in_both_palette_scopes() {
        let mut ui = crate::tabs::tests::app();
        crate::tabs::tests::push_tab(&mut ui, "/tmp/repo");
        ui.tabs[0].state.session.capabilities.mutate = true;
        for scope in [PaletteScope::Root, PaletteScope::Revision] {
            let ids: Vec<CommandId> = palette_commands(&ui, scope, &CommandArg::None)
                .iter()
                .map(|command| command.id)
                .collect();
            for id in ["revision.new-child", "revision.edit", "revision.abandon"] {
                assert!(ids.contains(&id), "{id} missing from {scope:?}");
            }
        }
        let files: Vec<CommandId> = palette_commands(&ui, PaletteScope::File, &CommandArg::None)
            .iter()
            .map(|command| command.id)
            .collect();
        assert!(!files.contains(&"revision.abandon"));
    }

    /// Two commands sharing a chord inside one context would make resolution
    /// depend on registry order, which is not something a reader can see.
    #[test]
    fn default_chords_do_not_collide_within_a_context() {
        for context in Context::ALL {
            let mut seen: Vec<(crate::keymap::Chord, CommandId)> = Vec::new();
            for command in REGISTRY {
                if !command.contexts.contains(context) {
                    continue;
                }
                for chord in command.chords.iter().chain(command.mac_chords) {
                    let chord = crate::keymap::Chord::parse(chord).expect("a parsable chord");
                    if let Some((_, other)) = seen
                        .iter()
                        .find(|(seen, id)| *seen == chord && *id != command.id)
                    {
                        panic!(
                            "{} and {other} both bind {chord:?} in {}",
                            command.id,
                            context.name()
                        );
                    }
                    seen.push((chord, command.id));
                }
            }
        }
    }
}

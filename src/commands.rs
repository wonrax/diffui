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
use crate::palette::{self, ResultRef};
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
}

impl CommandArg {
    fn revision(&self) -> Option<&RevisionSelection> {
        match self {
            CommandArg::Revision(revision)
            | CommandArg::Browse { revision, .. }
            | CommandArg::Detail { revision, .. } => Some(revision),
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
        .filter(|command| command.palette == scope)
        .filter(|command| (command.enabled)(ui, arg))
        .collect()
}

// ── Shared predicates ───────────────────────────────────────────────────────

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
fn can_mutate(ui: &Diffui, arg: &CommandArg) -> bool {
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

// ── Shared builders ─────────────────────────────────────────────────────────

/// The revision a command acts on: the one its producer named, the one a
/// palette result resolves to, or — for a bare chord — whatever is selected.
fn target_revision(ui: &Diffui, arg: &CommandArg) -> Option<RevisionSelection> {
    match arg {
        CommandArg::Result(item) => palette::revision_selection(item, ui),
        _ => match arg.revision() {
            Some(revision) => Some(revision.clone()),
            None => Some(ui.active().session.selected_revision.clone()),
        },
    }
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
        chords: &[],
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
        chords: &[],
        mac_chords: &[],
        enabled: can_fetch,
        build: |_ui, arg| match arg {
            CommandArg::Fetch(target) => Some(Action::Fetch(target)),
            _ => Some(Action::Fetch(FetchTarget::AllRemotes)),
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "repo.undo",
        label: "Undo",
        hint: "Revert the latest jj operation",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |_ui, _arg| Some(Action::Undo),
        palette: PaletteScope::Hidden,
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
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg).map(|target| Action::EditDescription {
                target: Some(target),
            })
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "revision.new-child",
        label: "New child",
        hint: "Start a new change on top of this one",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|parent| Action::Mutate(mutations::MutationOp::New { parent }))
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "revision.edit",
        label: "Edit",
        hint: "Make this revision the working copy",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|target| Action::Mutate(mutations::MutationOp::Edit { target }))
        },
        palette: PaletteScope::Hidden,
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
        palette: PaletteScope::Hidden,
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
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "revision.rebase.branch.start",
        label: "Whole branch onto\u{2026}",
        hint: "Move the branch this revision sits on",
        contexts: &[Context::Base],
        chords: &["b", "B"],
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
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "revision.squash.start",
        label: "Into\u{2026}",
        hint: "Squash this revision into a picked one",
        contexts: &[Context::Base],
        chords: &["s", "S"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| draft(ui, arg, mutations::DraftKind::Squash),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "revision.merge.start",
        label: "Merge with\u{2026}",
        hint: "Create a merge of this revision and a picked one",
        contexts: &[Context::Base],
        chords: &["m", "M"],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| draft(ui, arg, mutations::DraftKind::Merge),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "revision.squash-into-parent",
        label: "Into parent",
        hint: "Squash this revision into its parent",
        contexts: &[Context::Base],
        chords: &[],
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
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "revision.duplicate",
        label: "Duplicate",
        hint: "Copy this revision alongside itself",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| {
            target_revision(ui, &arg)
                .map(|target| Action::Mutate(mutations::MutationOp::Duplicate { target }))
        },
        palette: PaletteScope::Hidden,
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
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "revision.abandon",
        label: "Abandon",
        hint: "Drop this revision, keeping its descendants",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |ui, arg| match arg {
            CommandArg::Revisions(targets) if !targets.is_empty() => {
                Some(Action::Mutate(mutations::MutationOp::Abandon { targets }))
            }
            other => target_revision(ui, &other).map(|target| {
                Action::Mutate(mutations::MutationOp::Abandon {
                    targets: vec![target],
                })
            }),
        },
        palette: PaletteScope::Hidden,
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
        palette: PaletteScope::Hidden,
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
        hint: "Delete this bookmark",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |_ui, arg| match arg {
            CommandArg::Bookmark { name, .. } => {
                Some(Action::Mutate(mutations::MutationOp::DeleteBookmark {
                    name,
                }))
            }
            _ => None,
        },
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "bookmark.track",
        label: "Track",
        hint: "Track this remote bookmark locally",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_mutate,
        build: |_ui, arg| match arg {
            CommandArg::Bookmark {
                name,
                remote: Some(remote),
                ..
            } => Some(Action::Mutate(mutations::MutationOp::TrackBookmark {
                name,
                remote,
            })),
            _ => None,
        },
        palette: PaletteScope::Hidden,
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
        palette: PaletteScope::Hidden,
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
        palette: PaletteScope::Hidden,
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
        palette: PaletteScope::Hidden,
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
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "view.diff",
        label: "Show the diff",
        hint: "Switch the main pane back to the diff",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: always,
        build: |_ui, _arg| Some(Action::SetMainView(MainView::Diff)),
        palette: PaletteScope::Hidden,
    },
    Command {
        id: "view.source",
        label: "Browse the source tree",
        hint: "Switch the main pane to the source browser",
        contexts: &[Context::Base],
        chords: &[],
        mac_chords: &[],
        enabled: can_browse,
        build: |_ui, _arg| Some(Action::SetMainView(MainView::Source)),
        palette: PaletteScope::Hidden,
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
        palette: PaletteScope::Hidden,
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
];

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

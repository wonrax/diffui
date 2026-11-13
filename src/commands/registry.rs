//! The live registry: the built-in rows of [`super::REGISTRY`] plus whatever
//! config adds, as owned entries. Built once at startup and again on reload;
//! the keymap, the menu bar's tags and the palette all read it, so a command
//! that arrives late is found everywhere at once.
//!
//! A built-in row stays a plain `static` literal ([`super::Command`]); what
//! it doesn't spell out (the palette's wording, the group it lists under, its
//! glyph, the ids it went by before) is filled in here when the entry is made.

use std::borrow::{Borrow, Cow};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use super::{Availability, Command, CommandArg, Context, PaletteScope};
use crate::Diffui;
use crate::action::Action;

/// A command's stable identity: a string, so it survives in a config file and
/// in the persisted palette recents, and owned, so commands that config
/// declares have one too. Cheap to clone.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CommandId(Arc<str>);

impl CommandId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for CommandId {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for CommandId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for CommandId {
    fn from(id: &str) -> Self {
        Self(id.into())
    }
}

impl From<String> for CommandId {
    fn from(id: String) -> Self {
        Self(id.into())
    }
}

impl PartialEq<str> for CommandId {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for CommandId {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl fmt::Debug for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a command can't run right now, as the palette and the menus show it.
/// A built-in says it in a fixed phrase; a declared command can name what it
/// is missing.
pub(crate) type Disabled = Cow<'static, str>;

/// What running a command means.
pub(crate) enum Handler {
    /// A built-in: its predicate and its action, as the static row names them.
    Builtin {
        enabled: fn(&Diffui, &CommandArg) -> Availability,
        build: fn(&Diffui, CommandArg) -> Option<Action>,
    },
}

/// One command, wherever it was declared.
pub(crate) struct Entry {
    pub(crate) id: CommandId,
    /// What a menu row says.
    pub(crate) label: Cow<'static, str>,
    /// What a palette row says, which for a revision verb names the verb
    /// rather than leaning on a submenu (`Rebase onto…`, not `Onto…`).
    pub(crate) palette_label: Cow<'static, str>,
    pub(crate) hint: Cow<'static, str>,
    /// The palette heading it lists under.
    pub(crate) group: &'static str,
    /// Its glyph in the palette.
    pub(crate) icon: &'static str,
    pub(crate) contexts: &'static [Context],
    /// Default chords for the running platform, unparsed: the keymap parses
    /// them and reports the ones that don't.
    pub(crate) chords: Vec<Cow<'static, str>>,
    pub(crate) palette: PaletteScope,
    /// Ids an older build persisted for this command.
    pub(crate) aliases: &'static [&'static str],
    pub(crate) handler: Handler,
}

impl Entry {
    fn builtin(command: &'static Command) -> Self {
        Self {
            id: command.id.into(),
            label: Cow::Borrowed(command.label),
            palette_label: Cow::Borrowed(builtin_palette_label(command)),
            hint: Cow::Borrowed(command.hint),
            group: builtin_group(command.id),
            icon: builtin_icon(command.id),
            contexts: command.contexts,
            chords: command
                .default_chords()
                .iter()
                .map(|chord| Cow::Borrowed(*chord))
                .collect(),
            palette: command.palette,
            aliases: builtin_aliases(command.id),
            handler: Handler::Builtin {
                enabled: command.enabled,
                build: command.build,
            },
        }
    }

    /// `Err` with why the command can't run on `arg` right now.
    pub(crate) fn enabled(&self, ui: &Diffui, arg: &CommandArg) -> Result<(), Disabled> {
        match &self.handler {
            Handler::Builtin { enabled, .. } => enabled(ui, arg).map_err(Cow::Borrowed),
        }
    }

    /// The action running the command on `arg` performs, or `None` when there
    /// is nothing to do.
    pub(crate) fn build(&self, ui: &Diffui, arg: CommandArg) -> Option<Action> {
        match &self.handler {
            Handler::Builtin { build, .. } => build(ui, arg),
        }
    }
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry").field("id", &self.id).finish()
    }
}

/// Every command, in palette order: the built-ins as [`super::REGISTRY`]
/// lists them, then what config added.
#[derive(Debug)]
pub(crate) struct Registry {
    entries: Vec<Entry>,
    by_id: HashMap<CommandId, usize>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Registry {
    /// The built-in commands alone.
    pub(crate) fn builtin() -> Self {
        Self::from_entries(super::REGISTRY.iter().map(Entry::builtin).collect())
    }

    fn from_entries(entries: Vec<Entry>) -> Self {
        let by_id = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id.clone(), index))
            .collect();
        Self { entries, by_id }
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Entry> {
        self.by_id.get(id).map(|index| &self.entries[*index])
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// The id an older build persisted `id` as, resolved to the command that
    /// now answers to it.
    pub(crate) fn resolve_alias(&self, id: &str) -> Option<&CommandId> {
        self.entries
            .iter()
            .find(|entry| entry.aliases.contains(&id))
            .map(|entry| &entry.id)
    }

    /// The commands the palette offers in `scope`, in registry order, each with
    /// its verdict against the arg it would run with.
    pub(crate) fn palette_commands(
        &self,
        ui: &Diffui,
        scope: PaletteScope,
        arg: &CommandArg,
    ) -> Vec<(&Entry, Result<(), Disabled>)> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.palette == scope
                    || (entry.palette == PaletteScope::RevisionVerb
                        && matches!(scope, PaletteScope::Root | PaletteScope::Revision))
            })
            .map(|entry| (entry, entry.enabled(ui, arg)))
            .collect()
    }
}

// ── What a built-in row leaves to the registry ─────────────────────────────

/// The label a palette row shows. Most commands use their menu label; the
/// revision verbs are spelled for a menu that already says what they are
/// (`Rebase ▸ Onto…`), which reads as nothing on its own in a list.
fn builtin_palette_label(command: &Command) -> &'static str {
    match command.id {
        "revision.edit" => "Edit revision",
        "revision.rebase.start" => "Rebase onto\u{2026}",
        "revision.rebase.descendants.start" => "Rebase with descendants onto\u{2026}",
        "revision.rebase.branch.start" => "Rebase whole branch onto\u{2026}",
        "revision.rebase.onto-trunk" => "Rebase onto trunk",
        "revision.squash.start" => "Squash into\u{2026}",
        "revision.squash-into-parent" => "Squash into parent",
        "revision.abandon" => "Abandon\u{2026}",
        "repo.fetch" => "Fetch all remotes",
        "bookmark.track" => "Track remote bookmark",
        "bookmark.delete" => "Delete bookmark",
        _ => command.label,
    }
}

/// A built-in's group, from its id's namespace.
fn builtin_group(id: &str) -> &'static str {
    match id.split('.').next().unwrap_or(id) {
        "repo" | "tab" | "activity" | "palette" => "Repository",
        "revision" | "selection" | "range" | "diff" => "Revisions",
        "bookmark" => "Bookmarks",
        "changes" | "file" | "lines" => "Changes",
        "conflict" => "Conflicts",
        "compare" => "Compare",
        "view" | "find" | "source" | "history" => "View",
        "theme" => "Appearance",
        _ => "App",
    }
}

/// A built-in's glyph, from what it does.
fn builtin_icon(id: &str) -> &'static str {
    use crate::icons;
    use crate::overlay_kit::glyph;
    let verb = id.rsplit('.').find(|part| *part != "start").unwrap_or(id);
    match id {
        "repo.refresh" => icons::REFRESH,
        "repo.undo" | "history.revert-operation" => icons::UNDO,
        "repo.redo" => icons::REDO,
        "repo.fetch" => icons::FETCH,
        "repo.push" | "revision.push-change" | "bookmark.push-to" => icons::UPLOAD,
        "repo.open-dialog" => icons::FOLDER_OPEN,
        "palette.command-line" => icons::TERMINAL,
        "activity.toggle" => icons::LIST_CHECKS,
        "theme.light" => glyph::SUN,
        "theme.dark" => glyph::MOON,
        "theme.system" => glyph::MONITOR,
        "theme.contrast" => glyph::EYE,
        "app.settings" => glyph::SETTINGS,
        "app.reveal-config" => glyph::FILE_TEXT,
        "app.reveal-log" => glyph::FILE_TEXT,
        "help.shortcuts" => glyph::KEYBOARD,
        "find.open" => icons::SEARCH,
        "view.diff" | "diff.since-trunk" => icons::FILE_DIFF,
        "view.source" | "revision.browse-source" | "file.open" => icons::FILE_CODE,
        "view.toggle-split" => icons::SPLIT,
        "view.toggle-wrap" => icons::WRAP,
        "view.toggle-svg-markup" => icons::CODE_XML,
        "view.toggle-commit-details" => icons::SCROLL_TEXT,
        "source.toggle-annotate" => icons::USER,
        "history.op-log" | "history.evolog" => icons::HISTORY,
        "history.restore-operation" => icons::ROTATE_CCW,
        "file.next" => icons::ARROW_DOWN,
        "file.previous" => icons::ARROW_UP,
        "file.expand-diff" => icons::UNFOLD_VERTICAL,
        "lines.toggle" | "changes.toggle-file" | "selection.toggle" => icons::LIST_CHECKS,
        "range.edit" => icons::PENCIL,
        "tab.close" | "changes.clear" => icons::CLOSE,
        _ if id.starts_with("conflict.") => icons::ALERT_TRIANGLE,
        _ if id.starts_with("compare.") => icons::GIT_COMPARE,
        _ if id.starts_with("file.history") => icons::FILE_CLOCK,
        _ if id.starts_with("bookmark.") => glyph::BOOKMARK,
        _ => match verb {
            "rebase" | "branch" | "descendants" | "onto-trunk" => icons::GIT_BRANCH,
            "squash" | "squash-into-parent" | "absorb" | "merge" | "new-merge" => icons::GIT_MERGE,
            "new" | "new-child" | "new-after" | "new-before" => icons::SQUARE_PLUS,
            "duplicate"
            | "copy-change-id"
            | "copy-commit-message"
            | "copy-author"
            | "copy-path"
            | "copy-diff" => icons::COPY,
            "edit" | "edit-description" => icons::PENCIL,
            "commit" => glyph::GIT_COMMIT,
            "split" => icons::SPLIT,
            "parallelize" => icons::COLUMNS_3,
            "restore" | "discard" => icons::ROTATE_CCW,
            "move-into" => icons::ARROW_RIGHT,
            "reset-author" | "update-author-timestamp" => icons::USER,
            "abandon" => icons::CLOSE,
            "keep" => icons::GIT_FORK,
            "select" => icons::CIRCLE,
            _ => icons::CHEVRON_RIGHT,
        },
    }
}

/// The ids a pre-registry recents file called a built-in.
///
/// Command ids used to be a `CommandId` enum with its own kebab-case persist
/// names; the registry replaced them with dotted ids. Left alone, every user's
/// most-recent commands are silently dropped the first time the new build
/// reads the file: the palette forgets what they use, once, for no reason
/// they can see.
fn builtin_aliases(id: &str) -> &'static [&'static str] {
    match id {
        "repo.refresh" => &["refresh-repository"],
        "file.next" => &["select-next-file"],
        "file.previous" => &["select-previous-file"],
        "theme.system" => &["theme-system"],
        "theme.light" => &["theme-light"],
        "theme.dark" => &["theme-dark"],
        "theme.contrast" => &["theme-high-contrast"],
        "file.copy-diff" => &["copy-file-diff"],
        "find.open" => &["open-find"],
        "revision.select" => &["jump-to-revision"],
        "revision.copy-change-id" => &["copy-change-id"],
        "revision.copy-commit-message" => &["copy-commit-message"],
        "revision.copy-author" => &["copy-author"],
        "file.open" => &["open-file"],
        "file.copy-path" => &["copy-file-path"],
        _ => &[],
    }
}

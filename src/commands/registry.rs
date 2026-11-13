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
use crate::palette::ResultRef;
use crate::user_commands::UserCommand;

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
    /// A declared command: a `[[commands]]` row of `config.toml` or of the
    /// rows diffui ships. Its when-clause and the variables it names decide
    /// whether it runs; running it is [`Action::RunUserCommand`].
    User(Arc<UserCommand>),
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

    fn user(command: Arc<UserCommand>) -> Self {
        Self {
            id: command.id.as_str().into(),
            label: Cow::Owned(command.label.clone()),
            palette_label: Cow::Owned(command.label.clone()),
            hint: Cow::Owned(command.hint.clone()),
            group: command.group,
            icon: crate::icons::TERMINAL,
            contexts: &[Context::Base],
            chords: command.keys.iter().cloned().map(Cow::Owned).collect(),
            // Declared commands say where they go with `places`.
            palette: PaletteScope::Hidden,
            aliases: &[],
            handler: Handler::User(command),
        }
    }

    /// `Err` with why the command can't run on `arg` right now.
    pub(crate) fn enabled(&self, ui: &Diffui, arg: &CommandArg) -> Result<(), Disabled> {
        match &self.handler {
            Handler::Builtin { enabled, .. } => enabled(ui, arg).map_err(Cow::Borrowed),
            Handler::User(command) => command.availability(ui, arg),
        }
    }

    /// The action running the command on `arg` performs, or `None` when there
    /// is nothing to do.
    pub(crate) fn build(&self, ui: &Diffui, arg: CommandArg) -> Option<Action> {
        match &self.handler {
            Handler::Builtin { build, .. } => build(ui, arg),
            Handler::User(command) => Some(Action::RunUserCommand {
                command: command.clone(),
                tab: ui.active_tab_id()?,
                arg,
            }),
        }
    }

    /// The declared command behind this entry, if it is one.
    pub(crate) fn declared(&self) -> Option<&Arc<UserCommand>> {
        match &self.handler {
            Handler::User(command) => Some(command),
            Handler::Builtin { .. } => None,
        }
    }

    /// Whether the palette lists it in `scope`, run on `arg`.
    fn listed_in(&self, scope: PaletteScope, arg: &CommandArg) -> bool {
        let Some(command) = self.declared() else {
            return self.palette == scope
                || (self.palette == PaletteScope::RevisionVerb
                    && matches!(scope, PaletteScope::Root | PaletteScope::Revision));
        };
        let places = command.places;
        match scope {
            PaletteScope::Root => places.palette,
            PaletteScope::Revision => match arg {
                CommandArg::Result(ResultRef::Bookmark(_)) => places.bookmark || places.revision,
                _ => places.revision,
            },
            PaletteScope::File => places.file,
            PaletteScope::Hidden | PaletteScope::RevisionVerb => false,
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

    /// The built-ins, then `shipped` (the rows diffui declares), then
    /// `declared` (config's), and what was wrong with the declarations. A
    /// declared command whose id a built-in or a shipped row already has
    /// takes its place only with `override = true`; otherwise it is left out.
    pub(crate) fn build(
        shipped: &[Arc<UserCommand>],
        declared: &[Arc<UserCommand>],
    ) -> (Self, Vec<String>) {
        let mut entries: Vec<Entry> = super::REGISTRY.iter().map(Entry::builtin).collect();
        let mut problems = Vec::new();
        let builtins = entries.len();
        for command in shipped {
            entries.push(Entry::user(command.clone()));
        }
        let fixed = entries.len();
        for command in declared {
            match entries
                .iter()
                .position(|entry| entry.id == command.id.as_str())
            {
                Some(index) if index < fixed && command.replaces => {
                    entries[index] = Entry::user(command.clone());
                }
                Some(index) if index < fixed => problems.push(format!(
                    "{}: [[commands]] {:?} is {}; set override = true to replace it",
                    command.source,
                    command.id,
                    if index < builtins {
                        "a built-in command"
                    } else {
                        "a command diffui ships"
                    }
                )),
                // `parse_commands` keeps one row per id.
                Some(_) => {}
                None => entries.push(Entry::user(command.clone())),
            }
        }
        let registry = Self::from_entries(entries);
        problems.extend(registry.chord_collisions());
        (registry, problems)
    }

    /// A declared command's key that another command in the same context
    /// already binds by default. The declared one wins (its key is bound
    /// first); this says so.
    fn chord_collisions(&self) -> Vec<String> {
        use crate::keymap::Chord;
        let mut problems = Vec::new();
        for entry in &self.entries {
            let Some(command) = entry.declared() else {
                continue;
            };
            for raw in &entry.chords {
                let Some(chord) = Chord::parse(raw) else {
                    continue;
                };
                for context in entry.contexts {
                    let other = self.entries.iter().find(|other| {
                        other.id != entry.id
                            && other.contexts.contains(context)
                            && other
                                .chords
                                .iter()
                                .any(|theirs| Chord::parse(theirs).as_ref() == Some(&chord))
                    });
                    if let Some(other) = other {
                        problems.push(format!(
                            "{}: [[commands]] {:?}: {raw} is also {}'s in {}; this one wins",
                            command.source,
                            command.id,
                            other.id,
                            context.name()
                        ));
                    }
                }
            }
        }
        problems
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
            .filter(|entry| entry.listed_in(scope, arg))
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

/// The palette's headings, in no particular order. A declared command may
/// name one of these as its group.
pub(crate) const GROUPS: &[&str] = &[
    "Repository",
    "Revisions",
    "Bookmarks",
    "Changes",
    "Conflicts",
    "Compare",
    "View",
    "Appearance",
    "App",
    "Commands",
];

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
        "app.reload-commands" => icons::REFRESH,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{Chord, Keymap, RawKeys};
    use std::path::Path;

    fn declared(toml: &str) -> Vec<Arc<UserCommand>> {
        let (commands, problems) =
            crate::user_commands::parse_commands(Path::new("config.toml"), toml);
        assert!(problems.is_empty(), "{problems:?}");
        commands.into_iter().map(Arc::new).collect()
    }

    #[test]
    fn a_declared_id_takes_a_builtin_only_with_override() {
        let (registry, problems) = Registry::build(
            &[],
            &declared(
                r#"
[[commands]]
id = "repo.refresh"
label = "Mine"
exec = ["true"]
"#,
            ),
        );
        assert!(problems[0].contains("override = true"), "{problems:?}");
        assert!(registry.get("repo.refresh").unwrap().declared().is_none());

        let (registry, problems) = Registry::build(
            &[],
            &declared(
                r#"
[[commands]]
id = "repo.refresh"
label = "Mine"
exec = ["true"]
override = true
"#,
            ),
        );
        assert!(problems.is_empty(), "{problems:?}");
        let entry = registry.get("repo.refresh").unwrap();
        assert!(entry.declared().is_some());
        assert_eq!(entry.label, "Mine");
        // It keeps the built-in's place in the list.
        assert_eq!(
            registry.iter().position(|e| e.id == "repo.refresh"),
            Registry::builtin()
                .iter()
                .position(|e| e.id == "repo.refresh")
        );
    }

    #[test]
    fn a_shipped_row_is_replaced_the_same_way() {
        let shipped = declared(
            r#"
[[commands]]
id = "file.open-in-editor"
label = "Shipped"
edit = "{path}"
"#,
        );
        let mine = declared(
            r#"
[[commands]]
id = "file.open-in-editor"
label = "Mine"
exec = ["true"]
"#,
        );
        let (registry, problems) = Registry::build(&shipped, &mine);
        assert!(problems[0].contains("ships"), "{problems:?}");
        assert_eq!(
            registry.get("file.open-in-editor").unwrap().label,
            "Shipped"
        );
    }

    /// `[keys]` naming a command config hasn't declared yet is kept, and
    /// binds once a rebuild brings the command in; a declared key wins over
    /// a default it collides with, and says so.
    #[test]
    fn a_rebuild_picks_up_keys_for_late_commands() {
        let keys: RawKeys = toml::from_str(
            r#""user.late" = "cmd+shift+l"
"#,
        )
        .expect("parses");
        let chord = Chord::parse("cmd+shift+l").expect("a chord");
        let (map, problems) = Keymap::build(&Registry::builtin(), &keys);
        assert!(problems[0].contains("kept"), "{problems:?}");
        assert_eq!(map.resolve(Context::Base, &chord), None);

        let late = declared(
            r#"
[[commands]]
id = "user.late"
label = "Late"
keys = ["cmd+k"]
exec = ["true"]
"#,
        );
        let (registry, problems) = Registry::build(&[], &late);
        assert!(problems[0].contains("palette.toggle"), "{problems:?}");
        let (map, problems) = Keymap::build(&registry, &keys);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            map.resolve(Context::Base, &chord).as_deref(),
            Some("user.late")
        );
        // The override replaced its own `cmd+k`, so the palette keeps it.
        let palette = Chord::parse("cmd+k").expect("a chord");
        assert_eq!(
            map.resolve(Context::Base, &palette).as_deref(),
            Some("palette.toggle")
        );

        let (registry, _) = Registry::build(&[], &late);
        let (map, _) = Keymap::build(&registry, &RawKeys::default());
        assert_eq!(
            map.resolve(Context::Base, &palette).as_deref(),
            Some("user.late"),
            "a declared key goes in front of the default it shadows"
        );
    }

    #[test]
    fn recents_for_a_late_command_rank_once_it_loads() {
        let mut recents = crate::palette::Recents::default();
        recents.push_command("user.late".into());
        let (registry, _) = Registry::build(
            &[],
            &declared(
                r#"
[[commands]]
id = "user.late"
label = "Late"
exec = ["true"]
"#,
            ),
        );
        assert!(registry.get("user.late").is_some());
        assert!(recents.command_bonus("user.late") > 0);
    }
}

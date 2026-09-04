//! Chords, and the table that maps one to a command id per context.
//!
//! Defaults come from the registry ([`crate::commands::REGISTRY`]); the
//! `[keys]` section of `config.toml` overrides them. Nothing here is fatal: an
//! unparsable chord or an unknown command id is collected as a diagnostic,
//! reported once at startup, and skipped, so a config written against a newer
//! build still opens the app.
//!
//! ## Chord syntax
//!
//! `modifier+…+key`, e.g. `cmd+k`, `shift+enter`, `alt+z`, `j`.
//!
//!   * modifiers — `cmd` (`command`/`super`/`meta`; ⌘ on macOS, Ctrl
//!     elsewhere), `ctrl` (`control`), `alt` (`opt`/`option`), `shift`
//!   * keys — a single character, or one of `enter`, `esc`, `tab`, `space`,
//!     `backspace`, `delete`, `up`, `down`, `left`, `right`, `home`, `end`,
//!     `pageup`, `pagedown`, `insert`, `f1`–`f12`
//!
//! `shift` is only meaningful on a *named* key: the platform folds it into the
//! character otherwise, which is why `r` and `R` are two different chords
//! rather than one chord and a modifier. A lowercase character chord also
//! matches its uppercase form (so `cmd+k` fires with caps lock on); an
//! uppercase one matches only the shifted key.

use std::collections::BTreeMap;

use iced::keyboard::{self, key::Named};
use serde::Deserialize;

use crate::commands::{CommandId, Context, REGISTRY, command};

/// The modifier state a chord requires.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct Mods {
    pub(crate) command: bool,
    pub(crate) alt: bool,
    pub(crate) control: bool,
    pub(crate) shift: bool,
}

impl Mods {
    fn from_modifiers(modifiers: keyboard::Modifiers) -> Self {
        Self {
            command: modifiers.command(),
            alt: modifiers.alt(),
            control: modifiers.control(),
            shift: modifiers.shift(),
        }
        .normalized()
    }

    /// Off macOS, `cmd` *is* ctrl — iced's `Modifiers::COMMAND` is the ctrl
    /// bit there, so a raw Ctrl press sets both flags and would never match a
    /// chord that set only one. Fold them into `command` so `cmd+k` and
    /// `ctrl+k` name one chord off macOS and stay two on it.
    fn normalized(mut self) -> Self {
        if !cfg!(target_os = "macos") && (self.command || self.control) {
            self.command = true;
            self.control = false;
        }
        self
    }

    /// Whether any of ⌘/⌥/⌃ is held. Plain (unmodified) chords are the ones a
    /// focused text input must be allowed to keep, and the ones target mode
    /// swallows rather than passing down to the base context.
    pub(crate) fn any_command_like(self) -> bool {
        self.command || self.alt || self.control
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ChordKey {
    /// A character as the platform reports it, unmodified except by shift.
    Char(String),
    Named(Named),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Chord {
    pub(crate) key: ChordKey,
    pub(crate) mods: Mods,
}

impl Chord {
    /// Parse one chord, or `None` when it names no key we can match.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        // `+` is both the separator and a key. A trailing one is therefore the
        // key itself (`+`, `cmd++`), not an empty final segment.
        let (modifiers, key) = match raw.rsplit_once('+') {
            Some((head, "")) => (head.trim_end_matches('+'), "+"),
            Some((head, key)) => (head, key),
            None => ("", raw),
        };
        let mut mods = Mods::default();
        for part in modifiers.split('+').filter(|part| !part.trim().is_empty()) {
            match part.trim().to_ascii_lowercase().as_str() {
                "cmd" | "command" | "super" | "meta" => mods.command = true,
                "ctrl" | "control" => mods.control = true,
                "alt" | "opt" | "option" => mods.alt = true,
                "shift" => mods.shift = true,
                _ => return None,
            }
        }
        let key = match named_key(key) {
            Some(named) => ChordKey::Named(named),
            None if key.chars().count() == 1 => ChordKey::Char(key.to_owned()),
            None => return None,
        };
        // Shift on a character key is already reflected in the character, so
        // accepting it would build a chord that can never match.
        if matches!(key, ChordKey::Char(_)) {
            mods.shift = false;
        }
        Some(Self {
            key,
            mods: mods.normalized(),
        })
    }

    /// The chord a key event carries, or `None` for keys we never bind
    /// (modifiers on their own, dead keys).
    pub(crate) fn from_event(key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Option<Self> {
        let mut mods = Mods::from_modifiers(modifiers);
        let key = match key.as_ref() {
            keyboard::Key::Character(c) => {
                mods.shift = false;
                ChordKey::Char(c.to_owned())
            }
            keyboard::Key::Named(named) => ChordKey::Named(named),
            keyboard::Key::Unidentified => return None,
        };
        Some(Self { key, mods })
    }

    /// The same chord with its character folded to lower case, or `None` when
    /// that changes nothing. Resolution falls back to this so `cmd+k` fires
    /// for a shifted or caps-locked `K`.
    fn lowercased(&self) -> Option<Self> {
        let ChordKey::Char(c) = &self.key else {
            return None;
        };
        let lower: String = c.chars().flat_map(char::to_lowercase).collect();
        (lower != *c).then_some(Self {
            key: ChordKey::Char(lower),
            mods: self.mods,
        })
    }

    /// How this chord reads in a menu row — `⌘K` on macOS, `Ctrl+K` elsewhere.
    pub(crate) fn display(&self) -> String {
        // A character chord keeps shift *inside* the character (`R` is the
        // shifted `r`), so `mods.shift` is always clear on one. The key itself
        // then renders upper-case either way, and the Rebase submenu would
        // print the same hint against `Onto…` and `With descendants onto…` for
        // two different keys. Read the shift back off the character.
        let shifted = match &self.key {
            ChordKey::Char(c) => self.mods.shift || is_shifted(c),
            ChordKey::Named(_) => self.mods.shift,
        };
        let mut out = String::new();
        if cfg!(target_os = "macos") {
            if self.mods.control {
                out.push('\u{2303}');
            }
            if self.mods.alt {
                out.push('\u{2325}');
            }
            if shifted {
                out.push('\u{21e7}');
            }
            if self.mods.command {
                out.push('\u{2318}');
            }
        } else {
            for (held, name) in [
                (self.mods.control || self.mods.command, "Ctrl"),
                (self.mods.alt, "Alt"),
                (shifted, "Shift"),
            ] {
                if held {
                    out.push_str(name);
                    out.push('+');
                }
            }
        }
        match &self.key {
            ChordKey::Char(c) => out.push_str(&c.to_uppercase()),
            ChordKey::Named(named) => out.push_str(named_label(*named)),
        }
        out
    }
}

/// Whether a character is the shifted form of another one, i.e. an upper-case
/// letter. Anything that lower-cases to itself (a digit, `+`, `ω`) is not.
fn is_shifted(c: &str) -> bool {
    c.chars().flat_map(char::to_lowercase).ne(c.chars())
}

fn named_key(name: &str) -> Option<Named> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "enter" | "return" => Named::Enter,
        "esc" | "escape" => Named::Escape,
        "tab" => Named::Tab,
        "space" => Named::Space,
        "backspace" => Named::Backspace,
        "delete" | "del" => Named::Delete,
        "up" => Named::ArrowUp,
        "down" => Named::ArrowDown,
        "left" => Named::ArrowLeft,
        "right" => Named::ArrowRight,
        "home" => Named::Home,
        "end" => Named::End,
        "pageup" => Named::PageUp,
        "pagedown" => Named::PageDown,
        "insert" => Named::Insert,
        "f1" => Named::F1,
        "f2" => Named::F2,
        "f3" => Named::F3,
        "f4" => Named::F4,
        "f5" => Named::F5,
        "f6" => Named::F6,
        "f7" => Named::F7,
        "f8" => Named::F8,
        "f9" => Named::F9,
        "f10" => Named::F10,
        "f11" => Named::F11,
        "f12" => Named::F12,
        _ => return None,
    })
}

fn named_label(named: Named) -> &'static str {
    match named {
        Named::Enter => "\u{21a9}",
        Named::Escape => "Esc",
        Named::Tab => "\u{21e5}",
        Named::Space => "Space",
        Named::Backspace => "\u{232b}",
        Named::ArrowUp => "\u{2191}",
        Named::ArrowDown => "\u{2193}",
        Named::ArrowLeft => "\u{2190}",
        Named::ArrowRight => "\u{2192}",
        _ => "",
    }
}

/// Chord → command id, per context.
#[derive(Debug, Clone, Default)]
pub(crate) struct Keymap {
    /// One flat list per context. These hold a couple of dozen entries at
    /// most, so a scan beats the hashing a map would do on every keystroke.
    bindings: Vec<(Context, Chord, CommandId)>,
}

impl Keymap {
    /// The registry's defaults for the running platform, with `overrides`
    /// applied on top. Diagnostics name what was skipped and why.
    pub(crate) fn build(overrides: &RawKeys) -> (Self, Vec<String>) {
        let mut map = Keymap::default();
        let mut problems = Vec::new();
        for command in REGISTRY {
            for raw in command.default_chords() {
                let Some(chord) = Chord::parse(raw) else {
                    // Guarded by a unit test, so reaching this means the
                    // registry and the parser disagree — say so rather than
                    // dropping the binding silently.
                    problems.push(format!(
                        "built-in chord {raw:?} for {} does not parse",
                        command.id
                    ));
                    continue;
                };
                for context in command.contexts {
                    map.bindings.push((*context, chord.clone(), command.id));
                }
            }
        }
        map.apply(overrides, &mut problems);
        (map, problems)
    }

    fn apply(&mut self, overrides: &RawKeys, problems: &mut Vec<String>) {
        for (key, entry) in &overrides.0 {
            match entry {
                RawKeyEntry::Chords(spec) => match command(key) {
                    Some(command) => {
                        for context in command.contexts {
                            self.rebind(*context, command.id, spec, problems);
                        }
                    }
                    None => problems.push(format!("[keys] {key:?}: unknown command")),
                },
                RawKeyEntry::Table(table) => match Context::parse(key) {
                    Some(context) => {
                        for (id, spec) in table {
                            match command(id) {
                                Some(command) => self.rebind(context, command.id, spec, problems),
                                None => {
                                    problems.push(format!("[keys.{key}] {id:?}: unknown command"))
                                }
                            }
                        }
                    }
                    None => problems.push(format!("[keys.{key}]: unknown context")),
                },
            }
        }
    }

    /// Replace every binding `id` holds in `context` with `spec`. An override
    /// is a replacement, not an addition, so a rebind can't leave the default
    /// chord live alongside the new one.
    fn rebind(
        &mut self,
        context: Context,
        id: CommandId,
        spec: &ChordSpec,
        problems: &mut Vec<String>,
    ) {
        self.bindings
            .retain(|(bound, _, bound_id)| *bound != context || *bound_id != id);
        for raw in spec.chords() {
            if raw.trim().eq_ignore_ascii_case("none") {
                continue;
            }
            match Chord::parse(raw) {
                Some(chord) => self.bindings.push((context, chord, id)),
                None => problems.push(format!(
                    "[keys.{}] {id:?}: {raw:?} is not a chord",
                    context.name()
                )),
            }
        }
    }

    /// The command `chord` runs in `context`, if any.
    pub(crate) fn resolve(&self, context: Context, chord: &Chord) -> Option<CommandId> {
        self.lookup(context, chord).or_else(|| {
            chord
                .lowercased()
                .and_then(|lower| self.lookup(context, &lower))
        })
    }

    fn lookup(&self, context: Context, chord: &Chord) -> Option<CommandId> {
        self.bindings
            .iter()
            .find(|(bound, bound_chord, _)| *bound == context && bound_chord == chord)
            .map(|(_, _, id)| *id)
    }

    /// The first chord bound to `id` in `context` — what a menu row shows as
    /// its hint.
    pub(crate) fn chord_for(&self, context: Context, id: CommandId) -> Option<&Chord> {
        self.bindings
            .iter()
            .find(|(bound, _, bound_id)| *bound == context && *bound_id == id)
            .map(|(_, chord, _)| chord)
    }
}

/// The `[keys]` section as it appears on disk.
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct RawKeys(pub BTreeMap<String, RawKeyEntry>);

/// One `[keys]` entry: either a command id bound directly (applying to every
/// context that command is valid in), or a context name introducing a table of
/// command ids.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RawKeyEntry {
    Chords(ChordSpec),
    Table(BTreeMap<String, ChordSpec>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ChordSpec {
    One(String),
    Many(Vec<String>),
}

impl ChordSpec {
    fn chords(&self) -> &[String] {
        match self {
            ChordSpec::One(chord) => std::slice::from_ref(chord),
            ChordSpec::Many(chords) => chords,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a whole config fixture and hand back its `[keys]` section, the
    /// way `AppConfig::load` does.
    fn keys(toml: &str) -> RawKeys {
        #[derive(Deserialize)]
        struct Fixture {
            #[serde(default)]
            keys: RawKeys,
        }
        toml::from_str::<Fixture>(toml)
            .expect("the fixture parses")
            .keys
    }

    fn chord(raw: &str) -> Chord {
        Chord::parse(raw).expect("a parsable chord")
    }

    #[test]
    fn chord_syntax() {
        assert_eq!(
            chord("cmd+k"),
            Chord {
                key: ChordKey::Char("k".to_owned()),
                mods: Mods {
                    command: true,
                    ..Mods::default()
                },
            }
        );
        assert_eq!(
            chord("shift+enter"),
            Chord {
                key: ChordKey::Named(Named::Enter),
                mods: Mods {
                    shift: true,
                    ..Mods::default()
                },
            }
        );
        assert_eq!(chord("j").mods, Mods::default());
        // Modifier spelling and case are both forgiving.
        assert_eq!(chord("Option+Z"), chord("alt+Z"));
        // `ctrl` and `control` are two spellings of one modifier.
        assert_eq!(chord("ctrl+home"), chord("control+home"));
        // Shift on a character is already in the character, so it is dropped
        // rather than building a chord no event can match.
        assert!(!chord("shift+r").mods.shift);
        // Nonsense is rejected, never guessed at.
        assert_eq!(chord("+").key, ChordKey::Char("+".to_owned()));
        assert_eq!(
            chord("cmd++"),
            Chord {
                key: ChordKey::Char("+".to_owned()),
                mods: Mods {
                    command: true,
                    ..Mods::default()
                }
            }
        );
        assert!(Chord::parse("").is_none());
        assert!(Chord::parse("hyper+k").is_none());
        assert!(Chord::parse("cmd+notakey").is_none());
    }

    #[test]
    fn a_character_chord_ignores_shift_but_not_the_others() {
        let map = Keymap::build(&RawKeys::default()).0;
        let k = keyboard::Key::Character("k".into());
        let event = |modifiers| Chord::from_event(&k, modifiers).expect("a bindable key");
        // ⌘⇧K is still ⌘K: shift lives in the character, not the modifier set.
        assert_eq!(
            map.resolve(
                Context::Base,
                &event(keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT)
            ),
            Some("palette.toggle")
        );
        // ⌥ is not shift — it makes a different chord, bound to nothing.
        assert_eq!(
            map.resolve(
                Context::Base,
                &event(keyboard::Modifiers::COMMAND | keyboard::Modifiers::ALT)
            ),
            None
        );
    }

    #[test]
    fn shift_does_distinguish_a_named_key() {
        let map = Keymap::build(&RawKeys::default()).0;
        assert_eq!(
            map.resolve(Context::Find, &chord("enter")),
            Some("find.next")
        );
        assert_eq!(
            map.resolve(Context::Find, &chord("shift+enter")),
            Some("find.previous")
        );
    }

    #[test]
    fn defaults_resolve_per_context() {
        let (map, problems) = Keymap::build(&RawKeys::default());
        assert!(problems.is_empty(), "{problems:?}");
        // The same physical key means different things in different contexts.
        assert_eq!(map.resolve(Context::Base, &chord("j")), Some("file.next"));
        assert_eq!(
            map.resolve(Context::Draft, &chord("j")),
            Some("draft.candidate.next")
        );
        assert_eq!(
            map.resolve(Context::Base, &chord("esc")),
            Some("selection.clear")
        );
        assert_eq!(
            map.resolve(Context::Confirm, &chord("esc")),
            Some("confirm.cancel")
        );
        // A context a command doesn't list never resolves it.
        assert_eq!(map.resolve(Context::Confirm, &chord("j")), None);
        assert_eq!(map.resolve(Context::Base, &chord("tab")), None);
    }

    #[test]
    fn an_uppercase_character_is_its_own_chord() {
        let map = Keymap::build(&RawKeys::default()).0;
        assert_eq!(
            map.resolve(Context::Base, &chord("r")),
            Some("revision.rebase.start")
        );
        assert_eq!(
            map.resolve(Context::Base, &chord("R")),
            Some("revision.rebase.descendants.start")
        );
        // A lowercase binding still catches the shifted key when nothing
        // claims the uppercase form — that is how ⌘⇧K stays ⌘K.
        assert_eq!(
            map.resolve(Context::Base, &chord("cmd+K")),
            Some("palette.toggle")
        );
    }

    #[test]
    fn a_context_table_overrides_only_that_context() {
        let (map, problems) = Keymap::build(&keys(
            r#"
            [keys.base]
            "file.next" = "n"
            "#,
        ));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(map.resolve(Context::Base, &chord("n")), Some("file.next"));
        // The override replaces the defaults rather than piling on.
        assert_eq!(map.resolve(Context::Base, &chord("j")), None);
        // Target mode's own `j` is untouched.
        assert_eq!(
            map.resolve(Context::Draft, &chord("j")),
            Some("draft.candidate.next")
        );
    }

    #[test]
    fn a_bare_command_id_rebinds_every_context_it_is_valid_in() {
        let (map, problems) = Keymap::build(&keys(
            r#"
            [keys]
            "palette.toggle" = ["cmd+p", "f1"]
            "#,
        ));
        assert!(problems.is_empty(), "{problems:?}");
        for context in [Context::Base, Context::Palette, Context::Find] {
            assert_eq!(
                map.resolve(context, &chord("cmd+p")),
                Some("palette.toggle")
            );
            assert_eq!(map.resolve(context, &chord("f1")), Some("palette.toggle"));
            assert_eq!(map.resolve(context, &chord("cmd+k")), None);
        }
    }

    #[test]
    fn none_unbinds() {
        let (map, problems) = Keymap::build(&keys(
            r#"
            [keys.base]
            "selection.clear" = "none"
            "#,
        ));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(map.resolve(Context::Base, &chord("esc")), None);
        // Everything else in the context is left alone.
        assert_eq!(map.resolve(Context::Base, &chord("j")), Some("file.next"));
    }

    #[test]
    fn a_bad_entry_is_reported_and_skipped() {
        let (map, problems) = Keymap::build(&keys(
            r#"
            [keys]
            "no.such.command" = "cmd+q"

            [keys.base]
            "file.next" = "not a chord"

            [keys.nowhere]
            "file.next" = "n"
            "#,
        ));
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("no.such.command")));
        assert!(problems.iter().any(|p| p.contains("not a chord")));
        assert!(problems.iter().any(|p| p.contains("nowhere")));
        // The bad chord still cleared the default it was replacing — the user
        // asked for `file.next` to move, and half-applying would be worse than
        // either outcome.
        assert_eq!(map.resolve(Context::Base, &chord("j")), None);
        // Unrelated bindings survive a bad file.
        assert_eq!(
            map.resolve(Context::Base, &chord("cmd+k")),
            Some("palette.toggle")
        );
    }

    #[test]
    fn chords_render_for_menu_hints() {
        if cfg!(target_os = "macos") {
            assert_eq!(chord("cmd+k").display(), "\u{2318}K");
        } else {
            assert_eq!(chord("cmd+k").display(), "Ctrl+K");
        }
        assert_eq!(chord("r").display(), "R");
    }

    /// `r` and `R` start two different rebases and sit next to each other in
    /// the Rebase submenu, so their hints have to read apart.
    #[test]
    fn an_uppercase_character_renders_as_a_shifted_chord() {
        let (plain, shifted) = (chord("r").display(), chord("R").display());
        assert_ne!(plain, shifted);
        if cfg!(target_os = "macos") {
            assert_eq!(shifted, "\u{21e7}R");
            assert_eq!(chord("cmd+R").display(), "\u{21e7}\u{2318}R");
        } else {
            assert_eq!(shifted, "Shift+R");
            assert_eq!(chord("cmd+R").display(), "Ctrl+Shift+R");
        }
        // Only a letter has a shifted form; a digit or a symbol reads bare.
        assert_eq!(chord("1").display(), "1");
        assert_eq!(chord("+").display(), "+");
    }
}

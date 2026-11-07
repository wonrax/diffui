//! The keyboard-shortcut cheat sheet: `?` (or the palette's "Keyboard
//! shortcuts") lays every live binding out over the window, grouped by the
//! context it works in.
//!
//! It reads the built keymap rather than the registry's defaults, so a
//! `[keys]` override shows up here as bound, and a default it shadowed does
//! not.

use crate::Cx;
use crate::Diffui;
use crate::commands::{self, CommandArg, CommandId, Context, REGISTRY};
use crate::modes::{Mode, ModeKind};
use crate::theme::ThemeSpec;
use crate::typography::Aligned;
use gpui_kit::{AnyElement, Window};

const CARD_WIDTH: f32 = 760.0;
const CARD_MAX_HEIGHT: f32 = 720.0;
const ROW_HEIGHT: f32 = 24.0;

/// The sections after the base context's groups, in the order the sheet
/// lists them. The popup-menu context only exists off macOS and holds nothing
/// but Esc, so it is left out.
const SECTIONS: &[(Context, &str)] = &[
    (
        Context::Draft,
        "Target mode (rebase / squash / merge / new / duplicate)",
    ),
    (Context::Lines, "Line selection"),
    (Context::Palette, "Command palette"),
    (Context::Find, "Find in diff"),
    (Context::Description, "Description editor"),
    (Context::Split, "Split prompt"),
    (Context::ConflictEdit, "Conflict marker editor"),
    (Context::OpenRepo, "Open repository"),
    (Context::Confirm, "Confirmation"),
    (Context::ActivityPopover, "Activity log"),
];

/// The base context is one keyboard for the graph and the diff pane, so its
/// keys never collide; the sheet still splits them by what they act on, by
/// command-id prefix. Anything unmatched lands in "General".
const BASE_GROUPS: &[(&str, &[&str])] = &[
    (
        "Revisions",
        &["revision.", "selection.", "bookmark.", "compare."],
    ),
    (
        "Diff pane and files",
        &[
            "file.",
            "changes.",
            "lines.",
            "conflict.",
            "find.",
            "view.",
            "source.",
        ],
    ),
    ("Repository and history", &["repo.", "history."]),
];

/// Which base group `id` is listed under.
fn base_group(id: CommandId) -> &'static str {
    BASE_GROUPS
        .iter()
        .find(|(_, prefixes)| prefixes.iter().any(|prefix| id.starts_with(prefix)))
        .map_or("General", |(title, _)| title)
}

/// Pointer gestures, which have no binding to read but are just as hidden.
const GESTURES: &[(&str, &str)] = &[
    ("Right-click a revision", "Revision actions"),
    (
        "Drag a revision",
        "Rebase it (hold \u{2325} for descendants)",
    ),
    (
        "\u{2318}-click / \u{21e7}-click",
        "Mark revisions for a batch action",
    ),
    ("Right-click a file", "File actions"),
];

/// One row of the sheet: what the binding does and the keys that run it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub(crate) id: CommandId,
    pub(crate) label: String,
    pub(crate) keys: Vec<String>,
    /// Why the command can't run right now (base context only: the other
    /// contexts' keys only exist inside their mode).
    pub(crate) disabled: Option<commands::Reason>,
}

/// The live bindings of `context`, in registry order. The nine `⌘N` tab
/// jumps fold into one row.
pub(crate) fn entries(ui: &Diffui, context: Context) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut tab_jumps: Vec<String> = Vec::new();
    for command in REGISTRY {
        if !command.contexts.contains(&context) {
            continue;
        }
        let keys: Vec<String> = ui
            .keymap
            .live_chords(context, command.id)
            .into_iter()
            // macOS reports ⌥Z as the `Ω` it composes to, so the registry
            // binds both; only the key the user presses is worth listing.
            .filter(|chord| !is_composed(chord))
            .map(|chord| chord.display())
            .collect();
        if keys.is_empty() {
            continue;
        }
        if command.id.starts_with("tab.select-") {
            tab_jumps.extend(keys);
            continue;
        }
        let disabled = (context == Context::Base)
            .then(|| (command.enabled)(ui, &CommandArg::None).err())
            .flatten();
        out.push(Entry {
            id: command.id,
            label: label(command.id),
            keys,
            disabled,
        });
    }
    if let (Some(first), Some(last)) = (tab_jumps.first(), tab_jumps.last()) {
        let keys = if tab_jumps.len() > 1 {
            vec![format!("{first}\u{2013}{last}")]
        } else {
            vec![first.clone()]
        };
        out.push(Entry {
            id: "tab.select",
            label: "Go to tab 1\u{2013}9".to_owned(),
            keys,
            disabled: None,
        });
    }
    out
}

/// An ⌥ chord on a non-ASCII character: the platform's composed form of
/// another binding rather than a key anyone types.
fn is_composed(chord: &crate::keymap::Chord) -> bool {
    chord.mods.alt && matches!(&chord.key, crate::keymap::ChordKey::Char(c) if !c.is_ascii())
}

fn label(id: CommandId) -> String {
    commands::command(id)
        .map(|command| {
            command
                .palette_label()
                .trim_end_matches('\u{2026}')
                .to_owned()
        })
        .unwrap_or_default()
}

impl Diffui {
    pub(crate) fn toggle_shortcuts(&mut self, cx: &mut Cx) {
        if self.pop_mode(ModeKind::Shortcuts).is_none() {
            // It replaces the transient overlays rather than stacking over
            // them: they share one keyboard.
            self.pop_mode(ModeKind::Palette);
            self.pop_mode(ModeKind::Menu);
            self.pop_mode(ModeKind::ActivityPopover);
            self.push_mode(Mode::Shortcuts);
        }
    }
}

/// Stub: the shortcuts view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::overlay(
        "shortcuts::build_overlay",
        ui.mode(ModeKind::Shortcuts).is_some(),
    )
}

/// Stub: the shortcuts view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn keycap(
    ui: &Diffui,
    theme: ThemeSpec,
    key: String,
    window: &mut Window,
    cx: &mut Cx,
) -> Aligned {
    crate::stub::aligned("shortcuts::keycap")
}

/// Stub: the shortcuts view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn welcome_hints(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("shortcuts::welcome_hints")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sheet_lists_live_bindings_with_their_keys() {
        let ui = crate::tabs::tests::app();
        let base = entries(&ui, Context::Base);
        let find = |label: &str| {
            base.iter()
                .find(|entry| entry.label == label)
                .unwrap_or_else(|| panic!("{label} missing: {base:?}"))
        };
        assert_eq!(find("Select next file").keys, vec!["j", "\u{2193}"]);
        assert_eq!(find("Keyboard shortcuts").keys, vec!["?"]);
        // The nine tab jumps are one row.
        assert_eq!(
            base.iter()
                .filter(|entry| entry.label.starts_with("Go to tab"))
                .count(),
            1
        );
        // A command with no key in the context is not listed.
        assert!(
            base.iter()
                .all(|entry| entry.label != "Absorb into ancestors")
        );
    }

    #[test]
    fn an_override_moves_the_key_and_drops_the_shadowed_default() {
        let mut ui = crate::tabs::tests::app();
        let raw: crate::keymap::RawKeys =
            toml::from_str("[base]\n\"file.next\" = \"n\"").expect("parses");
        ui.keymap = crate::keymap::Keymap::build(&raw).0;
        let base = entries(&ui, Context::Base);
        let keys_of = |label: &str| {
            base.iter()
                .find(|entry| entry.label == label)
                .map(|entry| entry.keys.clone())
        };
        assert_eq!(keys_of("Select next file"), Some(vec!["n".to_owned()]));
        // `n` now selects the next file, so "New child" has no live key.
        assert_eq!(keys_of("New child"), None);
    }

    #[test]
    fn base_keys_group_by_what_they_act_on() {
        assert_eq!(base_group("revision.commit"), "Revisions");
        assert_eq!(base_group("compare.with"), "Revisions");
        assert_eq!(base_group("changes.split"), "Diff pane and files");
        assert_eq!(base_group("history.op-log"), "Repository and history");
        assert_eq!(base_group("palette.toggle"), "General");
        assert_eq!(base_group("tab.select"), "General");
    }
}

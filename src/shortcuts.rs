//! The keyboard-shortcut cheat sheet: `?` (or the palette's "Keyboard
//! shortcuts") lays every live binding out over the window, grouped by the
//! context it works in.
//!
//! It reads the built keymap rather than the registry's defaults, so a
//! `[keys]` override shows up here as bound, and a default it shadowed does
//! not.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use crate::commands::{self, CommandArg, CommandId, Context, REGISTRY};
use crate::modes::{Mode, ModeKind};
use crate::theme::{self, ThemeSpec, Weight, emphasis_font, text_size};
use crate::typography::{self, Aligned};
use crate::{Action, Cx, Diffui, ui};

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
    pub(crate) fn toggle_shortcuts(&mut self) {
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

/// The sheet over a scrim, or nothing when it is closed.
pub(crate) fn build_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if ui.mode(ModeKind::Shortcuts).is_none() {
        return div().into_any_element();
    }

    let column = || {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(px(theme::space::XL))
    };
    let (mut left, mut right) = (column(), column());
    let rows_of = |entries: Vec<Entry>, window: &mut Window, cx: &mut Cx| -> Vec<AnyElement> {
        entries
            .into_iter()
            .map(|entry| {
                binding_row(
                    ui,
                    theme,
                    entry.label,
                    entry.keys,
                    entry.disabled,
                    window,
                    cx,
                )
            })
            .collect()
    };
    // Revisions is the long one: it and the repository keys go left, the rest
    // balances against them on the right.
    let base = entries(ui, Context::Base);
    for title in [
        "General",
        "Revisions",
        "Diff pane and files",
        "Repository and history",
    ] {
        let rows: Vec<Entry> = base
            .iter()
            .filter(|entry| base_group(entry.id) == title)
            .cloned()
            .collect();
        if rows.is_empty() {
            continue;
        }
        let block = section(ui, theme, title, rows_of(rows, window, cx));
        if matches!(title, "Revisions" | "Repository and history") {
            left = left.child(block);
        } else {
            right = right.child(block);
        }
    }
    for (context, title) in SECTIONS {
        let rows = entries(ui, *context);
        if rows.is_empty() {
            continue;
        }
        right = right.child(section(ui, theme, title, rows_of(rows, window, cx)));
    }
    let gestures = GESTURES
        .iter()
        .map(|(gesture, what)| {
            binding_row(
                ui,
                theme,
                (*what).to_owned(),
                vec![(*gesture).to_owned()],
                None,
                window,
                cx,
            )
        })
        .collect();
    let left = left.child(section(ui, theme, "Mouse", gestures));

    let font = ui.config.ui_font;
    let title_font = emphasis_font(font, Weight::MEDIUM);
    let header = div()
        .flex_none()
        .pr(px(ui::DIALOG_PADDING))
        .child(typography::baseline_row(
            [
                Aligned::text(
                    typography::line(
                        "Keyboard shortcuts",
                        title_font,
                        text_size::TITLE,
                        theme.text,
                    ),
                    title_font,
                    text_size::TITLE,
                )
                .fill(),
                Aligned::text(
                    typography::line(
                        "Rebind in config.toml [keys]   \u{b7}   esc to close",
                        font,
                        text_size::CAPTION,
                        theme.subtle_text,
                    ),
                    font,
                    text_size::CAPTION,
                ),
            ],
            0.0,
        ));

    let columns = div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(theme::space::XXL * 1.5))
        // The list scrolls out to the card's right edge, so the bar sits where
        // every other scroll view's does; the inset moves inside.
        .pr(px(ui::DIALOG_PADDING))
        .child(left)
        .child(right);

    let height = (ui.window_size.height - crate::toolbar::modal_top() - theme::space::XXL)
        .clamp(240.0, CARD_MAX_HEIGHT);
    let card = ui::dialog_card(CARD_WIDTH, theme)
        .h(px(height))
        .pr(px(0.0))
        .gap(px(theme::space::XL))
        .child(header)
        .child(
            div()
                .flex_1()
                .min_h_0()
                .child(ui::scrollable("shortcuts-list", columns)),
        );
    let dismiss = Diffui::on_action::<()>(Action::CloseShortcuts, cx);
    ui::modal(
        "shortcuts-sheet",
        card,
        crate::toolbar::modal_top(),
        move |window, cx| dismiss(&(), window, cx),
    )
    .into_any_element()
}

fn section(
    ui: &Diffui,
    theme: ThemeSpec,
    title: &'static str,
    rows: Vec<AnyElement>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .child(div().pb(px(theme::space::XS)).child(typography::line(
            title,
            emphasis_font(ui.config.ui_font, Weight::SEMIBOLD),
            text_size::CAPTION,
            theme.subtle_text,
        )))
        .children(rows)
}

fn binding_row(
    ui: &Diffui,
    theme: ThemeSpec,
    label: String,
    keys: Vec<String>,
    disabled: Option<commands::Reason>,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let font = ui.config.ui_font;
    // The label and the mono keycaps share a baseline (see `crate::typography`). A
    // command that can't run right now greys out and says why after its label.
    let mut line = match disabled {
        None => vec![
            Aligned::text(
                typography::line(label, font, text_size::UI, theme.text).text_ellipsis(),
                font,
                text_size::UI,
            )
            .fill(),
        ],
        Some(reason) => vec![
            Aligned::text(
                typography::line(label, font, text_size::UI, theme.subtle_text).text_ellipsis(),
                font,
                text_size::UI,
            ),
            Aligned::gap(theme::space::SM),
            Aligned::text(
                typography::line(reason, font, text_size::CAPTION, theme.subtle_text)
                    .text_ellipsis(),
                font,
                text_size::CAPTION,
            )
            .fill(),
        ],
    };
    for (index, key) in keys.into_iter().enumerate() {
        line.push(Aligned::gap(if index == 0 {
            theme::space::MD
        } else {
            theme::space::XS
        }));
        line.push(keycap(ui, theme, key, window, cx));
    }
    div()
        .flex()
        .items_center()
        .w_full()
        .h(px(ROW_HEIGHT))
        .child(typography::baseline_row(line, 0.0))
        .into_any_element()
}

/// A key rendered like the tab strip's `⌘K` control: hairline border, mono.
pub(crate) fn keycap(
    ui: &Diffui,
    theme: ThemeSpec,
    key: String,
    _window: &mut Window,
    _cx: &mut Cx,
) -> Aligned {
    let mono = ui.config.mono_font;
    Aligned::chip(
        typography::line(key, mono, text_size::CAPTION, theme.muted_text),
        mono,
        text_size::CAPTION,
        [1.0, 5.0].into(),
        move |chip| {
            chip.bg(theme::chip_background(theme.muted_text))
                .border_1()
                .border_color(theme.border)
                .rounded(px(4.0))
        },
    )
}

/// The welcome screen's tips: the few keys and gestures that make the rest
/// discoverable, under the open prompt. Chords come from the keymap so an override
/// shows up here too.
pub(crate) fn welcome_hints(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let font = ui.config.ui_font;
    let chord = |id: CommandId| {
        ui.keymap
            .chord_for(Context::Base, id)
            .map(|chord| chord.display())
    };
    let mut tip = |key: Option<String>, what: &'static str| {
        let mut line = Vec::new();
        if let Some(key) = key {
            line.push(keycap(ui, theme, key, window, cx));
        }
        line.push(Aligned::text(
            typography::line(what, font, text_size::UI, theme.subtle_text),
            font,
            text_size::UI,
        ));
        typography::baseline_row(line, theme::space::SM)
    };
    // The prompt above owns the keyboard here, and the palette needs a repository,
    // so these read as "once one is open".
    let keys = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::XL))
        .child(tip(chord("palette.toggle"), "Command palette"))
        .child(tip(chord("help.shortcuts"), "All shortcuts"))
        .child(tip(chord("repo.open-dialog"), "Open another"));
    let all = ui::button(
        "welcome-shortcuts",
        ui::Variant::Ghost,
        ui::Size::Compact,
        theme,
        cx,
    )
    .child(typography::line(
        "Keyboard shortcuts",
        font,
        text_size::UI,
        theme.muted_text,
    ))
    .on_click(Diffui::on_action(Action::ToggleShortcuts, cx));
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(theme::space::MD))
        .child(typography::line(
            "Once a repository is open",
            emphasis_font(font, Weight::MEDIUM),
            text_size::CAPTION,
            theme.subtle_text,
        ))
        .child(keys)
        .child(typography::line(
            "Right-click a revision for its actions  \u{b7}  drag a revision or marked set onto another to rebase",
            font,
            text_size::UI,
            theme.subtle_text,
        ))
        .child(all)
        .into_any_element()
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

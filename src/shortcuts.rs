//! The keyboard-shortcut cheat sheet: `?` (or the palette's "Keyboard
//! shortcuts") lays every live binding out over the window, grouped by the
//! context it works in.
//!
//! It reads the built keymap rather than the registry's defaults, so a
//! `[keys]` override shows up here as bound, and a default it shadowed does
//! not.

use iced::{
    Alignment, Background, Border, Element, Length, Padding, Task, alignment,
    font::Weight,
    widget::{Space, column, container, row, text},
};

use crate::commands::{self, CommandArg, CommandId, Context, REGISTRY};
use crate::modes::{Mode, ModeKind};
use crate::theme::{self, ThemeSpec, emphasis_font, text_size};
use crate::typography::{self, Aligned};
use crate::{Action, Diffui, Message, UiEvent};

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
    pub(crate) fn toggle_shortcuts(&mut self) -> Task<Message> {
        if self.pop_mode(ModeKind::Shortcuts).is_none() {
            // It replaces the transient overlays rather than stacking over
            // them: they share one keyboard.
            self.pop_mode(ModeKind::Palette);
            self.pop_mode(ModeKind::Menu);
            self.pop_mode(ModeKind::ActivityPopover);
            self.push_mode(Mode::Shortcuts);
        }
        Task::none()
    }
}

/// The sheet over a scrim, or an empty `Space` when it is closed.
pub(crate) fn build_overlay(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if ui.mode(ModeKind::Shortcuts).is_none() {
        return Space::new().into();
    }

    let mut left = column![].spacing(theme::space::XL);
    let mut right = column![].spacing(theme::space::XL);
    let rows_of = |entries: Vec<Entry>| -> Vec<Element<'_, Message>> {
        entries
            .into_iter()
            .map(|entry| binding_row(ui, theme, entry.label, entry.keys, entry.disabled))
            .collect()
    };
    // Revisions is the long one: it and the repository keys go left, the
    // rest balances against them on the right.
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
        let block = section(ui, theme, title, rows_of(rows));
        if matches!(title, "Revisions" | "Repository and history") {
            left = left.push(block);
        } else {
            right = right.push(block);
        }
    }
    for (context, title) in SECTIONS {
        let rows = entries(ui, *context);
        if rows.is_empty() {
            continue;
        }
        right = right.push(section(ui, theme, title, rows_of(rows)));
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
            )
        })
        .collect();
    left = left.push(section(ui, theme, "Mouse", gestures));

    let title_font = emphasis_font(ui.config.ui_font, Weight::Medium);
    let header = row![typography::baseline_row(
        [
            Aligned::text(
                text("Keyboard shortcuts")
                    .size(text_size::TITLE)
                    .color(theme.text)
                    .font(title_font)
                    .width(Length::Fill),
                title_font,
                text_size::TITLE,
            ),
            Aligned::text(
                text("Rebind in config.toml [keys]   \u{b7}   esc to close")
                    .size(text_size::CAPTION)
                    .color(theme.subtle_text)
                    .font(ui.config.ui_font),
                ui.config.ui_font,
                text_size::CAPTION,
            ),
        ],
        0.0,
    )]
    .padding(Padding {
        right: crate::ui::DIALOG_PADDING,
        ..Padding::ZERO
    });

    let columns = row![
        left.width(Length::FillPortion(1)),
        right.width(Length::FillPortion(1))
    ]
    .spacing(theme::space::XXL * 1.5);

    let body = column![
        header,
        // The list scrolls out to the card's right edge, so the bar sits
        // where every other scroll view's does; the inset moves inside.
        crate::ui::scrollable(
            container(columns).padding(Padding {
                right: crate::ui::DIALOG_PADDING,
                ..Padding::ZERO
            }),
            theme,
        )
        .height(Length::Fill),
    ]
    .spacing(theme::space::XL);

    let card = crate::ui::dialog_card(body, CARD_WIDTH, theme)
        .padding(Padding {
            right: 0.0,
            ..Padding::from(crate::ui::DIALOG_PADDING)
        })
        .height(Length::Fixed(
            (ui.window_size.height - theme::modal_top() - theme::space::XXL)
                .clamp(240.0, CARD_MAX_HEIGHT),
        ));
    crate::ui::modal(
        card,
        Message::Action(Action::CloseShortcuts),
        Message::Ui(UiEvent::ConfirmNoOp),
    )
}

fn section<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    title: &'a str,
    rows: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut block = column![
        container(
            text(title)
                .size(text_size::CAPTION)
                .color(theme.subtle_text)
                .font(emphasis_font(ui.config.ui_font, Weight::Semibold)),
        )
        .padding(Padding {
            bottom: theme::space::XS,
            ..Padding::ZERO
        })
    ]
    .spacing(0);
    for row in rows {
        block = block.push(row);
    }
    block.into()
}

fn binding_row<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    label: String,
    keys: Vec<String>,
    disabled: Option<commands::Reason>,
) -> Element<'a, Message> {
    let font = ui.config.ui_font;
    let label = text(label)
        .size(text_size::UI)
        .font(font)
        .wrapping(text::Wrapping::None)
        .ellipsis(text::Ellipsis::End);
    // The label and the mono keycaps share a baseline (see
    // `crate::typography`). A command that can't run right now greys out
    // and says why after its label.
    let mut line = match disabled {
        None => vec![Aligned::text(
            label.color(theme.text).width(Length::Fill),
            font,
            text_size::UI,
        )],
        Some(reason) => vec![
            Aligned::text(label.color(theme.subtle_text), font, text_size::UI),
            Aligned::gap(theme::space::SM),
            Aligned::text(
                text(reason)
                    .size(text_size::CAPTION)
                    .color(theme.subtle_text)
                    .font(font)
                    .width(Length::Fill)
                    .wrapping(text::Wrapping::None)
                    .ellipsis(text::Ellipsis::End),
                font,
                text_size::CAPTION,
            ),
        ],
    };
    for (index, key) in keys.into_iter().enumerate() {
        line.push(Aligned::gap(if index == 0 {
            theme::space::MD
        } else {
            theme::space::XS
        }));
        line.push(keycap(ui, theme, key));
    }
    container(typography::baseline_row(line, 0.0))
        .height(Length::Fixed(ROW_HEIGHT))
        .center_y(Length::Fixed(ROW_HEIGHT))
        .into()
}

/// A key rendered like the tab strip's `⌘K` control: hairline border, mono.
pub(crate) fn keycap<'a>(ui: &'a Diffui, theme: ThemeSpec, key: String) -> Aligned<'a, Message> {
    let mono = ui.config.mono_font;
    Aligned::chip(
        text(key)
            .size(text_size::CAPTION)
            .color(theme.muted_text)
            .font(mono),
        mono,
        text_size::CAPTION,
        Padding::from([1, 5]),
        move |_| container::Style {
            background: Some(Background::Color(theme::chip_background(theme.muted_text))),
            border: Border {
                width: 1.0,
                color: theme.border,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        },
    )
}

/// The welcome screen's tips: the few keys and gestures that make the rest
/// discoverable, under the open prompt. Chords come from the keymap so an
/// override shows up here too.
pub(crate) fn welcome_hints(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let chord = |id: CommandId| {
        ui.keymap
            .chord_for(Context::Base, id)
            .map(|chord| chord.display())
    };
    let tip = |key: Option<String>, what: &'static str| -> Element<'_, Message> {
        let mut line = Vec::new();
        if let Some(key) = key {
            line.push(keycap(ui, theme, key));
        }
        line.push(Aligned::text(
            text(what)
                .size(text_size::UI)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
            ui.config.ui_font,
            text_size::UI,
        ));
        typography::baseline_row(line, theme::space::SM)
    };
    // The prompt above owns the keyboard here, and the palette needs a
    // repository, so these read as "once one is open".
    let heading = text("Once a repository is open")
        .size(text_size::CAPTION)
        .color(theme.subtle_text)
        .font(emphasis_font(ui.config.ui_font, Weight::Medium));
    let keys = row![
        tip(chord("palette.toggle"), "Command palette"),
        tip(chord("help.shortcuts"), "All shortcuts"),
        tip(chord("repo.open-dialog"), "Open another"),
    ]
    .spacing(theme::space::XL)
    .align_y(Alignment::Center);
    let gestures =
        text("Right-click a revision for its actions  \u{b7}  drag one onto another to rebase")
            .size(text_size::UI)
            .color(theme.subtle_text)
            .font(ui.config.ui_font);
    let all = crate::ui::button(
        text("Keyboard shortcuts")
            .size(text_size::UI)
            .color(theme.muted_text)
            .font(ui.config.ui_font),
        crate::ui::Variant::Ghost,
        crate::ui::Size::Compact,
        theme,
    )
    .on_press(Message::Action(Action::ToggleShortcuts));
    column![heading, keys, gestures, all]
        .spacing(theme::space::MD)
        .align_x(alignment::Horizontal::Center)
        .into()
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

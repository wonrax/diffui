//! The hint strip: one slim line docked under the diff, naming the keys of
//! whichever mode is on (target mode, line selection, a comparison), e.g.
//! `o onto · a after · b before · Space add source · ⏎ apply · Esc cancel`.
//!
//! Keys come from the live keymap, so a rebinding shows here as bound, and a
//! command with no key isn't advertised at all.

use iced::{
    Element, Length, Padding,
    widget::{container, text},
};

use diffui_core::DraftKind;

use crate::commands::{CommandId, Context};
use crate::theme::{self, ThemeSpec, text_size};
use crate::typography::{self, Aligned};
use crate::{Diffui, Message};

/// A hint: the commands whose keys it shows (several for a pair like `j k`),
/// and what pressing them does. No commands: a pointer gesture, shown as
/// text alone.
struct Hint {
    commands: &'static [CommandId],
    what: &'static str,
}

const fn hint(commands: &'static [CommandId], what: &'static str) -> Hint {
    Hint { commands, what }
}

/// The mode the strip speaks for, and its hints. Target mode outranks line
/// selection, which outranks a comparison: the top one owns the keyboard.
fn active_hints(ui: &Diffui) -> Option<(Context, Vec<Hint>)> {
    let tab = ui.active();
    if crate::conflicts::editor_shown(ui).is_some() {
        return Some((
            Context::ConflictEdit,
            vec![
                hint(&["conflict.edit.save"], "save"),
                hint(&["conflict.edit.cancel"], "cancel"),
            ],
        ));
    }
    if let Some(draft) = tab.op_draft() {
        let kind = draft.draft.kind;
        let mut hints = vec![hint(
            &["draft.candidate.next", "draft.candidate.previous"],
            "pick",
        )];
        if kind.uses_placement() {
            hints.extend([
                hint(&["draft.place.onto"], "onto"),
                hint(&["draft.place.after"], "after"),
                hint(&["draft.place.before"], "before"),
            ]);
        }
        let toggle = match kind {
            DraftKind::New => None,
            DraftKind::Merge => Some("add parent"),
            DraftKind::Parallelize => Some("add revision"),
            _ => Some("add source"),
        };
        if let Some(toggle) = toggle {
            hints.push(hint(&["draft.toggle-source"], toggle));
        }
        hints.extend([
            hint(&["draft.confirm"], "apply"),
            hint(&["draft.cancel"], "cancel"),
        ]);
        return Some((Context::Draft, hints));
    }
    if crate::changes::live_line_select(tab).is_some() {
        return Some((
            Context::Lines,
            vec![
                hint(&[], "Click or drag lines"),
                hint(&[], "\u{21e7}-click a range"),
                hint(&["lines.next", "lines.previous"], "move"),
                hint(&["lines.extend-next", "lines.extend-previous"], "extend"),
                hint(&["lines.toggle-line"], "pick"),
                hint(&["lines.exit"], "done"),
            ],
        ));
    }
    if let Some(mode) = crate::range::mode(ui) {
        use crate::range::RangeMode;
        let mut hints = Vec::new();
        let leave = match mode {
            RangeMode::Since => {
                hints.push(hint(&[], "Click a revision to move the end"));
                "stop diffing since trunk"
            }
            RangeMode::Span(_) => "clear the marks",
            RangeMode::Fixed => "leave the range",
        };
        hints.push(hint(&["range.edit"], "edit revset"));
        hints.push(hint(&["compare.exit"], leave));
        hints.push(hint(&["selection.clear"], leave));
        return Some((Context::Base, hints));
    }
    if crate::compare::comparing(ui) {
        let mut hints = vec![
            hint(&["compare.swap"], "swap sides"),
            hint(&["compare.interdiff"], "interdiff"),
            hint(&["compare.exit"], "stop comparing"),
        ];
        // Esc leaves a comparison once no rows are marked (it clears the
        // marks first).
        if tab.revision_multi_selection.is_empty() {
            hints.push(hint(&["selection.clear"], "stop comparing"));
        }
        return Some((Context::Base, hints));
    }
    // Conflict regions on screen: their keys, once bound.
    if crate::conflicts::has_regions(ui) {
        return Some((
            Context::Base,
            vec![
                hint(
                    &["conflict.region.next", "conflict.region.previous"],
                    "region",
                ),
                hint(&["conflict.region.take-side-1"], "take side 1"),
                hint(&["conflict.region.take-side-2"], "take side 2"),
                hint(&["conflict.region.take-base"], "take base"),
            ],
        ));
    }
    None
}

/// The first live key of each of `commands`, deduplicated.
fn keys(ui: &Diffui, context: Context, commands: &[CommandId]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for id in commands {
        if let Some(chord) = ui.keymap.live_chords(context, id).first() {
            let key = chord.display();
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

/// The strip for the active mode, or `None` when no mode with keys is on.
pub(crate) fn strip<'a>(ui: &'a Diffui, theme: ThemeSpec) -> Option<Element<'a, Message>> {
    let (context, hints) = active_hints(ui)?;
    let font = ui.config.ui_font;
    let mut line: Vec<Aligned<'a, Message>> = Vec::new();
    let mut seen_what: Vec<&str> = Vec::new();
    let mut any_key = false;
    for hint in hints {
        let keys = keys(ui, context, hint.commands);
        // An unbound command has nothing to teach; a second binding of the
        // same thing (Esc and compare.exit both stopping a comparison)
        // would only repeat it.
        if (keys.is_empty() && !hint.commands.is_empty()) || seen_what.contains(&hint.what) {
            continue;
        }
        seen_what.push(hint.what);
        if !line.is_empty() {
            line.push(Aligned::gap(theme::space::LG));
        }
        let gesture = keys.is_empty();
        any_key |= !gesture;
        for (index, key) in keys.into_iter().enumerate() {
            if index > 0 {
                line.push(Aligned::gap(theme::space::XS));
            }
            line.push(crate::shortcuts::keycap(ui, theme, key));
        }
        if !gesture {
            line.push(Aligned::gap(theme::space::SM));
        }
        line.push(Aligned::text(
            text(hint.what)
                .size(text_size::CAPTION)
                .font(font)
                .color(theme.muted_text)
                .wrapping(text::Wrapping::None),
            font,
            text_size::CAPTION,
        ));
    }
    // Gestures alone don't earn a strip: the mode has no keys left bound.
    if !any_key {
        return None;
    }
    Some(
        container(typography::baseline_row(line, 0.0))
            .width(Length::Fill)
            .clip(true)
            .padding(Padding::from([
                theme::space::SM,
                crate::diff_view::HEADER_HORIZONTAL_PADDING,
            ]))
            .style(move |_| {
                container::Style::default()
                    .background(theme.file_header)
                    .border(iced::Border {
                        width: 1.0,
                        color: theme.border,
                        radius: 0.0.into(),
                    })
            })
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::tests::{app, push_tab};

    #[test]
    fn keys_follow_the_live_keymap() {
        let mut ui = app();
        push_tab(&mut ui, "/tmp/hints");
        assert_eq!(
            keys(
                &ui,
                Context::Draft,
                &["draft.candidate.next", "draft.candidate.previous"]
            ),
            vec!["j".to_owned(), "k".to_owned()]
        );
        // Unbound by default: nothing to show.
        assert!(keys(&ui, Context::Base, &["compare.swap"]).is_empty());
        assert!(active_hints(&ui).is_none());
    }

    #[test]
    fn a_rebinding_shows_in_the_hints() {
        let mut ui = app();
        #[derive(serde::Deserialize)]
        struct Fixture {
            keys: crate::keymap::RawKeys,
        }
        let fixture: Fixture = toml::from_str(
            r#"
            [keys.lines]
            "lines.next" = "n"
            "lines.exit" = []
            "#,
        )
        .expect("valid keys");
        ui.keymap = crate::keymap::Keymap::build(&fixture.keys).0;
        assert_eq!(
            keys(&ui, Context::Lines, &["lines.next", "lines.previous"]),
            vec!["n".to_owned(), "k".to_owned()]
        );
        assert!(keys(&ui, Context::Lines, &["lines.exit"]).is_empty());
    }
}

//! The hint strip: one slim line docked under the diff, naming the keys of
//! whichever mode is on (target mode, line selection, a comparison), e.g.
//! `o onto · a after · b before · Space add source · ⏎ apply · Esc cancel`.
//!
//! Keys come from the live keymap, so a rebinding shows here as bound, and a
//! command with no key isn't advertised at all.

use crate::Cx;
use crate::typography::{self, Aligned};
use diffui_core::DraftKind;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use crate::Diffui;
use crate::commands::{CommandId, Context};
use crate::theme::{self, text_size};
use crate::ui::Look;

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
                hint(&["conflict.edit.save"], "Save"),
                hint(&["conflict.edit.cancel"], "Cancel"),
            ],
        ));
    }
    if let Some(draft) = tab.op_draft() {
        let kind = draft.draft.kind;
        let mut hints = vec![hint(
            &["draft.candidate.next", "draft.candidate.previous"],
            "Pick",
        )];
        if kind.uses_placement() {
            hints.extend([
                hint(&["draft.place.onto"], "Onto"),
                hint(&["draft.place.after"], "After"),
                hint(&["draft.place.before"], "Before"),
            ]);
        }
        let toggle = match kind {
            DraftKind::New => None,
            DraftKind::Merge => Some("Add parent"),
            DraftKind::Parallelize => Some("Add revision"),
            _ => Some("Add source"),
        };
        if let Some(toggle) = toggle {
            hints.push(hint(&["draft.toggle-source"], toggle));
        }
        hints.extend([
            hint(&["draft.confirm"], "Apply"),
            hint(&["draft.cancel"], "Cancel"),
        ]);
        return Some((Context::Draft, hints));
    }
    if crate::changes::live_line_select(tab).is_some() {
        return Some((
            Context::Lines,
            vec![
                hint(&[], "Click or drag lines"),
                hint(&[], "\u{21e7}-click a range"),
                hint(&["lines.next", "lines.previous"], "Move"),
                hint(&["lines.extend-next", "lines.extend-previous"], "Extend"),
                hint(&["lines.toggle-line"], "Pick"),
                hint(&["lines.exit"], "Done"),
            ],
        ));
    }
    if let Some(mode) = crate::range::mode(ui) {
        use crate::range::RangeMode;
        let mut hints = Vec::new();
        let leave = match mode {
            RangeMode::Since => {
                hints.push(hint(&[], "Click a revision to move the end"));
                "Stop diffing since trunk"
            }
            RangeMode::Span(_) => "Clear the marks",
            RangeMode::Fixed => "Leave the range",
        };
        hints.push(hint(&["range.edit"], "Edit revset"));
        hints.push(hint(&["compare.exit"], leave));
        hints.push(hint(&["selection.clear"], leave));
        return Some((Context::Base, hints));
    }
    if crate::compare::comparing(ui) {
        let mut hints = vec![
            hint(&["compare.swap"], "Swap sides"),
            hint(&["compare.interdiff"], "Interdiff"),
            hint(&["compare.exit"], "Stop comparing"),
        ];
        // Esc leaves a comparison once no rows are marked (it clears the
        // marks first).
        if tab.revision_multi_selection.is_empty() {
            hints.push(hint(&["selection.clear"], "Stop comparing"));
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
                    "Region",
                ),
                hint(&["conflict.region.take-side-1"], "Take side 1"),
                hint(&["conflict.region.take-side-2"], "Take side 2"),
                hint(&["conflict.region.take-base"], "Take base"),
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
pub(crate) fn strip(
    ui: &Diffui,
    look: Look,
    _window: &mut Window,
    _cx: &mut Cx,
) -> Option<AnyElement> {
    let Look { theme, .. } = look;
    let (context, hints) = active_hints(ui)?;
    let font = look.font;
    let mut line: Vec<Aligned> = Vec::new();
    let mut seen_what: Vec<&str> = Vec::new();
    let mut any_key = false;
    for hint in hints {
        let keys = keys(ui, context, hint.commands);
        // An unbound command has nothing to teach; a second binding of the same
        // thing (Esc and compare.exit both stopping a comparison) would only
        // repeat it.
        if (keys.is_empty() && !hint.commands.is_empty()) || seen_what.contains(&hint.what) {
            continue;
        }
        seen_what.push(hint.what);
        if !line.is_empty() {
            line.push(Aligned::gap(theme::space::LG));
        }
        let gesture = keys.is_empty();
        any_key |= !gesture;
        if !gesture {
            line.push(crate::ui::chord_aligned(keys, look));
        }
        if !gesture {
            line.push(Aligned::gap(theme::space::SM));
        }
        line.push(Aligned::text(
            typography::line(hint.what, font, text_size::CAPTION, theme.muted_text),
            font,
            text_size::CAPTION,
        ));
    }
    // Gestures alone don't earn a strip: the mode has no keys left bound.
    if !any_key {
        return None;
    }
    Some(
        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .px(px(crate::diff_view::HEADER_HORIZONTAL_PADDING))
            .py(px(theme::space::SM))
            .bg(theme.file_header)
            // Docked on the pane's bottom: the status bar's rule is under it
            // and the pane dividers beside it.
            .border_t_1()
            .border_color(theme.border)
            .child(typography::baseline_row(line, 0.0))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::tests::{app, push_tab};

    #[test]
    fn keys_follow_the_live_keymap() {
        crate::tabs::tests::with_cx(|_| {
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
        });
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

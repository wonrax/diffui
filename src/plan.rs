//! The plan as jj: the command an operation is equivalent to, shown where
//! the operation is (the target-mode banner, the activity log, the op log)
//! in mono with its revisions in the graph's colour, and a copy button.
//!
//! The command itself comes from core ([`diffui_core::plan`]); this module
//! names revisions from the loaded graph and draws the line.

use iced::{
    Element,
    widget::{button, text},
};

use diffui_core::plan::{self, Phrase};
use diffui_core::{MutationOp, RevisionSelection, Vcs};

use crate::prose::Prose;
use crate::theme::{ThemeSpec, ghost_button_style, text_size};
use crate::typography::{self, Aligned};
use crate::{Action, Diffui, Message, icons, ui};

/// Size of the line's glyphs.
const GLYPH: f32 = 12.0;

/// The jj command `op` is equivalent to, as a user would type it, named
/// from the active tab's graph. `None` outside a jj repository.
pub(crate) fn op_command(ui: &Diffui, op: &MutationOp, allow_immutable: bool) -> Option<String> {
    let tab = ui.active();
    if tab.repository.as_ref().map(|repository| repository.vcs) != Some(Vcs::Jj) {
        return None;
    }
    let equivalent = op.cli_equivalent(&tab.session.commits);
    let equivalent = if allow_immutable {
        plan::ignoring_immutable(equivalent)
    } else {
        equivalent
    };
    Some(equivalent.to_shell())
}

/// The jj command a target-mode draft stands for: the whole command once
/// its destination is armed (`op`), else the command up to where the
/// destination goes. The draft banner shows it and `:` opens the palette on
/// it, so the two always read the same.
pub(crate) fn draft_command(
    ui: &Diffui,
    draft: &diffui_core::OpDraft,
    op: Option<&MutationOp>,
) -> Option<String> {
    match op {
        Some(op) => op_command(ui, op, false),
        None => ui
            .active()
            .repository
            .as_ref()
            .map(|_| draft.cli_prefix(&ui.active().session.commits)),
    }
}

/// `jj git fetch …` for a fetch, in a jj repository.
pub(crate) fn fetch_command(ui: &Diffui, target: &diffui_core::FetchTarget) -> Option<String> {
    let repository = ui.active().repository.as_ref()?;
    (repository.vcs == Vcs::Jj).then(|| plan::fetch_command(target).to_shell())
}

/// What `op` will do, in words, named from the active tab's graph.
pub(crate) fn op_sentence(
    ui: &Diffui,
    op: &MutationOp,
    descendants: Option<usize>,
) -> Option<Prose> {
    let commits = &ui.active().session.commits;
    let name = |revision: &RevisionSelection| plan::graph_phrase(commits, revision);
    let phrases = plan::sentence(op, &name, descendants)?;
    Some(
        phrases
            .into_iter()
            .fold(Prose::new(), |prose, phrase| match phrase {
                Phrase::Text(text) => prose.text(text),
                Phrase::Revision(id) => prose.rev(id),
                Phrase::Bookmark(name) => prose.bookmark(name),
            }),
    )
}

/// A command line (or a revset) with the revisions in it marked, so they
/// render in the graph's colour: `'trunk()..xyz'` marks `xyz`.
pub(crate) fn command_prose(command: &str) -> Prose {
    let mut prose = Prose::new();
    let mut previous: Option<char> = None;
    let mut rest = command;
    while let Some(first) = rest.chars().next() {
        let len = if is_id_char(first) {
            rest.find(|c: char| !is_id_char(c)).unwrap_or(rest.len())
        } else {
            first.len_utf8()
        };
        let (run, tail) = rest.split_at(len);
        // `@` alone is the working copy; in `main@origin` it's a remote.
        let working_copy = run == "@" && !previous.is_some_and(char::is_alphanumeric);
        prose = if working_copy || names_revision(run) {
            prose.rev(run)
        } else {
            prose.code(run)
        };
        previous = run.chars().last();
        rest = tail;
    }
    prose
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_digit() || c.is_ascii_lowercase()
}

/// Whether a word of a command names a revision the way diffui writes them:
/// `@`, a short change id (jj's `k`–`z` alphabet) or a commit id prefix.
fn names_revision(word: &str) -> bool {
    let len = word.len();
    let change_id =
        (plan::NAME_CHARS..=32).contains(&len) && word.bytes().all(|b| (b'k'..=b'z').contains(&b));
    let commit_id = len == 12
        && word
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        && word.bytes().any(|b| b.is_ascii_digit());
    change_id || commit_id
}

/// The command on one line: a terminal glyph, the command in mono (cut
/// short with an ellipsis when it doesn't fit) and a copy button.
pub(crate) fn command_line<'a>(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &str,
    size: f32,
) -> Element<'a, Message> {
    let mono = ui.config.mono_font;
    let copy = ui::tooltip(
        button(icons::icon(icons::COPY, GLYPH, theme.subtle_text))
            .padding(2)
            .on_press(Message::Action(Action::Copy(command.to_owned())))
            .style(move |_, status| ghost_button_style(theme, status)),
        "Copy command",
        iced::widget::tooltip::Position::Top,
        ui.config.ui_font,
        theme,
    );
    typography::baseline_row(
        [
            Aligned::glyph(
                icons::icon(icons::TERMINAL, GLYPH, theme.subtle_text),
                GLYPH,
            ),
            Aligned::gap(6.0),
            Aligned::text(
                command_prose(command)
                    .view(mono, mono, theme.muted_text, theme)
                    .size(size)
                    .wrapping(text::Wrapping::None),
                mono,
                size,
            ),
            Aligned::gap(4.0),
            Aligned::glyph(copy, GLYPH + 4.0),
        ],
        0.0,
    )
}

/// [`command_line`] at caption size, the size the logs use.
pub(crate) fn caption_command_line<'a>(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &str,
) -> Element<'a, Message> {
    command_line(ui, theme, command, text_size::CAPTION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_are_told_from_the_rest_of_the_command() {
        for word in ["kmnopqrs", "qtmsuqtzlkps", "0123456789ab"] {
            assert!(names_revision(word), "{word}");
        }
        for word in [
            "jj",
            "rebase",
            "--ignore-immutable",
            "main",
            "squash",
            "'trunk()'",
        ] {
            assert!(!names_revision(word), "{word}");
        }
        assert_eq!(
            command_prose("jj new kmnopqrs").to_string(),
            "jj new kmnopqrs"
        );
        let revisions = |command: &str| -> Vec<String> {
            command_prose(command)
                .revisions()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(
            revisions("jj diff -r 'trunk()..kmnopqrs' @ main@origin"),
            vec!["kmnopqrs", "@"]
        );
    }
}

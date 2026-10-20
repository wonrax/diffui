//! The plan as jj: the command an operation is equivalent to, shown where
//! the operation is (the target-mode banner, the activity log, the op log)
//! in mono with its revisions in the graph's colour, and a copy button.
//!
//! The command itself comes from core ([`diffui_core::plan`]); this module
//! names revisions from the loaded graph and draws the line.

use std::cell::RefCell;
use std::collections::HashMap;

use iced::{
    Element,
    widget::{button, text},
};

use diffui_core::cli::highlight::{self, Mark};
use diffui_core::plan::{self, Phrase};
use diffui_core::{JjCli, MutationOp, RevisionSelection, Vcs};

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

/// A command line with the names in it marked by what jj's grammar says
/// each argument is, so they render in the graph's colours: the revisions
/// in its revsets, its bookmark and remote arguments. `jj bookmark set
/// push-xyz` marks `push-xyz` whole, as a bookmark.
pub(crate) fn command_prose(command: &str) -> Prose {
    /// Lines drawn recently. Each one is a walk through jj's clap tree, and
    /// the op log draws hundreds of them on every frame.
    const CAPACITY: usize = 1024;
    thread_local! {
        static MEMO: RefCell<HashMap<String, Prose>> = RefCell::new(HashMap::new());
    }
    if let Some(prose) = MEMO.with_borrow(|memo| memo.get(command).cloned()) {
        return prose;
    }
    let marks = highlight::command(&JjCli::default(), command);
    let prose = marked_prose(command, &marks);
    MEMO.with_borrow_mut(|memo| {
        if memo.len() >= CAPACITY {
            memo.clear();
        }
        memo.insert(command.to_owned(), prose.clone());
    });
    prose
}

/// A revset with the names in it marked (see [`highlight::revset`]).
pub(crate) fn revset_prose(revset: &str) -> Prose {
    marked_prose(revset, &highlight::revset(revset))
}

fn marked_prose(text: &str, marks: &[highlight::Marked]) -> Prose {
    let mut prose = Prose::new();
    let mut at = 0;
    for marked in marks {
        prose = prose.code(&text[at..marked.range.start]);
        let name = &text[marked.range.clone()];
        prose = match marked.mark {
            Mark::Revision => prose.rev(name),
            Mark::Bookmark => prose.bookmark(name),
            Mark::Remote => prose.remote(name),
        };
        at = marked.range.end;
    }
    prose.code(&text[at..])
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
    fn a_command_marks_what_its_arguments_name() {
        let prose = command_prose("jj bookmark set push-sknyzssrnkun -r ed200ca71ab8");
        assert_eq!(
            prose.to_string(),
            "jj bookmark set push-sknyzssrnkun -r ed200ca71ab8"
        );
        assert_eq!(prose.revisions().collect::<Vec<_>>(), ["ed200ca71ab8"]);
        assert_eq!(prose.bookmarks().collect::<Vec<_>>(), ["push-sknyzssrnkun"]);
        // A multi-line message is text, whatever it holds, and stays as
        // written (the one-line views fold its breaks).
        let line = "jj describe -r @ --message 'fix(ui): kmnopqrs\n\nAlign zzzzzzzz'";
        let prose = command_prose(line);
        assert_eq!(prose.to_string(), line);
        assert_eq!(prose.revisions().collect::<Vec<_>>(), ["@"]);
    }

    #[test]
    fn a_revset_marks_whole_symbols() {
        let prose = revset_prose("push-sknyzssrnkun@origin | trunk()..kmnopqrs");
        assert_eq!(prose.revisions().collect::<Vec<_>>(), ["kmnopqrs"]);
        assert_eq!(prose.bookmarks().collect::<Vec<_>>(), ["push-sknyzssrnkun"]);
    }
}

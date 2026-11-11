//! The plan as jj: the command an operation is equivalent to, shown where
//! the operation is (the target-mode banner, the activity log, the op log)
//! in mono with its revisions in the graph's colour, and a copy button.
//!
//! The command itself comes from core ([`diffui_core::plan`]); this module
//! names revisions from the loaded graph and draws the line.

use crate::typography::{self, Aligned};
use crate::ui::Tip as _;
use crate::{Action, Cx, icons, ui};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};
use std::cell::RefCell;
use std::collections::HashMap;

use diffui_core::cli::highlight::{self, Mark};
use diffui_core::plan::{self, Phrase};
use diffui_core::{JjCli, MutationOp, RevisionSelection, Vcs};

use crate::Diffui;
use crate::ellipsis;
use crate::prose::Prose;
use crate::theme::{ThemeSpec, text_size};

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
    let equivalent = if allow_immutable && !op.restores_operation() {
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

/// Width of a command line's glyph, gaps and copy button around the
/// command.
const CHROME: f32 = GLYPH + 6.0 + 4.0 + GLYPH + 4.0;

/// The command's text, highlighted.
fn command_text(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &str,
    size: f32,
) -> ellipsis::Line<'static> {
    let mono = ui.config.mono_font;
    command_prose(command).line(mono, mono, theme.muted_text, theme, size)
}

/// Whether the command fits `width` on one line, glyph and copy button
/// included.
pub(crate) fn command_fits(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &str,
    size: f32,
    width: f32,
) -> bool {
    let line = command_text(ui, theme, command, size);
    !line.is_multiline() && line.fits(width - CHROME)
}

/// The command on one line: a terminal glyph, the command in mono (cut short
/// with an ellipsis when it doesn't fit) and a copy button, which copies the
/// whole command.
pub(crate) fn command_line(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &str,
    size: f32,
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    // Width-free: the line gives up what its row can't hold, so it fits anywhere it
    // is placed and the copy button stays right behind the text.
    let text = command_text(ui, theme, command, size).view();
    laid_out(text, command, size, ui, theme, cx)
}

/// The whole command, wrapped to the width it gets, its later lines under the
/// command's first rather than under the glyph, the copy button after it.
pub(crate) fn wrapped_command_line(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &str,
    size: f32,
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let text = command_text(ui, theme, command, size).wrapped();
    laid_out(text, command, size, ui, theme, cx)
}

fn laid_out(
    text: gpui_kit::Div,
    command: &str,
    size: f32,
    ui: &Diffui,
    theme: ThemeSpec,
    cx: &mut Cx,
) -> AnyElement {
    let (ui_font, mono) = (ui.config.ui_font, ui.config.mono_font);
    // Commands repeat across a list: the text says which one this is.
    let id = {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        command.hash(&mut hasher);
        hasher.finish() as usize
    };
    let copy = div()
        .id(("copy-command-tip", id))
        .tip("Copy command", ui_font, theme)
        // Above a row that opens on press, a press here is the button's alone.
        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation()
        })
        .child(
            ui::button(
                ("copy-command", id),
                ui::Variant::Ghost,
                ui::Size::Compact,
                theme,
                cx,
            )
            .size(px(GLYPH + 4.0))
            .p(px(0.0))
            .child(icons::icon(icons::COPY, GLYPH, theme.subtle_text))
            .on_click(Diffui::on_action(Action::Copy(command.to_owned()), cx)),
        );
    typography::baseline_row(
        [
            Aligned::glyph(
                icons::icon(icons::TERMINAL, GLYPH, theme.subtle_text),
                GLYPH,
            ),
            Aligned::gap(6.0),
            Aligned::text(text, mono, size).shrink(),
            Aligned::gap(4.0),
            Aligned::glyph(copy, GLYPH + 4.0),
        ],
        0.0,
    )
    .into_any_element()
}

/// [`command_line`] at caption size, the size the logs use.
pub(crate) fn caption_command_line(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &str,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    command_line(ui, theme, command, text_size::CAPTION, window, cx)
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

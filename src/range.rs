//! Range diffs in the diff pane: `jj diff -r` over several commits.
//!
//! Three ways in, one banner. A typed range (`jj diff -r 'trunk()..alpha'`)
//! stays put. "Diff since trunk" shows `trunk()..X` for the browsed
//! revision X and follows the cursor, the way a comparison's base sticks. A
//! ⇧-click span of graph rows shows them as one diff once jj agrees they
//! have no gaps. The session owns the state ([`diffui_core::Session`]'s
//! `diff_since` and `span`); this module draws the banner, the commit list
//! that stands in for the commit info, and the empty states.

use diffui_core::{DiffBase, DiffRange, DiffSpan, RevisionSelection};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use crate::action::Action;
use crate::diff_view::{HeaderAction, HeaderField, HeaderLine};
use crate::prose::Prose;
use crate::theme::{ThemeSpec, Weight, emphasis_font, text_size};
use crate::typography::{self, Aligned};
use crate::{Cx, Diffui, icons, ui};

/// The revset "diff since trunk" measures from.
pub(crate) const TRUNK: &str = "trunk()";

/// Commits the list under the banner names before it counts the rest.
const LISTED_COMMITS: usize = 30;

/// The banner's edit and exit controls and the gap before them, which the revset
/// beside them leaves room for.
const BANNER_CONTROLS_WIDTH: f32 = crate::theme::control::COMPACT + 4.0 + ui::CLOSE_SIDE + 8.0;

#[derive(Debug, Clone)]
pub(crate) enum RangeAction {
    /// `trunk()..X`, following the cursor. `symbol` names X in the revset
    /// when it was picked by name (a bookmark).
    SinceTrunk {
        target: RevisionSelection,
        symbol: Option<String>,
    },
    /// Edit the range as its `jj diff -r` in the palette's command line.
    Edit,
}

/// Which kind of range the pane is on.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RangeMode<'a> {
    /// "Diff since trunk", following the cursor.
    Since,
    /// Marked rows shown as one diff, or refused as one.
    Span(&'a DiffSpan),
    /// A range that stays put: typed or edited as a `jj diff` command.
    Fixed,
}

/// The base the pane is showing, or about to: while a switch is in flight
/// the requested one, so the banner answers at once.
fn shown_base(ui: &Diffui) -> &DiffBase {
    let session = &ui.active().session;
    if session.diff_in_flight() {
        &session.diff_base
    } else {
        &session.document_base
    }
}

/// The range mode on, if any.
pub(crate) fn mode(ui: &Diffui) -> Option<RangeMode<'_>> {
    let session = &ui.active().session;
    if let Some(span) = &session.span {
        return Some(RangeMode::Span(span));
    }
    shown_base(ui).range()?;
    Some(match session.diff_since.as_deref() {
        Some(_) => RangeMode::Since,
        None => RangeMode::Fixed,
    })
}

/// Whether browsing a revision leaves the range on screen: a span or a
/// fixed range has nothing to say about another revision. "Diff since"
/// re-targets instead.
pub(crate) fn browsing_leaves(session: &diffui_core::Session) -> bool {
    session.span.is_some()
        || session.diff_since.is_none()
            && (session.diff_base.range().is_some() || session.document_base.range().is_some())
}

/// The range the document on screen covers, when it is the range the pane
/// is on (not a document left over from before the switch).
pub(crate) fn shown_range(ui: &Diffui) -> Option<&DiffRange> {
    let session = &ui.active().session;
    let range = session.document.range.as_deref()?;
    (session.document_base.range() == Some(range.revset.as_str())).then_some(range)
}

/// The span's refusal, when the marked rows can't be one diff.
pub(crate) fn refused_span(ui: &Diffui) -> Option<&diffui_core::RevsetError> {
    ui.active().session.span.as_ref()?.refused.as_ref()
}

/// The revset the banner names. A span reads as `root::head` once it
/// loaded with one of each, which is what such a set is.
fn display_revset(ui: &Diffui, mode: RangeMode<'_>) -> String {
    let session = &ui.active().session;
    if let RangeMode::Span(span) = mode {
        if let Some(range) = shown_range(ui)
            && let ([root], [head]) = (range.roots.as_slice(), range.heads.as_slice())
        {
            let name = |hex: &String| session.revision_symbol(&session.canonical_selection(hex));
            return format!("{}::{}", name(root), name(head));
        }
        return span.revset.clone();
    }
    shown_base(ui).range().unwrap_or_default().to_owned()
}

impl Diffui {
    pub(crate) fn perform_range(&mut self, action: RangeAction, cx: &mut Cx) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        match action {
            RangeAction::SinceTrunk { target, symbol } => {
                self.active_mut().revision_multi_selection.clear();
                let effects = self.active_mut().session.diff_since(TRUNK, target, symbol);
                self.run_effects(tab, effects)
            }
            RangeAction::Edit => {
                let Some(mode) = mode(self) else {
                    return;
                };
                let command = diff_command(&display_revset(self, mode));
                self.open_command_line(command, cx)
            }
        }
    }

    /// The marked rows changed by a ⇧- or ⌘-click: a ⇧-click span shows
    /// them as one diff; a ⌘-click reshapes a span already on screen.
    pub(crate) fn sync_span(&mut self, started_by_shift: bool) {
        let Some(tab) = self.active_tab_id() else {
            return;
        };
        let state = self.active();
        if !state.session.capabilities.range_diff
            || !(started_by_shift || state.session.span.is_some())
        {
            return;
        }
        let marked: Vec<RevisionSelection> = state
            .revision_multi_selection
            .iter()
            .filter(|hex| state.session.commits.find_by_commit_id(hex).is_some())
            .map(|hex| state.session.canonical_selection(hex))
            .collect();
        let effects = self.active_mut().session.diff_span(&marked);
        self.run_effects(tab, effects)
    }
}

/// `jj diff -r '<revset>'`, as the pane's range would be typed.
fn diff_command(revset: &str) -> String {
    format!(
        "jj diff -r {}",
        diffui_core::cli::tokenize::shell_quote(revset)
    )
}

/// "3 commits", the range's title.
pub(crate) fn title(range: &DiffRange) -> String {
    match range.count {
        0 => "No commits".to_owned(),
        1 => "1 commit".to_owned(),
        count => format!("{count} commits"),
    }
}

/// What the pane says when the range has no changes to show.
pub(crate) fn empty_message(ui: &Diffui, range: &DiffRange) -> &'static str {
    match (range.count, mode(ui)) {
        (0, Some(RangeMode::Since)) => "Already in trunk",
        (0, _) => "No commits in this range",
        _ => "These commits change no files",
    }
}

/// The commit list that stands in for the commit info: one row per commit,
/// its id and title as the commit info writes a parent's.
pub(crate) fn header_lines(ui: &Diffui, range: &DiffRange, theme: ThemeSpec) -> Vec<HeaderLine> {
    let session = &ui.active().session;
    let mut lines = Vec::new();
    for (index, hex) in range.commits.iter().take(LISTED_COMMITS).enumerate() {
        let row = session.commits.find_by_commit_id(hex);
        let (id, unique, subject) = match row {
            Some(row) => {
                let unique = row
                    .shortest_change_id_len()
                    .or_else(|| session.sidebar_prefix_lens.get(row.index()).copied())
                    .filter(|&len| len > 0)
                    .unwrap_or(crate::sidebar::REVISION_ID_CHARS);
                let change = row.change_id();
                let mut id: String = change
                    .chars()
                    .take(crate::sidebar::revision_id_display_len(unique, change))
                    .collect();
                if row.is_working_copy() {
                    id = format!("@ {id}");
                }
                let subject = row
                    .description()
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("(no description set)")
                    .to_owned();
                let unique = if row.is_working_copy() {
                    unique + 2
                } else {
                    unique
                };
                (id, unique, subject)
            }
            None => (
                hex.chars().take(crate::sidebar::COMMIT_ID_CHARS).collect(),
                0,
                "(not in the log)".to_owned(),
            ),
        };
        let id_chars = id.chars().count();
        let mut field = HeaderField::new(
            if index == 0 { "Commits" } else { "" },
            format!("{id}  {subject}"),
        );
        field.runs = vec![
            (0, theme.accent),
            (unique.min(id_chars), theme.subtle_text),
            (id_chars, theme.muted_text),
        ];
        field.link = Some((id_chars, HeaderAction::Open(hex.clone())));
        lines.push(HeaderLine::Rich(Box::new(field)));
    }
    let rest = range
        .count
        .saturating_sub(LISTED_COMMITS.min(range.commits.len()));
    if rest > 0 {
        let mut field = HeaderField::new("", format!("and {rest} more"));
        field.runs = vec![(0, theme.subtle_text)];
        lines.push(HeaderLine::Rich(Box::new(field)));
    }
    lines
}

/// The banner for a range: what it is, the revset,
/// the jj command, and edit and exit. `None` when no range is on.
pub(crate) fn banner(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> Option<AnyElement> {
    let mode = mode(ui)?;
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let medium = emphasis_font(font, Weight::MEDIUM);
    let refused = refused_span(ui);
    let (glyph, label) = match mode {
        RangeMode::Since => (icons::GIT_BRANCH, "Since trunk"),
        RangeMode::Span(span) if span.refused.is_some() || shown_range(ui).is_none() => {
            (icons::FILE_DIFF, "Marked")
        }
        RangeMode::Span(_) | RangeMode::Fixed => (icons::FILE_DIFF, "Range"),
    };
    let revset = display_revset(ui, mode);

    let mut line = vec![
        Aligned::glyph(
            icons::icon(glyph, ui::ICON_SIZE, theme.accent),
            ui::ICON_SIZE,
        ),
        Aligned::text(
            typography::line(label, medium, text_size::BODY, theme.text),
            medium,
            text_size::BODY,
        ),
    ];
    match (mode, refused) {
        (RangeMode::Span(span), Some(_)) => line.push(Aligned::text(
            typography::line(
                format!("{} revisions", span.count),
                font,
                text_size::BODY,
                theme.muted_text,
            ),
            font,
            text_size::BODY,
        )),
        _ => {
            // What the pane leaves the revset: it ellipsises before the controls.
            let room = (crate::diff_panel::diff_pane_width(ui)
                - 2.0 * crate::theme::space::XL
                - BANNER_CONTROLS_WIDTH
                - ui::ICON_SIZE
                - 16.0
                - crate::measure::line_width(label, text_size::BODY, medium))
            .max(0.0);
            line.push(Aligned::text(
                crate::plan::revset_prose(&revset)
                    .line(mono, mono, theme.text, theme, text_size::BODY)
                    .cut(room),
                mono,
                text_size::BODY,
            ));
        }
    }
    let headline = typography::baseline_row(line, 8.0);

    let subline: AnyElement = match refused {
        Some(_) => typography::line(
            "Not a connected range. \u{2318}-click rows to fill the gap",
            font,
            text_size::CAPTION,
            theme.conflict_marker,
        )
        .into_any_element(),
        None => crate::plan::caption_command_line(ui, theme, &diff_command(&revset), window, cx),
    };
    let sides = div().child(headline).child(subline);

    let mut controls = div();
    if refused.is_none() {
        controls = controls.child(
            ui::icon_button(
                "range-edit",
                icons::PENCIL,
                theme.muted_text,
                ui::Size::Compact,
                theme,
                cx,
            )
            .on_click(Diffui::on_action(Action::Range(RangeAction::Edit), cx))
            .tip("Edit as a jj command", font, theme),
        );
    }
    controls = controls.child(
        ui::close_button("range-exit", theme.muted_text, theme, cx)
            .on_click(Diffui::on_action(
                Action::Compare(crate::compare::CompareAction::Exit),
                cx,
            ))
            .tip(
                match mode {
                    RangeMode::Since => "Stop diffing since trunk",
                    RangeMode::Span(_) => "Clear the marks",
                    RangeMode::Fixed => "Leave the range",
                },
                font,
                theme,
            ),
    );

    Some(crate::compare::banner_strip(sides, controls, font, theme))
}

/// The pane when the marked rows can't be one diff: jj's reason, with the
/// revision it names styled as one.
pub(crate) fn refused_state(
    ui: &Diffui,
    theme: ThemeSpec,
    error: &diffui_core::RevsetError,
    _window: &mut Window,
    _cx: &mut Cx,
) -> AnyElement {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let mut detail = Prose::from("jj diff -r needs the marked revisions to have no gaps.");
    if let Some(hint) = &error.hint {
        detail = detail.text(" ");
        for (index, word) in hint.split(' ').enumerate() {
            if index > 0 {
                detail = detail.text(" ");
            }
            detail = if word.len() == 12 && word.bytes().all(|b| b.is_ascii_hexdigit()) {
                // jj names the missing commit by id; the graph by change.
                let session = &ui.active().session;
                let name = session
                    .commits
                    .iter()
                    .find(|row| row.commit_id().starts_with(word))
                    .map(|row| {
                        session.revision_symbol(&session.canonical_selection(row.commit_id()))
                    })
                    .unwrap_or_else(|| word.to_owned());
                detail.rev(name)
            } else {
                detail.text(word)
            };
        }
    }
    let title_font = emphasis_font(font, Weight::MEDIUM);
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(crate::theme::space::XXL))
        .child(
            div()
                .max_w(px(440.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(crate::theme::space::SM))
                .child(typography::line(
                    "Not a connected range",
                    title_font,
                    text_size::TITLE,
                    theme.text,
                ))
                .child(
                    div()
                        .w_full()
                        .text_center()
                        .text_size(px(text_size::BODY))
                        .child(detail.view(font, mono, theme.muted_text, theme)),
                ),
        )
        .into_any_element()
}

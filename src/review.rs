//! What a tab reviews, in up to three parts: a header over the diff (what
//! the thing is), its steps in the left panel (the ordered pieces it's made
//! of), and the diff. The tab's source says which parts it has
//! ([`Tab::steps`]); the layout reads that and never asks what kind of tab
//! it is.
//!
//! A repository's steps are its revision graph. A source without a graph
//! (a pull request) loads an [`Outline`] instead: its header, and its steps
//! as a plain list, oldest first, under one scope item for the whole. That
//! item is the default selection; a step scopes the diff to itself.

use diffui_core::history::{now_millis, relative_age};
use diffui_core::{Outline, RevisionSelection, Step, Subject};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Div, SharedString, Window, div, px};

use crate::history_panel::{
    caption_fill, fill_on_line, header_bar, list_column, list_row, mono_on_line, two_line,
};
use crate::revision_list::{
    AGE_TEXT_SIZE, REVISION_ROW_HEIGHT, ROW_CARD_INSET_X, ROW_CARD_INSET_Y, SMALL_TEXT_SIZE,
};
use crate::theme::{ThemeSpec, Weight, emphasis_font, space, text_size};
use crate::typography::{self, Aligned};
use crate::ui::Look;
use crate::{Action, Cx, Diffui, Tab, TabSource, TabState, icons, ui};

/// How a tab's steps show in the left panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Steps {
    /// The revision graph (a repository).
    Graph,
    /// A numbered list, oldest first, under a scope item for the whole.
    List(StepsNames),
    /// Nothing to step through: no panel and no control for one.
    None,
}

/// What a list source calls its whole and its steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StepsNames {
    /// The scope item ("All changes").
    pub(crate) whole: &'static str,
    /// One step, capitalised ("Commit", for "Commit 2 of 4").
    pub(crate) step: &'static str,
    /// Several ("commits").
    pub(crate) steps: &'static str,
}

impl Tab {
    /// Which steps this tab has. A list source that loaded none has none.
    pub(crate) fn steps(&self) -> Steps {
        match &self.source {
            TabSource::Repo { .. } => Steps::Graph,
            TabSource::ForgePr(_) => match &self.state.session.outline {
                Some(outline) if outline.steps.is_empty() => Steps::None,
                _ => Steps::List(StepsNames {
                    whole: "All changes",
                    step: "Commit",
                    steps: "commits",
                }),
            },
        }
    }
}

impl Diffui {
    /// The active tab's steps; a graph's when no tab is open.
    pub(crate) fn active_steps(&self) -> Steps {
        self.tabs.get(self.active).map_or(Steps::Graph, Tab::steps)
    }
}

/// What a list source's diff covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The whole thing: the default selection.
    All,
    /// Step `n` (0-based, oldest first).
    Step(usize),
}

impl TabState {
    /// The outline's steps, oldest first; none for a repository.
    pub(crate) fn outline_steps(&self) -> &[Step] {
        self.session
            .outline
            .as_ref()
            .map_or(&[], |outline| outline.steps.as_slice())
    }

    /// What the diff covers, for a tab with an outline. The default
    /// selection is the whole; a step is selected by its id.
    pub(crate) fn scope(&self) -> Option<Scope> {
        let outline = self.session.outline.as_ref()?;
        Some(match &self.session.selected_revision {
            RevisionSelection::WorkingCopy => Scope::All,
            RevisionSelection::Commit(id) => {
                Scope::Step(outline.steps.iter().position(|step| &step.id == id)?)
            }
        })
    }
}

/// What selects `scope` of `outline`.
fn selection(outline: &Outline, scope: Scope) -> Option<RevisionSelection> {
    match scope {
        Scope::All => Some(RevisionSelection::WorkingCopy),
        Scope::Step(index) => Some(RevisionSelection::Commit(
            outline.steps.get(index)?.id.clone(),
        )),
    }
}

/// The text a header describes its thing with, and how it reads by
/// default: a pull request's description (or a cover letter) is Markdown,
/// rendered; a commit message is plain text, shown as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TextKind {
    Markdown,
    Message,
}

/// Whether each kind of text shows rendered as Markdown or as written.
/// Remembered across launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TextViews {
    pub(crate) markdown_raw: bool,
    pub(crate) messages_rendered: bool,
}

impl TextViews {
    pub(crate) fn rendered(&self, kind: TextKind) -> bool {
        match kind {
            TextKind::Markdown => !self.markdown_raw,
            TextKind::Message => self.messages_rendered,
        }
    }

    fn set(&mut self, kind: TextKind, rendered: bool) {
        match kind {
            TextKind::Markdown => self.markdown_raw = !rendered,
            TextKind::Message => self.messages_rendered = rendered,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReviewAction {
    /// Scope the diff to the whole, or to one step.
    Scope(Scope),
    /// The step `by` places after (or before) the one selected; from the
    /// whole, the first (or the last).
    Step { by: isize },
    /// Show a long description whole (`true`), or its first lines again.
    ShowAll(bool),
    /// Show this kind of text rendered (`true`) or as written.
    Rendered(TextKind, bool),
}

impl Diffui {
    pub(crate) fn perform_review(&mut self, action: ReviewAction, _cx: &mut Cx) {
        match action {
            ReviewAction::Scope(scope) => self.select_scope(scope),
            ReviewAction::Step { by } => {
                let tab = self.active();
                let count = tab.outline_steps().len();
                let next = match tab.scope() {
                    None => return,
                    Some(Scope::All) if by > 0 => Some(0),
                    Some(Scope::All) => count.checked_sub(1),
                    Some(Scope::Step(index)) => {
                        index.checked_add_signed(by).filter(|index| *index < count)
                    }
                };
                if let Some(index) = next {
                    self.select_scope(Scope::Step(index));
                }
            }
            ReviewAction::ShowAll(whole) => {
                let shown = whole
                    .then(|| crate::commit_info::shown_description(self))
                    .flatten();
                self.active_mut().description_whole = shown;
            }
            ReviewAction::Rendered(kind, rendered) => {
                self.text_views.set(kind, rendered);
                self.mark_geometry_dirty();
            }
        }
    }

    fn select_scope(&mut self, scope: Scope) {
        let Some(selection) = self
            .active()
            .session
            .outline
            .as_ref()
            .and_then(|outline| selection(outline, scope))
        else {
            return;
        };
        self.select_revision(selection);
    }
}

/// The number column of a step row: wide enough for "250", the most
/// commits a pull request lists.
const NUMBER_WIDTH: f32 = 24.0;

/// The steps panel of a list source: under its head, the scope item for the
/// whole, a break, then the steps numbered oldest first. Built from
/// History's list parts: its head, row cards, two-line rows and captions.
pub(crate) fn build_steps_panel(
    ui: &Diffui,
    names: StepsNames,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    let tab = ui.active();
    let steps = tab.outline_steps();
    let count = (!steps.is_empty()).then(|| steps.len().to_string());
    let mut title = names.steps.to_owned();
    title[..1].make_ascii_uppercase();
    let head = div().w_full().child(typography::baseline_row(
        [ui::panel_title(title, count, look)],
        0.0,
    ));
    let mut pane = div()
        .size_full()
        .flex()
        .flex_col()
        .bg(theme.panel_background)
        // No rule under the head: the list fades under it once scrolled.
        .child(header_bar(head, false, theme));
    let Some(outline) = tab.session.outline.as_ref() else {
        if let diffui_core::LoadStatus::Failed(error) = &tab.session.status {
            pane = pane.child(crate::empty_states::load_error(ui, look, error, cx));
        }
        return pane.into_any_element();
    };
    let scope = tab.scope();
    let now = now_millis();
    let mut list = div()
        .w_full()
        .flex()
        .flex_col()
        .pb(px(space::XL))
        .child(list_column().py(px(ROW_CARD_INSET_Y)).child(whole_row(
            ui,
            outline,
            names,
            scope == Some(Scope::All),
            look,
            cx,
        )))
        .child(rule(theme));
    for (index, step) in outline.steps.iter().enumerate() {
        let selected = scope == Some(Scope::Step(index));
        list = list.child(
            list_column()
                .py(px(ROW_CARD_INSET_Y))
                .child(step_row(ui, index, step, selected, now, look, cx)),
        );
    }
    let handle = ui::scroll_handle("review-steps-scroll", window, cx);
    let handle = handle.read(cx).clone();
    pane.child(div().flex_1().min_h_0().child(ui::scroll_list(
        "review-steps",
        &handle,
        ui::ScrollList {
            fade: Some(theme.panel_background),
            ..Default::default()
        },
        list,
    )))
    .into_any_element()
}

/// The hairline that sets the whole apart from its steps, as the Notes
/// list breaks its groups.
fn rule(theme: ThemeSpec) -> Div {
    div()
        .w_full()
        .h(px(space::LG))
        .flex_none()
        .flex()
        .items_center()
        .px(px(ROW_CARD_INSET_X))
        .child(div().w_full().h(px(1.0)).bg(theme.border))
}

/// `+a −d` in the diff's colours, at the meta line's size.
fn counts(additions: usize, deletions: usize, ui: &Diffui, look: Look) -> Aligned {
    let Look { theme, .. } = look;
    typography::group(
        [
            mono_on_line(
                format!("+{additions}"),
                crate::diff_panel::stat_color(additions, theme.added_text, theme),
                ui,
            ),
            mono_on_line(
                format!("\u{2212}{deletions}"),
                crate::diff_panel::stat_color(deletions, theme.removed_text, theme),
                ui,
            ),
        ],
        space::XS,
    )
}

/// The leading column of a row's first line: a step's number, or the
/// whole's glyph. With the line's gap after it, the text starts
/// [`NUMBER_WIDTH`] in.
fn lead(content: Aligned) -> Aligned {
    content.map(|element| {
        div()
            .flex_none()
            .w(px(NUMBER_WIDTH - space::MD))
            .child(element)
            .into_any_element()
    })
}

/// A row's second line, under the first line's text.
fn under_lead(meta: Vec<Aligned>) -> Div {
    typography::baseline_row(meta, space::SM).pl(px(NUMBER_WIDTH))
}

/// The scope item: the whole, with how many steps and files it holds and
/// its lines added and removed.
fn whole_row(
    ui: &Diffui,
    outline: &Outline,
    names: StepsNames,
    selected: bool,
    look: Look,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, font, .. } = look;
    let Subject::PullRequest(info) = &outline.subject;
    let title_font = emphasis_font(font, Weight::MEDIUM);
    let title = typography::baseline_row(
        [
            lead(Aligned::glyph(
                icons::icon(icons::FILE_DIFF, ui::ICON_SIZE, theme.muted_text),
                SMALL_TEXT_SIZE,
            )),
            fill_on_line(
                typography::line(names.whole, title_font, SMALL_TEXT_SIZE, theme.text)
                    .text_ellipsis(),
                title_font,
                SMALL_TEXT_SIZE,
            ),
        ],
        space::MD,
    );
    let steps = outline.steps.len();
    let one = names.step.to_lowercase();
    let files = match ui.active().scope() {
        Some(Scope::All) if !ui.active().session.document.files.is_empty() => {
            ui.active().session.document.files.len()
        }
        _ => info.changed_files,
    };
    let mut meta = vec![fill_on_line(
        caption_fill(
            format!(
                "{steps} {} \u{b7} {files} {}",
                if steps == 1 {
                    one.as_str()
                } else {
                    names.steps
                },
                if files == 1 { "file" } else { "files" }
            ),
            theme.subtle_text,
            ui,
        ),
        font,
        text_size::CAPTION,
    )];
    if let Some((additions, deletions)) = info.totals {
        meta.push(counts(additions, deletions, ui, look));
    }
    list_row(
        "review-scope-all",
        REVISION_ROW_HEIGHT,
        selected,
        Action::Review(ReviewAction::Scope(Scope::All)),
        theme,
        cx,
    )
    .child(two_line(title, under_lead(meta)))
    .into_any_element()
}

/// A step: its number, title and age; under them its short id, author and
/// lines added and removed.
fn step_row(
    ui: &Diffui,
    index: usize,
    step: &Step,
    selected: bool,
    now: i64,
    look: Look,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, font, .. } = look;
    let (summary, ink) = if step.title.trim().is_empty() {
        ("(no description set)".to_owned(), theme.subtle_text)
    } else {
        (step.title.clone(), theme.text)
    };
    let age = step
        .authored_millis
        .map(|millis| relative_age(millis, now))
        .unwrap_or_default();
    let line = typography::baseline_row(
        [
            lead(Aligned::text(
                typography::line(
                    (index + 1).to_string(),
                    font,
                    SMALL_TEXT_SIZE,
                    theme.subtle_text,
                ),
                font,
                SMALL_TEXT_SIZE,
            )),
            fill_on_line(
                typography::line(summary, font, SMALL_TEXT_SIZE, ink).text_ellipsis(),
                font,
                SMALL_TEXT_SIZE,
            ),
            Aligned::text(
                typography::line(age, font, AGE_TEXT_SIZE, theme.subtle_text),
                font,
                AGE_TEXT_SIZE,
            ),
        ],
        space::MD,
    );
    let mut meta = vec![
        mono_on_line(
            step.id
                .chars()
                .take(crate::sidebar::COMMIT_ID_CHARS)
                .collect::<String>(),
            theme.muted_text,
            ui,
        ),
        fill_on_line(
            caption_fill(step.author.clone(), theme.subtle_text, ui),
            font,
            text_size::CAPTION,
        ),
    ];
    if let Some((additions, deletions)) = step.counts {
        meta.push(counts(additions, deletions, ui, look));
    }
    list_row(
        SharedString::from(format!("review-step-{index}")),
        REVISION_ROW_HEIGHT,
        selected,
        Action::Review(ReviewAction::Scope(Scope::Step(index))),
        theme,
        cx,
    )
    .child(two_line(line, under_lead(meta)))
    .into_any_element()
}

#[cfg(test)]
#[path = "review_scenarios.rs"]
pub(crate) mod scenarios;

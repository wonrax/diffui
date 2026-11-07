//! How the palette looks as a command line: the input with its words
//! coloured in place, the revset arguments with their live counts (or the
//! error, pointed at), the completion list in the palette's own rows, and a
//! footer that says what Enter will do.

use crate::theme::{self, Color, Font, Padding, Weight, text_size};
use crate::typography::{self, Aligned};
use crate::{Cx, input, palette, ui};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, MouseButton, MouseMoveEvent, Window, div, px};
use std::ops::Range;

use diffui_core::cli::{CommandKind, tokenize};
use diffui_core::completion::{Completion, CompletionKind};

use crate::Diffui;
use crate::cmdline::{COMMAND_INPUT_ID, CommandLine, PlanState};
use crate::icons;
use crate::palette::{CONTENT_X, GLYPH_GAP, INPUT_HEIGHT, RESULTS_INSET};
use crate::theme::ThemeSpec;

/// The strip under the input: the revset arguments, or the error.
pub(crate) const ARGS_HEIGHT: f32 = 30.0;
/// How far kit's editor sets its text in from its left edge, gutter included.
const EDITOR_MARGIN: f32 = 11.0;

/// What a stretch of the line is, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Program,
    Subcommand,
    Flag,
    Revset,
    /// The revset the caret is in.
    FocusedRevset,
    Value,
    /// What the error on show points at.
    Error,
}

fn role_color(role: Role, theme: ThemeSpec) -> Color {
    match role {
        Role::Program => theme.subtle_text,
        Role::Flag => theme.muted_text,
        Role::Revset | Role::FocusedRevset => theme.accent,
        Role::Error => theme.removed_text,
        Role::Subcommand | Role::Value => theme.text,
    }
}

/// The line cut into runs that don't overlap, each with the role it is
/// drawn as: the error's span over everything, then the roles of the words.
fn painted_roles(ui: &Diffui, line: &str, command: &CommandLine) -> Vec<(Range<usize>, Role)> {
    let roles = roles(line, command);
    let error = error_span(ui, line, command);
    let mut cuts: Vec<usize> = vec![0, line.len()];
    for (range, _) in &roles {
        cuts.extend([range.start, range.end]);
    }
    if let Some(error) = &error {
        cuts.extend([error.start.min(line.len()), error.end.min(line.len())]);
    }
    cuts.retain(|at| *at <= line.len() && line.is_char_boundary(*at));
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter_map(|pair| {
            let (start, end) = (pair[0], pair[1]);
            let in_error = error
                .as_ref()
                .is_some_and(|error| error.start <= start && end <= error.end);
            let role = if in_error {
                Role::Error
            } else {
                roles
                    .iter()
                    .find(|(range, _)| range.start <= start && end <= range.end)
                    .map(|(_, role)| *role)?
            };
            Some((start..end, role))
        })
        .collect()
}

/// The spans the line colours as revset arguments.
#[cfg(test)]
pub(crate) fn revset_roles(line: &str, command: &CommandLine) -> Vec<Range<usize>> {
    roles(line, command)
        .into_iter()
        .filter(|(_, role)| matches!(role, Role::Revset | Role::FocusedRevset))
        .map(|(range, _)| range)
        .collect()
}

fn roles(line: &str, command: &CommandLine) -> Vec<(Range<usize>, Role)> {
    let tokens = tokenize::tokenize(line);
    let slots = command.slots();
    let focused = command.focused_slot();
    let path_len = command
        .parsed
        .as_ref()
        .map_or(1, |parsed| parsed.path.len().max(1));
    let mut roles: Vec<(Range<usize>, Role)> = Vec::new();
    for (index, slot) in slots.iter().enumerate() {
        let role = if Some(index) == focused {
            Role::FocusedRevset
        } else {
            Role::Revset
        };
        roles.push((slot.span.clone(), role));
    }
    let mut subcommands = 0;
    let mut after_flag = false;
    for (index, token) in tokens.iter().enumerate() {
        let span = token.span.clone();
        if slots.iter().any(|slot| overlaps(&slot.span, &span)) {
            // The flag half of `--onto=x`.
            if let Some(eq) = token.value.find('=')
                && token.value.starts_with('-')
            {
                roles.push((span.start..span.start + eq + 1, Role::Flag));
            }
            after_flag = false;
            continue;
        }
        if index == 0 && token.value == "jj" {
            roles.push((span, Role::Program));
            continue;
        }
        if token.value.starts_with('-') && token.value.len() > 1 {
            roles.push((span, Role::Flag));
            after_flag = !token.value.contains('=');
            continue;
        }
        if !after_flag && subcommands < path_len {
            subcommands += 1;
            roles.push((span, Role::Subcommand));
        } else {
            roles.push((span, Role::Value));
        }
        after_flag = false;
    }
    roles
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// The span the error underlines, once typing has paused.
fn error_span(ui: &Diffui, line: &str, command: &CommandLine) -> Option<Range<usize>> {
    match ui.plan(command).state {
        PlanState::Blocked { span, .. } if command.error_visible(line, span.as_ref()) => span,
        _ => None,
    }
}

pub(crate) fn completion_glyph(kind: CompletionKind, theme: ThemeSpec) -> (&'static str, Color) {
    match kind {
        CompletionKind::Subcommand => (icons::TERMINAL, theme.modified_token),
        CompletionKind::Flag => (icons::MINUS, theme.subtle_text),
        CompletionKind::Value => (icons::CHECK, theme.subtle_text),
        CompletionKind::Function | CompletionKind::Alias => (icons::CODE, theme.info),
        CompletionKind::Bookmark | CompletionKind::RemoteBookmark => {
            (icons::HASH, theme.modified_token)
        }
        CompletionKind::Tag => (icons::TAG, theme.modified_token),
        CompletionKind::Remote => (icons::FETCH, theme.info),
        CompletionKind::Revision => (icons::CIRCLE, theme.accent),
        CompletionKind::Path => (icons::FILE, theme.info),
        CompletionKind::History => (icons::HISTORY, theme.subtle_text),
    }
}

fn enter_label(kind: Option<CommandKind>) -> &'static str {
    match kind {
        Some(CommandKind::View) => "Show",
        Some(CommandKind::Mutation) => "Preview",
        Some(CommandKind::OpensUi) => "Open",
        Some(CommandKind::Passthrough) | None => "Run",
    }
}

/// The command line's input: a one-line editor in the code font, its words
/// coloured over the editor's own layout, so the text, the caret and the
/// selection can never drift apart.
pub(crate) fn input(
    ui: &Diffui,
    theme: ThemeSpec,
    query: &str,
    command: &CommandLine,
    focused: bool,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let mono = ui.config.mono_font;
    let colors = painted_roles(ui, query, command)
        .into_iter()
        .map(|(range, role)| (range, role_color(role, theme)))
        .collect();
    let mut spec = input::FieldSpec::new(COMMAND_INPUT_ID, query);
    if focused {
        let on_edit = Diffui::listen(cx, |this, (line, cursor): &(String, usize), _, cx| {
            this.palette_on_command_edit(line.clone(), *cursor, cx);
        });
        let on_submit = Diffui::listen(cx, |this, _: &(), _, cx| this.palette_on_accept(cx));
        spec = spec
            .on_edit(move |line, cursor, window, cx| on_edit(&(line, cursor), window, cx))
            .on_submit(move |window, cx| on_submit(&(), window, cx));
    }
    let look = input::StyledLook {
        submit_on_enter: true,
        colors,
        ..Default::default()
    };
    let line_height = text_size::TITLE * typography::DEFAULT_LINE_HEIGHT;
    div()
        .w_full()
        .flex_none()
        .h(px(INPUT_HEIGHT + 2.0 * RESULTS_INSET))
        // The editor keeps its own margin before the text.
        .pl(px(CONTENT_X - EDITOR_MARGIN))
        .pr(px(CONTENT_X))
        .pt(px((INPUT_HEIGHT + 2.0 * RESULTS_INSET - line_height) / 2.0))
        .font(mono.gpui())
        .child(
            input::styled_text(spec, look, mono, text_size::TITLE, window, cx)
                .with_size(gpui_kit::component::Size::XSmall)
                .line_height(px(line_height))
                .h(px(line_height)),
        )
        .into_any_element()
}

/// The strip under the input: each revset argument with its count, or the
/// error. Nothing when there's neither.
pub(crate) fn args_strip(
    ui: &Diffui,
    theme: ThemeSpec,
    line: &str,
    command: &CommandLine,
    _window: &mut Window,
    _cx: &mut Cx,
) -> Option<AnyElement> {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let plan = ui.plan(command);
    let blocked = match &plan.state {
        PlanState::Blocked { reason, span } if command.error_visible(line, span.as_ref()) => {
            Some(reason.clone())
        }
        _ => None,
    };
    let content = if let Some(reason) = blocked {
        typography::baseline_row(
            [
                Aligned::glyph(
                    icons::icon(icons::ALERT_TRIANGLE, 12.0, theme.removed_text),
                    12.0,
                ),
                Aligned::gap(theme::space::SM),
                sentence(
                    &crate::prose::Prose::from_backticks(&reason),
                    font,
                    mono,
                    theme.removed_text,
                    theme,
                ),
            ],
            0.0,
        )
    } else {
        let slots = command.slots();
        if slots.is_empty() {
            return None;
        }
        let focused = command.focused_slot();
        let mut parts: Vec<Aligned> = Vec::new();
        for (index, slot) in slots.iter().enumerate().take(4) {
            if index > 0 {
                parts.push(Aligned::gap(theme::space::LG));
            }
            let is_focused = Some(index) == focused;
            let flag = crate::cmdline::typed_flag(line, slot);
            if !flag.is_empty() {
                parts.push(Aligned::text(
                    typography::line(flag.to_owned(), mono, text_size::UI, theme.subtle_text),
                    mono,
                    text_size::UI,
                ));
                parts.push(Aligned::gap(theme::space::XS));
            }
            let shown: String = if slot.revset.chars().count() > 18 {
                let head: String = slot.revset.chars().take(17).collect();
                format!("{head}\u{2026}")
            } else {
                slot.revset.clone()
            };
            let color = if is_focused {
                theme.accent
            } else {
                theme.muted_text
            };
            parts.push(Aligned::text(
                typography::line(shown, mono, text_size::UI, color),
                mono,
                text_size::UI,
            ));
            parts.push(Aligned::gap(theme::space::SM));
            let (label, color) = match command.resolution(&slot.revset).map(|r| &r.result) {
                Some(Ok(found)) => (found.count_label(), theme.accent),
                Some(Err(_)) => ("no match".to_owned(), theme.removed_text),
                None => ("\u{2026}".to_owned(), theme.subtle_text),
            };
            parts.push(count_chip(label, font, text_size::CAPTION, color));
        }
        typography::baseline_row(parts, 0.0)
    };
    Some(
        div()
            .w_full()
            .flex_none()
            .h(px(ARGS_HEIGHT))
            .flex()
            .items_center()
            .px(px(CONTENT_X))
            .overflow_hidden()
            .child(content)
            .into_any_element(),
    )
}

/// The little tinted count or kind beside a word.
fn count_chip(
    label: impl Into<gpui_kit::SharedString>,
    font: Font,
    size: f32,
    color: Color,
) -> Aligned {
    Aligned::chip(
        typography::line(label, font, size, color),
        font,
        size,
        Padding::from([1.0, 6.0]),
        move |chip| {
            chip.bg(theme::chip_background(color))
                .rounded(px(theme::radius::CONTROL))
        },
    )
}

/// The candidates, in the palette's row style. With none, recent commands.
pub(crate) fn results(
    ui: &Diffui,
    theme: ThemeSpec,
    line: &str,
    command: &CommandLine,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if command.completions.is_empty() {
        let label = if line.trim_end() != "jj" {
            "No suggestions here"
        } else if ui.recents.cli.is_empty() {
            "Type a jj command, as in a terminal"
        } else {
            "Type a jj command, or \u{2191} for recent ones"
        };
        return palette::empty_results(ui, theme, label.to_owned()).into_any_element();
    }
    let mut list = div().w_full().flex().flex_col().p(px(RESULTS_INSET));
    for (index, completion) in command.completions.iter().enumerate() {
        let body = completion_row(ui, theme, completion, window, cx);
        list = list.child(
            div()
                .id(("command-row", index))
                .child(palette::row_shell(
                    body,
                    index == command.selected,
                    theme,
                    window,
                    cx,
                ))
                .on_mouse_move(
                    cx.listener(move |this: &mut Diffui, _: &MouseMoveEvent, _, cx| {
                        if this
                            .command_line()
                            .is_some_and(|command| command.selected != index)
                        {
                            this.act(cx, |this, _| this.palette_on_select_index(0, index));
                        }
                    }),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    Diffui::listen(cx, move |this, _, _, cx| {
                        this.palette_on_accept_index(0, index, cx);
                    }),
                ),
        );
    }
    let handle = palette::list_scroll("command-results-scroll", command.scroll_y, 0, window, cx);
    ui::scrollable_with("command-results", &handle, list).into_any_element()
}

/// One candidate: a glyph for its kind, the text it inserts, and its help.
pub(crate) fn completion_row(
    ui: &Diffui,
    theme: ThemeSpec,
    completion: &Completion,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let mono = ui.config.mono_font;
    let font = ui.config.ui_font;
    let (glyph, glyph_color) = completion_glyph(completion.kind, theme);
    let label_color = match completion.kind {
        CompletionKind::Revision => theme.accent,
        CompletionKind::Bookmark | CompletionKind::RemoteBookmark => theme.lane_base,
        _ => theme.text,
    };
    typography::baseline_row(
        [
            palette::glyph_icon(glyph, glyph_color, window, cx),
            Aligned::gap(GLYPH_GAP),
            Aligned::text(
                typography::line(completion.label.clone(), mono, text_size::BODY, label_color),
                mono,
                text_size::BODY,
            ),
            Aligned::gap(theme::space::MD),
            Aligned::text(
                palette::filling(
                    completion.detail.clone(),
                    font,
                    text_size::CAPTION,
                    theme.subtle_text,
                ),
                font,
                text_size::CAPTION,
            )
            .fill(),
        ],
        0.0,
    )
    .into_any_element()
}

/// The footer: the kind badge and what Enter does, in words.
pub(crate) fn footer(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &CommandLine,
    _window: &mut Window,
    _cx: &mut Cx,
) -> AnyElement {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let plan = ui.plan(command);
    let mut lead: Vec<Aligned> = Vec::new();
    if let Some(kind) = plan.kind {
        lead.push(badge(kind, font, theme));
        lead.push(Aligned::gap(theme::space::SM));
    }
    let blocked = matches!(plan.state, PlanState::Blocked { .. });
    let words = if plan.words.is_empty() {
        crate::prose::Prose::from(if blocked {
            "Fix the line to run it"
        } else {
            ""
        })
    } else {
        plan.words.clone()
    };
    lead.push(sentence(&words, font, mono, theme.text, theme));
    let (key, label, live) = match &plan.state {
        PlanState::Ready(_) => ("\u{21b5}", enter_label(plan.kind), true),
        PlanState::Pending => ("", "Resolving\u{2026}", false),
        PlanState::Blocked { .. } => ("\u{21b5}", enter_label(plan.kind), false),
    };
    lead.push(Aligned::gap(theme::space::LG));
    let hint_color = if live {
        theme.muted_text
    } else {
        theme::mix(theme.subtle_text, theme.panel_background, 0.4)
    };
    if !key.is_empty() {
        lead.push(Aligned::text(
            typography::line(key, font, text_size::CAPTION, hint_color),
            font,
            text_size::CAPTION,
        ));
        lead.push(Aligned::gap(theme::space::XS));
    }
    let label_color = if live { theme.subtle_text } else { hint_color };
    lead.push(Aligned::text(
        typography::line(label, font, text_size::CAPTION, label_color),
        font,
        text_size::CAPTION,
    ));
    palette::footer_strip(theme)
        .px(px(CONTENT_X))
        .child(typography::baseline_row(lead, 0.0))
        .into_any_element()
}

/// A one-line sentence that shares the row's baseline with its badge or
/// glyph, clipped at the row's end.
fn sentence(
    words: &crate::prose::Prose,
    font: Font,
    mono: Font,
    color: Color,
    theme: ThemeSpec,
) -> Aligned {
    typography::group(words.runs(font, mono, color, theme, text_size::UI), 0.0)
        .map(|line| {
            div()
                .w_full()
                .overflow_hidden()
                .child(line)
                .into_any_element()
        })
        .fill()
}

/// The kind badge: View, Preview, Opens editor, CLI.
pub(crate) fn badge(kind: CommandKind, font: Font, theme: ThemeSpec) -> Aligned {
    let color = match kind {
        CommandKind::View => theme.info,
        CommandKind::Mutation => theme.accent,
        CommandKind::OpensUi => theme.modified_token,
        CommandKind::Passthrough => theme.muted_text,
    };
    count_chip(
        kind.label(),
        theme::emphasis_font(font, Weight::MEDIUM),
        text_size::CAPTION,
        color,
    )
}

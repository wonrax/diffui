//! How the palette looks as a command line: the input with its words
//! coloured in place, the revset arguments with their live counts (or the
//! error, pointed at), the completion list in the palette's own rows, and a
//! footer that says what Enter will do.

use std::ops::Range;

use diffui_core::cli::{CommandKind, tokenize};
use diffui_core::completion::{Completion, CompletionKind};
use iced::advanced::text::{Ellipsis, Wrapping};
use iced::widget::text::Span;
use iced::widget::{column, container, mouse_area, rich_text, stack, text};
use iced::{Background, Border, Color, Element, Length, Padding};

use crate::cmdline::{CommandLine, PlanState};
use crate::icons;
use crate::palette::{self, CONTENT_X, GLYPH_GAP, INPUT_HEIGHT, PaletteMessage, RESULTS_INSET};
use crate::theme::{self, ThemeSpec, text_size};
use crate::typography::{self, Aligned};
use crate::{Diffui, Message};

/// The strip under the input: the revset arguments, or the error.
pub(crate) const ARGS_HEIGHT: f32 = 30.0;

/// The input, with the line's words coloured over it when it fits.
pub(crate) fn input<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    query: &'a str,
    command: &'a CommandLine,
    focused: bool,
) -> Element<'a, Message> {
    let mono = ui.config.mono_font;
    let padding = typography::trimmed_padding(
        Padding::from([10.0, CONTENT_X - RESULTS_INSET]),
        mono,
        text_size::TITLE,
    );
    let room = palette::COLUMN_WIDTH - 2.0 * RESULTS_INSET - padding.left - padding.right - 4.0;
    let overlay = crate::measure::line_width(query, text_size::TITLE, mono) < room;
    let mut field = iced::widget::text_input("", query)
        .id(palette::PALETTE_INPUT_ID)
        .padding(padding)
        .size(text_size::TITLE)
        .font(mono)
        .style(move |_, _| {
            let mut style = theme::input_style(theme);
            style.background = Background::Color(Color::TRANSPARENT);
            style.border = Border::default();
            if overlay {
                // The coloured copy underneath shows through.
                style.value = Color::TRANSPARENT;
            }
            style
        });
    if focused {
        field = field
            .on_input(|q| Message::Palette(PaletteMessage::QueryChanged(q)))
            .on_submit(Message::Palette(PaletteMessage::Accept));
    }
    let field = if overlay {
        crate::input::single_line_with_caret(field, query, mono, theme.text)
    } else {
        crate::input::single_line(field, query, mono)
    };
    let content: Element<'a, Message> = if overlay {
        let spans = colored_spans(ui, theme, query, command);
        let painted = container(
            rich_text(spans)
                .size(text_size::TITLE)
                .font(mono)
                .wrapping(Wrapping::None),
        )
        .padding(padding)
        .width(Length::Fill)
        .center_y(Length::Fill);
        stack![painted, field].into()
    } else {
        field
    };
    container(content)
        .padding(RESULTS_INSET)
        .height(Length::Fixed(INPUT_HEIGHT + 2.0 * RESULTS_INSET))
        .width(Length::Fill)
        .center_y(Length::Fixed(INPUT_HEIGHT + 2.0 * RESULTS_INSET))
        .into()
}

/// What a stretch of the line is, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Program,
    Subcommand,
    Flag,
    Revset,
    /// The revset the caret is in.
    FocusedRevset,
    Value,
}

/// The line cut into coloured runs: `jj`, the subcommand, flags, revset
/// arguments (the focused one washed), other values, and the error's span
/// underlined.
fn colored_spans<'a>(
    ui: &Diffui,
    theme: ThemeSpec,
    line: &str,
    command: &CommandLine,
) -> Vec<Span<'a, ()>> {
    let roles = roles(line, command);
    let error = error_span(ui, line, command);
    // Cut at every role change and at the error's edges.
    let mut cuts: Vec<usize> = vec![0, line.len()];
    for (range, _) in &roles {
        cuts.push(range.start);
        cuts.push(range.end);
    }
    if let Some(error) = &error {
        cuts.push(error.start.min(line.len()));
        cuts.push(error.end.min(line.len()));
    }
    cuts.retain(|at| line.is_char_boundary(*at));
    cuts.sort_unstable();
    cuts.dedup();
    let mut spans = Vec::new();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start == end {
            continue;
        }
        let role = roles
            .iter()
            .find(|(range, _)| range.start <= start && end <= range.end)
            .map(|(_, role)| *role);
        let mut span = Span::new(line[start..end].to_owned()).color(match role {
            Some(Role::Program) => theme.subtle_text,
            Some(Role::Subcommand) => theme.text,
            Some(Role::Flag) => theme.muted_text,
            Some(Role::Revset | Role::FocusedRevset) => theme.accent,
            Some(Role::Value) | None => theme.text,
        });
        if role == Some(Role::FocusedRevset) {
            span = span.background(theme::chip_background(theme.accent));
        }
        if let Some(error) = &error
            && error.start <= start
            && end <= error.end.max(error.start + 1)
        {
            span = span.color(theme.removed_text).underline(true);
        }
        spans.push(span);
    }
    // An error at the very end (a missing value) has nothing to underline:
    // mark the spot.
    if let Some(error) = &error
        && error.start >= line.len()
    {
        spans.push(
            Span::new("\u{2038}".to_owned())
                .color(theme.removed_text)
                .underline(false),
        );
    }
    spans
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

/// The strip under the input: each revset argument with its count, or the
/// error. Nothing when there's neither.
pub(crate) fn args_strip<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    line: &str,
    command: &'a CommandLine,
) -> Option<Element<'a, Message>> {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let plan = ui.plan(command);
    let blocked = match &plan.state {
        PlanState::Blocked { reason, span } if command.error_visible(line, span.as_ref()) => {
            Some(reason.clone())
        }
        _ => None,
    };
    let content: Element<'a, Message> = if let Some(reason) = blocked {
        typography::baseline_row(
            [
                Aligned::glyph(
                    icons::icon(icons::ALERT_TRIANGLE, 12.0, theme.removed_text),
                    12.0,
                ),
                Aligned::gap(theme::space::SM),
                Aligned::text(
                    crate::prose::Prose::from_backticks(&reason)
                        .view(font, mono, theme.removed_text, theme)
                        .size(text_size::UI)
                        .wrapping(Wrapping::None)
                        .ellipsis(Ellipsis::End)
                        .width(Length::Fill),
                    font,
                    text_size::UI,
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
        let mut parts: Vec<Aligned<'a, Message>> = Vec::new();
        for (index, slot) in slots.iter().enumerate().take(4) {
            if index > 0 {
                parts.push(Aligned::gap(theme::space::LG));
            }
            let is_focused = Some(index) == focused;
            let flag = crate::cmdline::typed_flag(line, slot);
            if !flag.is_empty() {
                parts.push(Aligned::text(
                    text(flag)
                        .size(text_size::UI)
                        .font(mono)
                        .color(theme.subtle_text),
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
            parts.push(Aligned::text(
                text(shown)
                    .size(text_size::UI)
                    .font(mono)
                    .color(if is_focused {
                        theme.accent
                    } else {
                        theme.muted_text
                    })
                    .wrapping(Wrapping::None),
                mono,
                text_size::UI,
            ));
            parts.push(Aligned::gap(theme::space::SM));
            let (label, color) = match command.resolution(&slot.revset).map(|r| &r.result) {
                Some(Ok(found)) => (found.count_label(), theme.accent),
                Some(Err(_)) => ("no match".to_owned(), theme.removed_text),
                None => ("\u{2026}".to_owned(), theme.subtle_text),
            };
            parts.push(Aligned::chip(
                text(label).size(text_size::CAPTION).font(font).color(color),
                font,
                text_size::CAPTION,
                Padding::from([1.0, 6.0]),
                move |_| crate::chip::container_style(color),
            ));
        }
        typography::baseline_row(parts, 0.0)
    };
    Some(
        container(content)
            .padding(Padding {
                top: 0.0,
                right: CONTENT_X,
                bottom: 0.0,
                left: CONTENT_X,
            })
            .width(Length::Fill)
            .height(Length::Fixed(ARGS_HEIGHT))
            .center_y(Length::Fixed(ARGS_HEIGHT))
            .clip(true)
            .into(),
    )
}

/// The candidates, in the palette's row style. With none, recent commands.
pub(crate) fn results<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    line: &str,
    command: &'a CommandLine,
) -> Element<'a, Message> {
    if command.completions.is_empty() {
        let label = if line.trim_end() != "jj" {
            "No suggestions here"
        } else if ui.recents.cli.is_empty() {
            "Type a jj command, as in a terminal"
        } else {
            "Type a jj command, or \u{2191} for recent ones"
        };
        return container(
            text(label)
                .size(text_size::BODY)
                .color(theme.subtle_text)
                .font(ui.config.ui_font),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .padding([24, 16])
        .style(move |_| container::Style {
            background: Some(Background::Color(theme.panel_background_elevated)),
            ..container::Style::default()
        })
        .into();
    }
    let mut list = column![].spacing(0);
    for (index, completion) in command.completions.iter().enumerate() {
        let selected = index == command.selected;
        list = list.push(
            mouse_area(palette::row_shell(
                completion_row(ui, theme, completion),
                selected,
                theme,
            ))
            .on_press(Message::Palette(PaletteMessage::AcceptIndex(0, index)))
            .on_enter(Message::Palette(PaletteMessage::SelectIndex(0, index))),
        );
    }
    crate::ui::scrollable(list.padding(RESULTS_INSET), theme)
        .id(palette::results_scrollable_id(0))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// One candidate: a glyph for its kind, the text it inserts, and its help.
pub(crate) fn completion_row<'a>(
    ui: &Diffui,
    theme: ThemeSpec,
    completion: &Completion,
) -> Element<'a, Message> {
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
            palette::glyph_icon(glyph, glyph_color),
            Aligned::gap(GLYPH_GAP),
            Aligned::text(
                text(completion.label.clone())
                    .size(text_size::BODY)
                    .font(mono)
                    .color(label_color)
                    .wrapping(Wrapping::None),
                mono,
                text_size::BODY,
            ),
            Aligned::gap(theme::space::MD),
            Aligned::text(
                text(completion.detail.clone())
                    .size(text_size::CAPTION)
                    .font(font)
                    .color(theme.subtle_text)
                    .width(Length::Fill)
                    .wrapping(Wrapping::None)
                    .ellipsis(Ellipsis::End),
                font,
                text_size::CAPTION,
            ),
        ],
        0.0,
    )
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

/// The footer: the kind badge and what Enter does, in words.
pub(crate) fn footer<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    command: &'a CommandLine,
) -> Element<'a, Message> {
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let plan = ui.plan(command);
    let mut lead: Vec<Aligned<'a, Message>> = Vec::new();
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
    lead.push(Aligned::text(
        words
            .view(font, mono, theme.text, theme)
            .size(text_size::UI)
            .wrapping(Wrapping::None)
            .ellipsis(Ellipsis::End)
            .width(Length::Fill),
        font,
        text_size::UI,
    ));
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
            text(key)
                .size(text_size::CAPTION)
                .font(font)
                .color(hint_color),
            font,
            text_size::CAPTION,
        ));
        lead.push(Aligned::gap(theme::space::XS));
    }
    lead.push(Aligned::text(
        text(label)
            .size(text_size::CAPTION)
            .font(font)
            .color(if live { theme.subtle_text } else { hint_color }),
        font,
        text_size::CAPTION,
    ));
    container(typography::baseline_row(lead, 0.0))
        .padding([0.0, CONTENT_X])
        .width(Length::Fill)
        .height(Length::Fixed(palette::FOOTER_HEIGHT))
        .center_y(Length::Fixed(palette::FOOTER_HEIGHT))
        .style(move |_| palette::footer_style(theme))
        .into()
}

fn enter_label(kind: Option<CommandKind>) -> &'static str {
    match kind {
        Some(CommandKind::View) => "Show",
        Some(CommandKind::Mutation) => "Preview",
        Some(CommandKind::OpensUi) => "Open",
        Some(CommandKind::Passthrough) | None => "Run",
    }
}

/// The kind badge: View, Preview, Opens editor, CLI.
pub(crate) fn badge<'a>(
    kind: CommandKind,
    font: iced::Font,
    theme: ThemeSpec,
) -> Aligned<'a, Message> {
    let color = match kind {
        CommandKind::View => theme.info,
        CommandKind::Mutation => theme.accent,
        CommandKind::OpensUi => theme.modified_token,
        CommandKind::Passthrough => theme.muted_text,
    };
    Aligned::chip(
        text(kind.label())
            .size(text_size::CAPTION)
            .font(theme::emphasis_font(font, iced::font::Weight::Medium))
            .color(color),
        theme::emphasis_font(font, iced::font::Weight::Medium),
        text_size::CAPTION,
        Padding::from([1.0, 6.0]),
        move |_| crate::chip::container_style(color),
    )
}

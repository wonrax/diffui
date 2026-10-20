//! How the palette looks as a command line: the input with its words
//! coloured in place, the revset arguments with their live counts (or the
//! error, pointed at), the completion list in the palette's own rows, and a
//! footer that says what Enter will do.

use std::ops::Range;

use diffui_core::cli::{CommandKind, tokenize};
use diffui_core::completion::{Completion, CompletionKind};
use iced::advanced::text::{Ellipsis, Wrapping};
use iced::widget::{column, container, mouse_area, text};
use iced::{Background, Border, Color, Element, Length, Padding};

use crate::cmdline::{CommandLine, PlanState};
use crate::icons;
use crate::palette::{self, CONTENT_X, GLYPH_GAP, INPUT_HEIGHT, PaletteMessage, RESULTS_INSET};
use crate::theme::{self, ThemeSpec, text_size};
use crate::typography::{self, Aligned};
use crate::{Diffui, Message};

/// The strip under the input: the revset arguments, or the error.
pub(crate) const ARGS_HEIGHT: f32 = 30.0;

/// The command line's input: a one-line editor in the code font, its words
/// coloured by a highlighter over the editor's own layout, so the text, the
/// caret and the selection can never drift apart.
pub(crate) fn input<'a>(
    ui: &'a Diffui,
    theme: ThemeSpec,
    query: &'a str,
    command: &'a CommandLine,
    focused: bool,
) -> Element<'a, Message> {
    use iced::widget::text_editor;
    let mono = ui.config.mono_font;
    let line_height = text_size::TITLE * typography::DEFAULT_LINE_HEIGHT;
    // The box the search input's line sits in: the line centred in the band,
    // its caps optically centred on the line.
    let padding = typography::trimmed_padding(
        Padding::from([
            (INPUT_HEIGHT - line_height) / 2.0,
            CONTENT_X - RESULTS_INSET,
        ]),
        mono,
        text_size::TITLE,
    );
    let mut editor = text_editor(&command.editor.0)
        .id(iced::widget::Id::new(crate::cmdline::COMMAND_INPUT_ID))
        .font(mono)
        .size(text_size::TITLE)
        .padding(padding)
        .height(Length::Fixed(INPUT_HEIGHT))
        .wrapping(Wrapping::None)
        .key_binding(|press| {
            use iced::keyboard::{Key, key::Named};
            let press = crate::ui::cocoa_key_press(press, false);
            match press.key.as_ref() {
                // Enter runs (or takes a candidate); the palette's own keys
                // move through the list and history.
                Key::Named(Named::Enter) => Some(text_editor::Binding::Custom(Message::Palette(
                    PaletteMessage::Accept,
                ))),
                Key::Named(Named::ArrowUp | Named::ArrowDown | Named::Tab | Named::Escape) => None,
                _ => text_editor::Binding::from_key_press(press),
            }
        })
        .style(move |_, _| {
            let input = theme::input_style(theme);
            text_editor::Style {
                background: Background::Color(Color::TRANSPARENT),
                border: Border::default(),
                placeholder: input.placeholder,
                value: input.value,
                selection: input.selection,
            }
        })
        .highlight_with::<RoleParser>(
            painted_roles(ui, query, command),
            move |role: Role, _: &iced::Theme| iced::advanced::text::highlighter::Style {
                color: Some(role_color(role, theme)),
                style: None,
            },
        );
    if focused {
        editor = editor.on_action(|action| Message::Palette(PaletteMessage::CommandEdit(action)));
    }
    container(editor)
        .padding(RESULTS_INSET)
        .height(Length::Fixed(INPUT_HEIGHT + 2.0 * RESULTS_INSET))
        .width(Length::Fill)
        .center_y(Length::Fixed(INPUT_HEIGHT + 2.0 * RESULTS_INSET))
        .into()
}

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

/// Feeds the editor's highlighter the roles worked out for its one line.
pub(crate) struct RoleParser {
    roles: Vec<(Range<usize>, Role)>,
    line: usize,
}

impl iced::advanced::text::Parser for RoleParser {
    type Settings = Vec<(Range<usize>, Role)>;
    type Output = Role;
    type Iterator<'a> = std::vec::IntoIter<(Range<usize>, Role)>;

    fn new(settings: &Self::Settings) -> Self {
        Self {
            roles: settings.clone(),
            line: 0,
        }
    }

    fn update(&mut self, settings: &Self::Settings) {
        self.roles = settings.clone();
        self.line = 0;
    }

    fn change_line(&mut self, line: usize) {
        self.line = self.line.min(line);
    }

    fn parse_line(&mut self, line: &str) -> Self::Iterator<'_> {
        let first = self.line == 0;
        self.line += 1;
        let roles = if first {
            self.roles
                .iter()
                .filter(|(range, _)| range.end <= line.len())
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        roles.into_iter()
    }

    fn current_line(&self) -> usize {
        self.line
    }
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

/// A one-line sentence that shares the row's baseline with its badge or
/// glyph, clipped at the row's end.
fn sentence<'a>(
    words: &crate::prose::Prose,
    font: iced::Font,
    mono: iced::Font,
    color: Color,
    theme: ThemeSpec,
) -> Aligned<'a, Message> {
    typography::group(words.runs(font, mono, color, theme, text_size::UI), 0.0)
        .map(|line| container(line).width(Length::Fill).clip(true).into())
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

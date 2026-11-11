//! How the palette looks as a command line: the input with its words
//! coloured in place, the revset arguments with their live counts (or the
//! error, pointed at), the completion list in the palette's own rows, and a
//! footer that says what Enter will do.

use crate::root::PaneSet;
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
use crate::cmdline::{COMMAND_INPUT_ID, CommandLine, PREFIX, PlanState};
use crate::icons;
use crate::palette::{CONTENT_X, GLYPH_GAP, INPUT_BAND, RESULTS_INSET};
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
            (crate::overlay_kit::glyph::BOOKMARK, theme.modified_token)
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

/// The command line's input: the `jj` it always starts with as a solid chip,
/// then a one-line editor in the code font holding the rest, its words
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
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    // The field holds what follows the `jj ` the chip stands for; offsets into
    // the line move by its length.
    let shown = query.strip_prefix(PREFIX).unwrap_or(query);
    let offset = query.len() - shown.len();
    let colors = painted_roles(ui, query, command)
        .into_iter()
        .filter_map(|(range, role)| {
            let start = range.start.max(offset);
            (range.end > start)
                .then(|| (start - offset..range.end - offset, role_color(role, theme)))
        })
        .collect();
    let mut spec =
        input::FieldSpec::new(COMMAND_INPUT_ID, shown).placeholder("Subcommand and arguments");
    if focused {
        let on_edit = Diffui::listen(cx, |this, (line, cursor): &(String, usize), _, cx| {
            this.command_line_edit(format!("{PREFIX}{line}"), cursor + PREFIX.len(), cx);
        });
        let on_submit = Diffui::listen(cx, |this, _: &(), _, cx| this.palette_submit(cx));
        spec = spec
            .on_edit(move |line, cursor, window, cx| on_edit(&(line, cursor), window, cx))
            .on_submit(move |window, cx| on_submit(&(), window, cx));
    }
    let look = input::StyledLook {
        submit_on_enter: true,
        colors,
        ..Default::default()
    };
    let line_height = text_size::BODY_LG * typography::DEFAULT_LINE_HEIGHT;
    div()
        .w_full()
        .flex_none()
        .h(px(INPUT_BAND))
        .flex()
        .flex_row()
        .items_center()
        .px(px(CONTENT_X))
        .font(mono.gpui())
        .child(prefix_chip(mono, theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                // The editor keeps its own margin before the text.
                .ml(px(GLYPH_GAP - EDITOR_MARGIN))
                .child(
                    input::styled_text(spec, look, mono, text_size::BODY_LG, window, cx)
                        .with_size(gpui_kit::component::Size::XSmall)
                        .line_height(px(line_height))
                        .h(px(line_height)),
                ),
        )
        .child(crate::overlay_kit::keycap("esc", font, theme))
        .into_any_element()
}

/// The solid `jj` the command line starts with.
fn prefix_chip(mono: Font, theme: ThemeSpec) -> impl IntoElement {
    let mono = theme::emphasis_font(mono, Weight::SEMIBOLD);
    div()
        .flex_none()
        .h(px(22.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .rounded(px(theme::radius::XS))
        .bg(theme.accent)
        .child(typography::centered(
            typography::line(
                "jj",
                mono,
                text_size::BODY,
                theme::primary_button_style(theme).label,
            ),
            mono,
            text_size::BODY,
        ))
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
                    icons::icon(
                        icons::ALERT_TRIANGLE,
                        crate::ui::ICON_SIZE,
                        theme.removed_text,
                    ),
                    crate::ui::ICON_SIZE,
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
                    typography::line(flag.to_owned(), mono, text_size::BODY, theme.subtle_text),
                    mono,
                    text_size::BODY,
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
                typography::line(shown, mono, text_size::BODY, color),
                mono,
                text_size::BODY,
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
    let size = typography::optical_size(font, size);
    Aligned::chip(
        typography::line(label, font, size, color),
        font,
        size,
        Padding::from([1.0, 6.0]),
        move |chip| {
            chip.bg(theme::chip_background(color))
                .rounded(px(theme::radius::XS))
        },
    )
}

/// Above the candidates, the command that would run; then the candidates in
/// the palette's row style, grouped by kind. With none, a hint.
pub(crate) fn results(
    ui: &Diffui,
    theme: ThemeSpec,
    line: &str,
    command: &CommandLine,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let preview = preview(ui, theme, line, command);
    let list = if command.completions.is_empty() {
        let label = if line.trim_end() != "jj" {
            "No suggestions here"
        } else if ui.recents.cli.is_empty() {
            "Type a jj command, as in a terminal"
        } else {
            "Type a jj command, or \u{2191} for recent ones"
        };
        palette::empty_results(ui, theme, label.to_owned()).into_any_element()
    } else {
        let font = ui.config.ui_font;
        let handle =
            palette::list_scroll("command-results-scroll", command.scroll_y, 0, window, cx);
        let items = palette::visible_results(
            command
                .completions
                .iter()
                .map(|completion| Some(completion_group(completion.kind))),
            &handle,
            palette::RESULTS_VIEWPORT_HEIGHT,
            palette::ROW_HEIGHT,
            |heading| palette::group_heading(heading, font, theme).into_any_element(),
            |index| {
                let completion = &command.completions[index];
                let selected = index == command.selected;
                let body = candidate_row(ui, theme, completion, selected, window, cx);
                div()
                    .id(("command-row", index))
                    .child(palette::row_shell(body, selected, theme, window, cx))
                    .on_mouse_move(cx.listener(
                        move |this: &mut Diffui, _: &MouseMoveEvent, _, cx| {
                            if this
                                .command_line()
                                .is_some_and(|command| command.selected != index)
                            {
                                this.act_on(cx, PaneSet::FLOATS, |this, _| {
                                    this.palette_on_select_index(0, index)
                                });
                            }
                        },
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        Diffui::listen(cx, move |this, _, _, cx| {
                            this.palette_on_accept_index(0, index, cx);
                        }),
                    )
                    .into_any_element()
            },
        );
        let list = div()
            .w_full()
            .flex()
            .flex_col()
            .p(px(RESULTS_INSET))
            .children(items);
        ui::scrollable_with("command-results", &handle, list).into_any_element()
    };
    div()
        .size_full()
        .flex()
        .flex_col()
        .children(preview)
        .child(div().flex_1().min_h_0().w_full().child(list))
        .into_any_element()
}

/// The heading a candidate lists under.
pub(crate) fn completion_group(kind: CompletionKind) -> &'static str {
    match kind {
        CompletionKind::Subcommand => "Subcommands",
        CompletionKind::Flag => "Flags",
        CompletionKind::Value => "Values",
        CompletionKind::Function | CompletionKind::Alias => "Functions",
        CompletionKind::Bookmark | CompletionKind::RemoteBookmark => "Bookmarks",
        CompletionKind::Tag => "Tags",
        CompletionKind::Remote => "Remotes",
        CompletionKind::Revision => "Revisions",
        CompletionKind::Path => "Paths",
        CompletionKind::History => "Recent",
    }
}

/// How much of the list's height the preview takes, card and inset, for the
/// keyboard's scroll math.
pub(crate) const PREVIEW_BLOCK: f32 = 66.0;

/// The command Enter would run, in full: the line as typed with the
/// highlighted candidate ghosted in where it would go, what kind of command it
/// is, and what it will do in words. Nothing on a bare `jj ` with nothing to
/// offer.
fn preview(ui: &Diffui, theme: ThemeSpec, line: &str, command: &CommandLine) -> Option<AnyElement> {
    let completion = command.completions.get(command.selected);
    if line.trim_end() == "jj" && completion.is_none() {
        return None;
    }
    let font = ui.config.ui_font;
    let mono = ui.config.mono_font;
    let plan = ui.plan(command);
    let line = line.trim_end_matches(['\n', '\r']);
    let mut spans: Vec<crate::ellipsis::Span<'_>> = Vec::new();
    // What a candidate would add, ghosted in. The kind and the words are the
    // typed line's, so they show only while there's none: they'd describe
    // something other than the line on show.
    let mut ghosted = false;
    match completion {
        Some(completion) => {
            let start = completion.replace.start.min(line.len());
            let end = completion.replace.end.clamp(start, line.len());
            let typed = line.get(start..end).unwrap_or("");
            // What's typed of the candidate stays plain; the rest is ghosted.
            let kept = if completion.insert.starts_with(typed) {
                typed.len()
            } else {
                0
            };
            spans.push(crate::ellipsis::Span::new(
                line.get(..start).unwrap_or(line),
            ));
            spans.push(crate::ellipsis::Span::new(&completion.insert[..kept]));
            ghosted = kept < completion.insert.len();
            spans.push(
                crate::ellipsis::Span::new(&completion.insert[kept..]).color(theme.subtle_text),
            );
            spans.push(crate::ellipsis::Span::new(line.get(end..).unwrap_or("")));
        }
        None => spans.push(crate::ellipsis::Span::new(line)),
    }
    let mut head: Vec<Aligned> = Vec::new();
    if let Some(kind) = plan.kind.filter(|_| !ghosted) {
        head.push(badge(kind, font, theme));
        head.push(Aligned::gap(theme::space::MD));
    }
    head.push(
        Aligned::text(
            crate::overlay_kit::rich_line(mono, text_size::BODY, theme.text)
                .w_full()
                .overflow_hidden()
                .text_ellipsis()
                .child(crate::ellipsis::rich_text(&spans, mono, theme.text)),
            mono,
            text_size::BODY,
        )
        .fill(),
    );
    let mut card = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(theme::space::XS))
        .px(px(theme::space::LG))
        .py(px(10.0))
        .map(|card| {
            theme::squircle(
                card,
                theme::radius::MD,
                Some(crate::overlay_kit::inset_fill(theme)),
                Some(theme.border),
            )
        })
        .child(typography::baseline_row(head, 0.0));
    if !ghosted && !plan.words.is_empty() {
        card = card.child(typography::baseline_row(
            [sentence(&plan.words, font, mono, theme.muted_text, theme)],
            0.0,
        ));
    }
    Some(
        div()
            .w_full()
            .flex_none()
            .px(px(RESULTS_INSET))
            .pt(px(RESULTS_INSET))
            .child(card)
            .into_any_element(),
    )
}

/// One candidate: a glyph for its kind, the text it inserts, and its help.
pub(crate) fn completion_row(
    ui: &Diffui,
    theme: ThemeSpec,
    completion: &Completion,
    selected: bool,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    candidate_row(ui, theme, completion, selected, window, cx)
}

/// [`completion_row`], the highlighted one showing the key that takes it.
fn candidate_row(
    ui: &Diffui,
    theme: ThemeSpec,
    completion: &Completion,
    selected: bool,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let mono = ui.config.mono_font;
    let font = ui.config.ui_font;
    let (glyph, _) = completion_glyph(completion.kind, theme);
    let label_color = match completion.kind {
        CompletionKind::Revision => theme.accent,
        CompletionKind::Bookmark | CompletionKind::RemoteBookmark => theme.lane_base,
        _ => theme.text,
    };
    let text = typography::baseline_row(
        [
            palette::glyph_icon(glyph, palette::glyph_ink(selected, theme), window, cx),
            Aligned::gap(GLYPH_GAP),
            Aligned::text(
                typography::line(completion.label.clone(), mono, text_size::BODY, label_color)
                    .overflow_hidden()
                    .text_ellipsis(),
                mono,
                text_size::BODY,
            )
            .shrink(),
            Aligned::gap(theme::space::MD),
            Aligned::text(
                palette::filling(
                    completion.detail.clone(),
                    font,
                    text_size::BODY,
                    theme.subtle_text,
                ),
                font,
                text_size::BODY,
            )
            .fill(),
        ],
        0.0,
    )
    .flex_1()
    .min_w_0();
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::LG))
        .child(text)
        .when(selected, |row| {
            row.child(crate::overlay_kit::keycap("\u{21e5}", font, theme))
        })
        .into_any_element()
}

/// The footer: the keys of the command line, and where what runs ends up.
pub(crate) fn footer(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &CommandLine,
    _window: &mut Window,
    _cx: &mut Cx,
) -> AnyElement {
    let font = ui.config.ui_font;
    let plan = ui.plan(command);
    let enter = match &plan.state {
        PlanState::Pending => "Resolving\u{2026}",
        PlanState::Ready(_) | PlanState::Blocked { .. } => enter_label(plan.kind),
    };
    let lands = match plan.kind {
        Some(CommandKind::View) => "Shows it in diffui",
        Some(CommandKind::Mutation) => "Previews it before it runs",
        Some(CommandKind::OpensUi) => "Opens the editor",
        Some(CommandKind::Passthrough) | None => "Output opens in the CLI panel",
    };
    let hint =
        |keys: &[&str], label: &'static str| crate::overlay_kit::key_hint(keys, label, font, theme);
    crate::overlay_kit::footer(
        crate::overlay_kit::Density::Strip,
        crate::overlay_kit::MODAL_FOOT,
        theme,
    )
    .child(hint(&["\u{21e5}"], "Complete"))
    .child(hint(&["\u{21b5}"], enter))
    .children(crate::overlay_kit::bound_hint(
        ui,
        crate::commands::Context::Palette,
        &["palette.leave-command-line"],
        "Back to commands",
        theme,
    ))
    .child(crate::overlay_kit::footer_note(lands, font, theme).ml_auto())
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
    typography::group(words.runs(font, mono, color, theme, text_size::BODY), 0.0)
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

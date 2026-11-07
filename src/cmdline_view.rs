//! How the palette looks as a command line: the input with its words
//! coloured in place, the revset arguments with their live counts (or the
//! error, pointed at), the completion list in the palette's own rows, and a
//! footer that says what Enter will do.

use crate::Cx;
use crate::theme::Color;
use gpui_kit::{AnyElement, Window};
use std::ops::Range;

use diffui_core::cli::{CommandKind, tokenize};
use diffui_core::completion::{Completion, CompletionKind};

use crate::Diffui;
use crate::cmdline::{CommandLine, PlanState};
use crate::icons;
use crate::theme::ThemeSpec;

/// The strip under the input: the revset arguments, or the error.
pub(crate) const ARGS_HEIGHT: f32 = 30.0;

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

/// Stub: the cmdline_view view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn input(
    ui: &Diffui,
    theme: ThemeSpec,
    query: &str,
    command: &CommandLine,
    focused: bool,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("cmdline_view::input")
}

/// Stub: the cmdline_view view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn args_strip(
    ui: &Diffui,
    theme: ThemeSpec,
    line: &str,
    command: &CommandLine,
    window: &mut Window,
    cx: &mut Cx,
) -> Option<AnyElement> {
    None
}

/// Stub: the cmdline_view view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn results(
    ui: &Diffui,
    theme: ThemeSpec,
    line: &str,
    command: &CommandLine,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("cmdline_view::results")
}

/// Stub: the cmdline_view view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn completion_row(
    ui: &Diffui,
    theme: ThemeSpec,
    completion: &Completion,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("cmdline_view::completion_row")
}

/// Stub: the cmdline_view view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn footer(
    ui: &Diffui,
    theme: ThemeSpec,
    command: &CommandLine,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("cmdline_view::footer")
}

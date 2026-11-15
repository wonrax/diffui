//! The commit info block: what a revision is, above its diff.
//!
//! Above the diff sits a bar with the revision's ids, the change's size and
//! the edit and fold controls. The rest scrolls with the diff, drawn by the
//! diff view as its header ([`header_lines`]): the title, wrapped to the
//! pane, then the description's body in mono, then one column of fields:
//! author, date, change and commit ids in the graph's colours with copy
//! buttons, parents as links, and the refs as the graph draws them. The
//! History view's diffs carry the same fields. Folded, the header is the
//! title alone.

use diffui_core::history::{human_timestamp, now_millis, parse_timestamp_millis};
use diffui_core::{RevisionDetails, SignatureInfo};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use std::rc::Rc;

use diffui_core::Subject;
use diffui_core::forge::{ForgeKind, PrInfo};

use crate::diff_view::{DocSpot, HeaderAction, HeaderField, HeaderLine, Inset};
use crate::meta_line;
use crate::review::{ReviewAction, Scope, TextKind};
use crate::sidebar::{COMMIT_ID_CHARS, REVISION_ID_CHARS, revision_id_display_len};
use crate::theme::{self, Color, ThemeSpec, Weight, emphasis_font, radius, space, text_size};
use crate::typography::Aligned;
use crate::ui::Look;
use crate::ui::Tip as _;
use crate::{Action, Cx, Diffui, icons, typography, ui};

/// The title's step: the revision list's description size.
const TITLE_SIZE: f32 = text_size::BODY;
/// Between the header's title and what follows it.
const TITLE_GAP: f32 = 8.0;
/// Ids and counts, a step under the title.
const META_SIZE: f32 = text_size::BODY;
/// The diffstat: this many cells, each this square, this far apart.
const STAT_CELLS: usize = 5;
const STAT_CELL: f32 = 6.0;
const STAT_CELL_GAP: f32 = 1.5;
/// A trigger's wash reaches this far past its text.
const TRIGGER_PAD: [f32; 2] = [3.0, 6.0];

/// A count short enough for the stats line: thousands abbreviated past
/// four digits.
fn compact_count(count: usize) -> String {
    match count {
        0..10_000 => count.to_string(),
        10_000..1_000_000 => format!("{}k", count / 1_000),
        _ => format!("{}M", count / 1_000_000),
    }
}

/// How many of the diffstat's cells go to additions and to deletions, the
/// rest neutral, as GitHub draws it: each side's share of five, rounded, a
/// non-zero side getting at least one.
fn stat_cells(additions: usize, deletions: usize) -> (usize, usize) {
    let total = additions + deletions;
    if total == 0 {
        return (0, 0);
    }
    let share = |count: usize| {
        let cells = (count as f64 / total as f64 * STAT_CELLS as f64).round() as usize;
        if count > 0 { cells.max(1) } else { 0 }
    };
    let (mut added, mut removed) = (share(additions), share(deletions));
    while added + removed > STAT_CELLS {
        if added > removed {
            added -= 1;
        } else {
            removed -= 1;
        }
    }
    (added, removed)
}

/// How much of a row's id is its shortest unique prefix: jj's own count,
/// else the one the graph computed (git hashes), else the default id width.
fn unique_prefix(ui: &Diffui, row: Option<diffui_core::RowView<'_>>) -> usize {
    row.and_then(|row| {
        row.shortest_change_id_len().or_else(|| {
            ui.active()
                .session
                .sidebar_prefix_lens
                .get(row.index())
                .copied()
        })
    })
    .filter(|&len| len > 0)
    .unwrap_or(REVISION_ID_CHARS)
}

/// What the header over a diff says, whatever the tab views: the title,
/// the facts as a column of fields, then the description (what follows the
/// title). Each source fills it in its own terms ([`revision_header`],
/// [`pull_request_header`]); [`header_lines`] lays any of them out.
#[derive(Debug, Clone)]
pub(crate) struct Header {
    /// `None`: it has none, and the header says so.
    pub(crate) title: Option<String>,
    pub(crate) facts: Vec<HeaderLine>,
    pub(crate) description: String,
    pub(crate) kind: TextKind,
}

/// How many lines of a long description show until "Show all".
pub(crate) const FOLD_LINES: usize = 10;

/// The header of the diff on screen: the revision's, the pull request's
/// whole, or the step's. `None` with nothing to describe yet, and for a
/// range (which [`crate::range`] heads).
pub(crate) fn header(ui: &Diffui, look: Look) -> Option<Header> {
    let tab = ui.active();
    if let (Some(outline), Some(Scope::All)) = (&tab.session.outline, tab.scope()) {
        let Subject::PullRequest(info) = &outline.subject;
        let kind = match &ui.tabs.get(ui.active)?.source {
            crate::TabSource::ForgePr(spec) => spec.kind,
            crate::TabSource::Repo { .. } => return None,
        };
        return Some(pull_request_header(kind, info, look));
    }
    if let Some(details) = &tab.session.revision_details {
        return Some(revision_header(ui, details, look));
    }
    // A step whose diff hasn't brought its details: what the outline says.
    let Some(Scope::Step(index)) = tab.scope() else {
        return None;
    };
    let step = tab.outline_steps().get(index)?;
    let details = RevisionDetails {
        commit_id: step.id.clone(),
        author: SignatureInfo {
            name: step.author.clone(),
            ..SignatureInfo::default()
        },
        description: step.message.clone(),
        ..RevisionDetails::default()
    };
    Some(revision_header(ui, &details, look))
}

/// The hash the header's description is known by: what "Show all" keeps
/// whole.
pub(crate) fn shown_description(ui: &Diffui) -> Option<u64> {
    let theme = ui.resolved_theme().spec();
    header(ui, ui.look(theme)).map(|header| text_hash(&header.description))
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// A pull request's header: its title, its number with its state, who
/// opened it, its branches, and its description in Markdown.
pub(crate) fn pull_request_header(kind: ForgeKind, info: &PrInfo, look: Look) -> Header {
    let Look { theme, .. } = look;
    let reference = format!("{}{}", kind.sigil(), info.number);
    let mut number = HeaderField::new(kind.title_noun(), reference.clone());
    number.runs = vec![(0, theme.accent)];
    if !info.url.is_empty() {
        number.link = Some((
            reference.chars().count(),
            HeaderAction::OpenUrl(info.url.clone()),
        ));
        number.buttons = vec![(icons::COPY, HeaderAction::Copy(info.url.clone()))];
    }
    let (glyph, color) = crate::forge::state_mark(info.state, theme);
    number.chips = vec![crate::chip::Chip {
        label: info.state.label().to_owned(),
        font: look.mono,
        background: crate::theme::chip_background(color),
        text_color: color,
        border_color: None,
        border_dashed: false,
        icon: Some(glyph),
        icon_color: None,
        suffix: None,
        marks: Vec::new(),
        tip: None,
    }];
    let mut facts = vec![rich(number)];
    if !info.author.is_empty() {
        facts.push(rich(HeaderField::new("Author", info.author.clone())));
    }
    if !info.head_ref.is_empty() {
        let head = info.head_ref.chars().count();
        let mut branches = HeaderField::new(
            "Branches",
            format!("{} \u{2192} {}", info.head_ref, info.base_ref),
        );
        branches.runs = vec![
            (0, theme.accent),
            (head, theme.muted_text),
            (head + 3, theme.accent),
        ];
        facts.push(rich(branches));
    }
    Header {
        title: Some(info.title.clone()).filter(|title| !title.trim().is_empty()),
        facts,
        description: info.body.clone(),
        kind: TextKind::Markdown,
    }
}

/// A revision's header: its title, then author, date, ids, parents, refs
/// and pull requests as fields, then the rest of its message.
pub(crate) fn revision_header(ui: &Diffui, details: &RevisionDetails, look: Look) -> Header {
    let Look { theme, .. } = look;
    let title = details
        .description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned);
    let description = details
        .description
        .trim()
        .split_once('\n')
        .map_or("", |(_, body)| body)
        .trim()
        .to_owned();
    let mut lines = Vec::new();
    lines.push(rich(signature_field("Author", &details.author, theme)));
    if let Some(timestamp) = details
        .author
        .timestamp
        .as_deref()
        .filter(|timestamp| !timestamp.is_empty())
    {
        let exact = human_timestamp(timestamp).unwrap_or_else(|| timestamp.to_owned());
        let mut date = HeaderField::new("Date", exact.clone());
        if let Some(millis) = parse_timestamp_millis(timestamp) {
            date.value = format!("{exact}  {}", meta_line::age_phrase(millis, now_millis()));
            date.runs = vec![(0, theme.text), (exact.chars().count(), theme.subtle_text)];
        }
        lines.push(rich(date));
    }
    // Only when someone else committed it: the same person again, a few
    // minutes later, is noise.
    if let Some(committer) = details.committer.as_ref().filter(|committer| {
        committer.name != details.author.name || committer.email != details.author.email
    }) {
        lines.push(rich(signature_field("Committer", committer, theme)));
    }
    if let Some(signature) = &details.signature {
        lines.push(rich(HeaderField::new("Signature", signature.clone())));
    }

    let session = &ui.active().session;
    let row_of = session.commits.find_by_commit_id(&details.commit_id);
    if let Some(change_id) = details.change_id.as_deref() {
        let unique = unique_prefix(ui, row_of);
        let mut value = change_id.to_owned();
        // How jj log addresses one copy of a divergent or hidden change.
        if let Some(offset) = row_of.and_then(|row| row.change_offset()) {
            value.push_str(&format!("/{offset}"));
        }
        let mut change = HeaderField::new("Change", value);
        change.runs = vec![(0, theme.accent), (unique, theme.subtle_text)];
        if let Some(row) = row_of {
            change.chips = crate::sidebar::status_chips_for(row, look);
        }
        change.buttons = vec![(icons::COPY, HeaderAction::Copy(change_id.to_owned()))];
        lines.push(rich(change));
    }
    // A pull-request tab's header names it by its number: `#12`, or `!12`
    // for a merge request.
    let pull_request = details.change_id.is_none() && details.commit_id.starts_with(['#', '!']);
    let mut commit = HeaderField::new(
        match details.commit_id.chars().next() {
            Some('#') if pull_request => "Pull request",
            Some('!') if pull_request => "Merge request",
            _ => "Commit",
        },
        details.commit_id.clone(),
    );
    commit.runs = if details.change_id.is_some() {
        // jj: the short commit id stands out, as the graph shows it.
        vec![(0, theme.muted_text), (COMMIT_ID_CHARS, theme.subtle_text)]
    } else if pull_request {
        vec![(0, theme.muted_text)]
    } else {
        // Git: the hash is the revision's one id, coloured as the graph
        // colours it.
        vec![
            (0, theme.accent),
            (unique_prefix(ui, row_of), theme.subtle_text),
        ]
    };
    commit.buttons = vec![(icons::COPY, HeaderAction::Copy(details.commit_id.clone()))];
    lines.push(rich(commit));

    for (index, parent) in details.parents.iter().enumerate() {
        let parent_row = session.commits.find_by_commit_id(&parent.commit_id);
        let unique = unique_prefix(ui, parent_row);
        let mut id: String = match parent.change_id.as_deref() {
            Some(change_id) => change_id
                .chars()
                .take(revision_id_display_len(unique, change_id))
                .collect(),
            None => parent.commit_id.chars().take(COMMIT_ID_CHARS).collect(),
        };
        if let Some(offset) = parent_row.and_then(|row| row.change_offset()) {
            id.push_str(&format!("/{offset}"));
        }
        let root = !parent.commit_id.is_empty() && parent.commit_id.chars().all(|c| c == '0');
        let subject = if root {
            "root()".to_owned()
        } else if !parent.summary.is_empty() {
            parent.summary.clone()
        } else {
            parent_row
                .map(|row| {
                    row.description()
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned()
                })
                .filter(|summary| !summary.is_empty())
                .unwrap_or_else(|| "(no description set)".to_owned())
        };
        let id_chars = id.chars().count();
        let label = match (index, details.parents.len()) {
            (0, 1) => "Parent",
            (0, _) => "Parents",
            _ => "",
        };
        let mut field = HeaderField::new(label, format!("{id}  {subject}"));
        field.runs = vec![
            (0, theme.accent),
            (unique.min(id_chars), theme.subtle_text),
            (id_chars, theme.muted_text),
        ];
        field.link = Some((id_chars, HeaderAction::Open(parent.commit_id.clone())));
        lines.push(rich(field));
    }
    // The refs as the graph draws them on this revision's row, lane colour
    // and all; a revision off the graph shows its bookmarks in the accent.
    let chips = match row_of {
        Some(row) => crate::sidebar::graph_ref_chips(session, row.index(), look),
        None => crate::sidebar::bookmark_chips_for(&details.bookmarks, theme.accent, look),
    };
    if !chips.is_empty() {
        lines.push(HeaderLine::Bookmarks {
            label: if details.change_id.is_some() {
                "Bookmarks"
            } else {
                "Refs"
            }
            .to_owned(),
            chips,
        });
    }
    if let Some(row) = row_of {
        lines.extend(pull_request_fields(ui, row.bookmarks(), look));
    }
    Header {
        title,
        facts: lines,
        description,
        kind: TextKind::Message,
    }
}

/// The diff view's header for `header`: the title (when `foldable`: the
/// diff pane's; the History view's own headers already name the revision),
/// the facts, then the description. Folded, it's the title alone.
///
/// The description reads as written (mono lines, wrapped to `pane_width`
/// with line wrap on) or, with `inset` and its kind shown rendered, as
/// Markdown in the header's inset ([`header_inset`]). A long one shows its
/// first lines and "Show all" there.
pub(crate) fn header_lines(
    ui: &Diffui,
    header: &Header,
    look: Look,
    foldable: bool,
    pane_width: f32,
    inset: bool,
) -> Vec<HeaderLine> {
    let mut lines = if foldable {
        title_lines(ui, header.title.as_deref(), pane_width)
    } else {
        Vec::new()
    };
    if foldable && ui.commit_details_collapsed {
        return lines;
    }
    if !lines.is_empty() {
        lines.push(HeaderLine::spacer(TITLE_GAP));
    }
    lines.extend(header.facts.iter().cloned());
    let text = header.description.trim();
    if text.is_empty() {
        return lines;
    }
    lines.push(HeaderLine::blank());
    if inset && ui.text_views.rendered(header.kind) {
        lines.push(HeaderLine::Inset);
        return lines;
    }
    let mut body = raw_lines(ui, text, look, pane_width);
    if inset && body.len() > FOLD_LINES && !shown_whole(ui, header) {
        body.truncate(FOLD_LINES);
        lines.extend(body.iter().map(|line| HeaderLine::description(line)));
        lines.push(HeaderLine::Inset);
        return lines;
    }
    let long = inset && body.len() > FOLD_LINES;
    lines.extend(body.iter().map(|line| HeaderLine::description(line)));
    if long {
        lines.push(HeaderLine::Inset);
    }
    lines
}

/// `text` as the header's mono lines: wrapped at spaces to the pane with
/// line wrap on, so a header line is one row.
fn raw_lines(ui: &Diffui, text: &str, look: Look, pane_width: f32) -> Vec<String> {
    let columns = ui.diff_wrap.then(|| {
        let size = ui.config.code_type.size;
        let char_width = crate::measure::line_width("0000000000", size, look.mono) / 10.0;
        let text_width = pane_width
            - 2.0 * crate::diff_view::HEADER_HORIZONTAL_PADDING
            - crate::scrollbar::GUTTER;
        (text_width / char_width.max(1.0)).floor().max(20.0) as usize
    });
    text.lines()
        .flat_map(|line| match columns {
            Some(columns) => wrap_columns(line, columns),
            None => vec![line.to_owned()],
        })
        .collect()
}

/// Whether the header's description was asked to show whole.
fn shown_whole(ui: &Diffui, header: &Header) -> bool {
    ui.active().description_whole == Some(text_hash(&header.description))
}

/// The first lines of a Markdown text: about [`FOLD_LINES`], cut back to
/// the paragraph break before them when there's one.
fn markdown_head(text: &str) -> String {
    let lines: Vec<&str> = text.lines().take(FOLD_LINES).collect();
    let end = lines
        .iter()
        .rposition(|line| line.trim().is_empty())
        .filter(|&blank| blank >= FOLD_LINES / 2)
        .unwrap_or(lines.len());
    lines[..end].join("\n")
}

/// A button's press, shared with each build of an inset.
type OnClick = Rc<dyn Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App)>;

/// What goes in the header's [`HeaderLine::Inset`]: the description
/// rendered as Markdown, and "Show all" (or "Show less") when it's long.
pub(crate) fn header_inset(
    ui: &Diffui,
    header: &Header,
    look: Look,
    pane_width: f32,
    cx: &mut Cx,
) -> Option<Inset> {
    let text = header.description.trim();
    if text.is_empty() || ui.commit_details_collapsed {
        return None;
    }
    let rendered = ui.text_views.rendered(header.kind);
    let long = if rendered {
        text.lines().count() > FOLD_LINES
    } else {
        raw_lines(ui, text, look, pane_width).len() > FOLD_LINES
    };
    let whole = shown_whole(ui, header);
    if !rendered && !long {
        return None;
    }
    let shown = rendered.then(|| {
        if long && !whole {
            markdown_head(text)
        } else {
            text.to_owned()
        }
    });
    let toggle = long.then(|| {
        let on: OnClick = Rc::new(Diffui::on_action(
            Action::Review(ReviewAction::ShowAll(!whole)),
            cx,
        ));
        (if whole { "Show less" } else { "Show all" }, on)
    });
    let key = {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (text, rendered, long, whole).hash(&mut hasher);
        hasher.finish()
    };
    Some(Inset {
        at: DocSpot::Header,
        key,
        build: Rc::new(move |_, _| {
            let mut column = div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(space::SM))
                .pl(px(crate::diff_view::HEADER_HORIZONTAL_PADDING))
                .pr(px(
                    crate::diff_view::HEADER_HORIZONTAL_PADDING + crate::scrollbar::GUTTER
                ));
            if let Some(text) = &shown {
                column = column.child(crate::ai::rendered_markdown("review-header", text, look));
            }
            if let Some((label, on)) = &toggle {
                let on = on.clone();
                // The label's text, not its wash, lines up with the text
                // above it.
                column = column.child(
                    div().flex().ml(px(-ui::Size::Small.pad_x())).child(
                        look.button("review-header-show-all", *label)
                            .ghost()
                            .small()
                            .on_click(move |event, window, cx| on(event, window, cx)),
                    ),
                );
            }
            column.into_any_element()
        }),
    })
}

/// The pull requests of the revision's bookmarks: each a link to it with its
/// state, then its checks with a link to them.
fn pull_request_fields(ui: &Diffui, bookmarks: &[String], look: Look) -> Vec<HeaderLine> {
    let Look { theme, .. } = look;
    let forge = &ui.active().forge;
    let mut shown: Vec<&str> = Vec::new();
    let mut lines = Vec::new();
    for decoration in bookmarks
        .iter()
        .filter_map(|label| crate::forge::local_name(label))
        .filter_map(|name| forge.of(name))
    {
        if shown.contains(&decoration.pr.url.as_str()) {
            continue;
        }
        shown.push(&decoration.pr.url);
        let reference = decoration.reference();
        let id_chars = reference.chars().count();
        let mut field = HeaderField::new(
            decoration.remote.kind.title_noun(),
            format!("{reference}  {}", decoration.pr.title),
        );
        field.runs = vec![(0, theme.accent), (id_chars, theme.muted_text)];
        field.link = Some((id_chars, HeaderAction::OpenUrl(decoration.pr.url.clone())));
        let (glyph, color) = crate::forge::state_mark(decoration.pr.state, theme);
        field.chips = vec![crate::chip::Chip {
            label: decoration.pr.state.label().to_owned(),
            font: look.mono,
            background: crate::theme::chip_background(color),
            text_color: color,
            border_color: None,
            border_dashed: false,
            icon: Some(glyph),
            icon_color: None,
            suffix: None,
            marks: Vec::new(),
            tip: None,
        }];
        field.buttons = vec![(icons::COPY, HeaderAction::Copy(decoration.pr.url.clone()))];
        lines.push(rich(field));
        let Some(checks) = decoration
            .checks
            .as_ref()
            .filter(|checks| checks.state != diffui_core::forge::CheckState::None)
        else {
            continue;
        };
        let mut field = HeaderField::new("Checks", checks.state.phrase());
        if let Some((_, color)) = crate::forge::checks_mark(checks.state, theme) {
            field.runs = vec![(0, color)];
        }
        if let Some(url) = &checks.url {
            field.buttons = vec![(icons::EXTERNAL_LINK, HeaderAction::OpenUrl(url.clone()))];
        }
        lines.push(rich(field));
    }
    lines
}

/// What the diff view's header links and buttons do.
pub(crate) fn header_action(action: HeaderAction) -> Action {
    match action {
        HeaderAction::Copy(value) => Action::Copy(value),
        HeaderAction::Open(commit_id) => {
            Action::History(crate::history_panel::HistoryAction::OpenRevision(commit_id))
        }
        HeaderAction::OpenUrl(url) => Action::OpenUrl(url),
    }
}

/// `line` broken at spaces into rows of at most `columns` characters; a
/// word longer than a row is cut. Continuation rows keep the line's indent,
/// so a wrapped list item still hangs under its text.
fn wrap_columns(line: &str, columns: usize) -> Vec<String> {
    let line = line.trim_end();
    if line.chars().count() <= columns {
        return vec![line.to_owned()];
    }
    let indent: String = line.chars().take_while(|c| *c == ' ').collect();
    let hang = if indent.len() * 2 < columns {
        indent.as_str()
    } else {
        ""
    };
    let mut rows = Vec::new();
    let mut row = indent.clone();
    for word in line.split(' ').filter(|word| !word.is_empty()) {
        let mut word = word.to_owned();
        loop {
            let used = row.chars().count();
            let fresh = row.trim().is_empty();
            let needed = word.chars().count() + usize::from(!fresh);
            if used + needed <= columns {
                if !fresh {
                    row.push(' ');
                }
                row.push_str(&word);
                break;
            }
            if !fresh {
                rows.push(std::mem::replace(&mut row, hang.to_owned()));
                continue;
            }
            // A word wider than a whole row: cut it.
            let room = columns.saturating_sub(used).max(1);
            let head: String = word.chars().take(room).collect();
            word = word.chars().skip(room).collect();
            row.push_str(&head);
            rows.push(std::mem::replace(&mut row, hang.to_owned()));
            if word.is_empty() {
                break;
            }
        }
    }
    if !row.trim().is_empty() {
        rows.push(row);
    }
    rows
}

fn rich(field: HeaderField) -> HeaderLine {
    HeaderLine::Rich(Box::new(field))
}

/// "Name <email>", the address quieter.
fn signature_field(label: &str, signature: &SignatureInfo, theme: ThemeSpec) -> HeaderField {
    let name = if signature.name.is_empty() {
        "Unknown"
    } else {
        signature.name.as_str()
    };
    let mut field = HeaderField::new(label, name);
    if !signature.email.is_empty() {
        field.value = format!("{name} <{}>", signature.email);
        field.runs = vec![(0, theme.text), (name.chars().count(), theme.subtle_text)];
    }
    field
}

/// The header bar over the diff: which revision it is (its ids), how big the
/// change is, and what can be done to it. With a panel hidden, the
/// ids and the file count open it over the diff (see [`crate::root`]).
pub(crate) fn view(ui: &Diffui, look: Look, _window: &mut Window, cx: &mut Cx) -> AnyElement {
    let Look { theme, .. } = look;
    let session = &ui.active().session;
    let font = look.font;
    let has_files = !session.document.files.is_empty();
    let title_font = emphasis_font(font, Weight::SEMIBOLD);
    let title = |title: String, color: Color| {
        Aligned::text(
            typography::line(title, title_font, TITLE_SIZE, color).text_ellipsis(),
            title_font,
            TITLE_SIZE,
        )
        .shrink()
    };

    // A range has no one revision to describe: its title counts the
    // commits, which the list under it names.
    if let Some(range) = crate::range::shown_range(ui) {
        let mut line = vec![title(crate::range::title(range), theme.text)];
        if has_files {
            line.extend(diff_stats(ui, look, cx));
        }
        return block(theme).child(bar(line, Vec::new())).into_any_element();
    }
    let scope = ui.active().scope();
    let mut line = match (scope, session.revision_details.as_ref()) {
        (Some(scope), _) => outline_ids(ui, scope, look, cx),
        (None, Some(details)) => vec![revision_ids(ui, details, look, cx)],
        (None, None) => Vec::new(),
    };
    if has_files {
        line.extend(diff_stats(ui, look, cx));
    }
    let Some(header) = header(ui, look) else {
        return block(theme).child(bar(line, Vec::new())).into_any_element();
    };

    let mut actions = Vec::new();
    if session.capabilities.mutate {
        actions.push(
            look.button("commit-edit", "Describe")
                .compact()
                .icon(icons::PENCIL, theme.muted_text)
                .on_click(Diffui::on_action(
                    Action::EditDescription { target: None },
                    cx,
                ))
                .tip(crate::toolbar::with_shortcut(
                    ui,
                    "Edit description",
                    "revision.edit-description",
                ))
                .into_any_element(),
        );
    }
    if let Some(Scope::Step(index)) = scope {
        let count = ui.active().outline_steps().len();
        let noun = steps_names(ui).map_or("step", |names| names.step);
        for (id, icon, by, enabled, tip) in [
            (
                "review-step-previous",
                icons::CHEVRON_UP,
                -1,
                index > 0,
                "Previous",
            ),
            (
                "review-step-next",
                icons::CHEVRON_DOWN,
                1,
                index + 1 < count,
                "Next",
            ),
        ] {
            actions.push(
                look.icon_button(id, icon, theme.muted_text)
                    .disabled(!enabled)
                    .on_click(Diffui::on_action(
                        Action::Review(ReviewAction::Step { by }),
                        cx,
                    ))
                    .tip(format!("{tip} {}", noun.to_lowercase()))
                    .into_any_element(),
            );
        }
    }
    let folded = ui.commit_details_collapsed;
    if !folded && !header.description.trim().is_empty() {
        let kind = header.kind;
        actions.push(crate::ai::view_toggle(
            look,
            "review-header-view",
            ["Rendered", "Raw"],
            !ui.text_views.rendered(kind),
            move |raw| Action::Review(ReviewAction::Rendered(kind, !raw)),
            cx,
        ));
    }
    actions.push(
        look.icon_button(
            "commit-fold",
            if folded {
                icons::UNFOLD_VERTICAL
            } else {
                icons::FOLD_VERTICAL
            },
            theme.muted_text,
        )
        .on_click(Diffui::on_action(Action::ToggleCommitDetails, cx))
        .tip(if folded {
            "Show the description and details"
        } else {
            "Show just the title"
        })
        .into_any_element(),
    );
    if let Some(more) = more_button(ui, look, cx) {
        actions.push(more);
    }

    block(theme).child(bar(line, actions)).into_any_element()
}

/// Where the bar's first text starts: the diff header's own inset, so the
/// title sits over the description under it.
const BAR_LEAD: f32 = crate::diff_view::HEADER_HORIZONTAL_PADDING;
/// The bar's inset after its last button: the button's glyph, not its wash,
/// ends on the diff pane's trailing edge, the file headers' glyphs' line.
const BAR_TRAIL: f32 =
    crate::diff_view::HEADER_HORIZONTAL_PADDING - (theme::control::COMPACT - ui::ICON_SIZE) / 2.0;

/// The bar's ground. No rule under it: the sticky file header's own edge
/// separates the diff scrolling under it, as the side panes' fades do theirs.
fn block(theme: ThemeSpec) -> gpui_kit::Div {
    div()
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(theme.panel_background)
}

/// One row: `line` on one baseline, cut where it runs out of room, then the
/// buttons.
fn bar(line: Vec<Aligned>, actions: Vec<AnyElement>) -> gpui_kit::Div {
    ui::panel_header()
        .gap(px(space::XXS))
        .pl(px(BAR_LEAD))
        .pr(px(BAR_TRAIL))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                // The clip reaches past the line by the triggers' washes, which
                // spill out of the line box, so they aren't cut top and bottom.
                .py(px(TRIGGER_PAD[0]))
                .my(px(-TRIGGER_PAD[0]))
                .px(px(TRIGGER_PAD[1]))
                .mx(px(-TRIGGER_PAD[1]))
                .child(typography::baseline_row(line, space::MD)),
        )
        .children(actions)
}

/// `content` as a control that opens a hidden panel over the diff, when
/// `panel` is hidden; plain otherwise. The wash reaches past the text without
/// moving it, so the line doesn't shift between the two.
fn panel_trigger(
    id: &'static str,
    content: Vec<Aligned>,
    panel: Option<crate::root::Panel>,
    tooltip: &'static str,
    ui: &Diffui,
    look: Look,
    cx: &mut Cx,
) -> Aligned {
    let Look { theme, .. } = look;
    let group = typography::group(content, space::MD);
    let Some(panel) = panel else {
        return group;
    };
    let open = ui.panel_peek.is_some_and(|peek| peek.panel == panel);
    let content = vec![
        group,
        Aligned::glyph(
            icons::icon(icons::CHEVRON_DOWN, 11.0, theme.muted_text),
            META_SIZE,
        ),
    ];
    let (anchor, tracker) = crate::menu::anchor_tracker();
    let on_click = Diffui::listen(cx, move |this, _: &gpui_kit::ClickEvent, _, _| {
        this.toggle_panel_peek(panel, anchor.get());
    });
    trigger_with(id, content, open, tooltip, on_click, Some(tracker), look)
}

/// `content` as a control in the bar's line: a wash on hover (and while
/// `open`) that reaches past the text without moving it.
fn trigger(
    id: &'static str,
    content: Vec<Aligned>,
    open: bool,
    tooltip: &'static str,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
    look: Look,
) -> Aligned {
    trigger_with(
        id,
        content,
        open,
        tooltip,
        on_click,
        None::<gpui_kit::Div>,
        look,
    )
}

fn trigger_with(
    id: &'static str,
    content: Vec<Aligned>,
    open: bool,
    tooltip: &'static str,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
    tracker: Option<impl IntoElement + 'static>,
    look: Look,
) -> Aligned {
    let Look { theme, .. } = look;
    let [py, px_] = TRIGGER_PAD;
    typography::group(content, 3.0).map(move |line| {
        div()
            .id(id)
            .test_support()
            .relative()
            .flex()
            .py(px(py))
            .my(px(-py))
            .px(px(px_))
            .mx(px(-px_))
            .rounded(px(radius::XS))
            .cursor_pointer()
            .when(open, |trigger| trigger.bg(theme::press_wash(theme)))
            .hover(move |trigger| trigger.bg(theme::hover_wash(theme)))
            .tip(tooltip, look)
            .on_click(on_click)
            .children(tracker)
            .child(line)
            .into_any_element()
    })
}

/// Which panel a trigger opens, if that panel is hidden.
fn hidden(ui: &Diffui, panel: crate::root::Panel) -> Option<crate::root::Panel> {
    let tab = ui.active();
    let hidden = tab.main_view == crate::MainView::Diff
        && match panel {
            crate::root::Panel::Revisions => {
                tab.history_panel_collapsed && ui.active_steps() != crate::review::Steps::None
            }
            crate::root::Panel::Files => tab.files_panel_collapsed,
        };
    hidden.then_some(panel)
}

/// What a list source's bar names: the pull request's number on the
/// whole; on a step, where it is ("All changes / Commit 2 of 4", the first
/// part leading back to the whole) and its short id.
fn outline_ids(ui: &Diffui, scope: Scope, look: Look, cx: &mut Cx) -> Vec<Aligned> {
    let Look { theme, .. } = look;
    let (mono, font) = (look.mono, look.font);
    let text = |value: String, font, color: Color| {
        Aligned::text(
            typography::line(value, font, META_SIZE, color),
            font,
            META_SIZE,
        )
    };
    let tab = &ui.tabs[ui.active];
    let names = steps_names(ui);
    let panel = crate::root::Panel::Revisions;
    match scope {
        Scope::All => {
            let crate::TabSource::ForgePr(spec) = &tab.source else {
                return Vec::new();
            };
            vec![panel_trigger(
                "commit-revisions-trigger",
                vec![text(
                    format!("{}{}", spec.kind.sigil(), spec.number),
                    mono,
                    theme.muted_text,
                )],
                hidden(ui, panel),
                "Show commits",
                ui,
                look,
                cx,
            )]
        }
        Scope::Step(index) => {
            let steps = tab.state.outline_steps();
            let Some(step) = steps.get(index) else {
                return Vec::new();
            };
            let whole = names.map_or("All changes", |names| names.whole);
            let back = trigger(
                "review-scope-all",
                vec![text(whole.to_owned(), font, theme.accent)],
                false,
                "Show all changes",
                Diffui::on_action(Action::Review(ReviewAction::Scope(Scope::All)), cx),
                look,
            );
            let place = format!(
                "{} {} of {}",
                names.map_or("Step", |names| names.step),
                index + 1,
                steps.len()
            );
            vec![
                typography::group(
                    vec![
                        back,
                        text("/".to_owned(), font, theme.subtle_text),
                        text(place, font, theme.text),
                    ],
                    space::SM,
                ),
                text(
                    step.id.chars().take(COMMIT_ID_CHARS).collect(),
                    mono,
                    theme.subtle_text,
                ),
            ]
        }
    }
}

/// What the tab's list source calls its whole and its steps.
fn steps_names(ui: &Diffui) -> Option<crate::review::StepsNames> {
    match ui.tabs.get(ui.active)?.steps() {
        crate::review::Steps::List(names) => Some(names),
        _ => None,
    }
}

/// The change id as jj prints it (its shortest unique prefix bright), then
/// the commit hash, dimmed; for git the hash alone, as the change id. With
/// the revisions hidden, they open them.
fn revision_ids(ui: &Diffui, details: &RevisionDetails, look: Look, cx: &mut Cx) -> Aligned {
    let Look { theme, .. } = look;
    let mono = look.mono;
    let row_of = ui
        .active()
        .session
        .commits
        .find_by_commit_id(&details.commit_id);
    let text = |value: String, color: Color| {
        Aligned::text(
            typography::line(value, mono, META_SIZE, color),
            mono,
            META_SIZE,
        )
    };
    let prefixed = |id: &str, unique: usize, suffix: Option<String>| {
        let shown: String = id
            .chars()
            .take(revision_id_display_len(unique, id))
            .collect();
        let bright: String = shown.chars().take(unique).collect();
        let mut rest: String = shown.chars().skip(unique).collect();
        if let Some(suffix) = suffix {
            rest.push_str(&suffix);
        }
        let mut parts = vec![text(bright, theme.change_id)];
        if !rest.is_empty() {
            parts.push(text(rest, theme.subtle_text));
        }
        typography::group(parts, 0.0)
    };
    let short_hash = || {
        details
            .commit_id
            .chars()
            .take(COMMIT_ID_CHARS)
            .collect::<String>()
    };
    let ids = match details.change_id.as_deref() {
        Some(change_id) => {
            let offset = row_of
                .and_then(|row| row.change_offset())
                .map(|offset| format!("/{offset}"));
            vec![
                prefixed(change_id, unique_prefix(ui, row_of), offset),
                text(short_hash(), theme.subtle_text),
            ]
        }
        // A PR's one id is its number.
        None if details.commit_id.starts_with('#') => {
            vec![text(details.commit_id.clone(), theme.muted_text)]
        }
        None => vec![prefixed(
            &details.commit_id,
            unique_prefix(ui, row_of),
            None,
        )],
    };
    let panel = crate::root::Panel::Revisions;
    panel_trigger(
        "commit-revisions-trigger",
        ids,
        hidden(ui, panel),
        "Show revisions",
        ui,
        look,
        cx,
    )
}

/// `+N −M`, the five-cell diffstat, and the file count. With the changed
/// files hidden, the count opens them.
fn diff_stats(ui: &Diffui, look: Look, cx: &mut Cx) -> Vec<Aligned> {
    let Look { theme, .. } = look;
    let document = &ui.active().session.document;
    let (additions, deletions) = (document.total_additions, document.total_deletions);
    let mono = look.mono;
    let count = |value: String, color: Color| {
        Aligned::text(
            typography::line(value, mono, META_SIZE, color),
            mono,
            META_SIZE,
        )
    };
    let counts = typography::group(
        [
            count(
                format!("+{}", compact_count(additions)),
                crate::diff_panel::stat_color(additions, theme.added_text, theme),
            ),
            count(
                format!("\u{2212}{}", compact_count(deletions)),
                crate::diff_panel::stat_color(deletions, theme.removed_text, theme),
            ),
        ],
        space::SM,
    );

    let (added, removed) = stat_cells(additions, deletions);
    let cell = |color: Color| {
        div()
            .size(px(STAT_CELL))
            .flex_none()
            .rounded(px(1.5))
            .bg(color)
    };
    let mut cells = div().flex().flex_row().gap(px(STAT_CELL_GAP));
    for index in 0..STAT_CELLS {
        cells = cells.child(cell(if index < added {
            theme.added_text
        } else if index < added + removed {
            theme.removed_text
        } else {
            theme.border
        }));
    }

    let files = document.files.len();
    let font = look.font;
    let label = format!("{files} {}", if files == 1 { "file" } else { "files" });
    let panel = crate::root::Panel::Files;
    vec![
        counts,
        Aligned::glyph(cells, STAT_CELL),
        panel_trigger(
            "commit-files-trigger",
            vec![Aligned::text(
                typography::line(label, font, META_SIZE, theme.muted_text),
                font,
                META_SIZE,
            )],
            hidden(ui, panel),
            "Show changed files",
            ui,
            look,
            cx,
        ),
    ]
}

/// ⋯: the revision's context menu, the one its row in the list opens. jj
/// only, as that menu is.
fn more_button(ui: &Diffui, look: Look, cx: &mut Cx) -> Option<AnyElement> {
    let Look { theme, .. } = look;
    let tab = ui.active();
    if tab.repository.as_ref().map(|r| r.vcs) != Some(crate::repository::Vcs::Jj) {
        return None;
    }
    let key = match &tab.session.selected_revision {
        diffui_core::RevisionSelection::WorkingCopy => {
            crate::revision_list::RowSelectionKey::WorkingCopy
        }
        diffui_core::RevisionSelection::Commit(id) => {
            crate::revision_list::RowSelectionKey::Commit(id.clone())
        }
    };
    let (anchor, tracker) = crate::menu::anchor_tracker();
    let open = Diffui::listen(cx, move |this, _: &gpui_kit::ClickEvent, _, cx| {
        let rect = anchor.get();
        let cursor = crate::geometry::Point::new(rect.x, rect.y + rect.height);
        this.open_revision_menu(key.clone(), rect, cursor, cx);
    });
    Some(
        div()
            .relative()
            .child(tracker)
            .child(
                look.icon_button("commit-more", icons::ELLIPSIS, theme.muted_text)
                    .on_click(open)
                    .tip("More revision actions"),
            )
            .into_any_element(),
    )
}

/// A title as header lines, wrapped at spaces to the pane so
/// a narrow pane shows all of it.
fn title_lines(ui: &Diffui, summary: Option<&str>, pane_width: f32) -> Vec<HeaderLine> {
    let font = emphasis_font(ui.config.ui_font, Weight::SEMIBOLD);
    let room =
        (pane_width - 2.0 * crate::diff_view::HEADER_HORIZONTAL_PADDING - crate::scrollbar::GUTTER)
            .max(120.0);
    let fits = |text: &str| {
        crate::measure::line_width(text, crate::diff_view::HEADER_TITLE_SIZE, font) <= room
    };
    let mut rows: Vec<String> = Vec::new();
    for word in summary.unwrap_or("No description").split(' ') {
        match rows.last_mut() {
            Some(row) if fits(&format!("{row} {word}")) => {
                row.push(' ');
                row.push_str(word);
            }
            _ => rows.push(word.to_owned()),
        }
    }
    rows.into_iter()
        .map(|text| HeaderLine::Title {
            text,
            placeholder: summary.is_none(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use diffui_core::{ParentBrief, RevisionDetails, SignatureInfo};

    use super::{compact_count, header_lines, stat_cells, wrap_columns};

    #[test]
    fn the_diffstat_splits_five_cells_and_counts_stay_short() {
        assert_eq!(stat_cells(0, 0), (0, 0));
        assert_eq!(stat_cells(5, 0), (5, 0));
        assert_eq!(stat_cells(0, 5), (0, 5));
        assert_eq!(stat_cells(1, 1), (3, 2));
        assert_eq!(stat_cells(30, 10), (4, 1));
        // One deletion against thousands of additions still gets a cell.
        assert_eq!(stat_cells(10_000, 1), (4, 1));
        assert_eq!(compact_count(9_999), "9999");
        assert_eq!(compact_count(12_345), "12k");
        assert_eq!(compact_count(3_400_000), "3M");
    }

    #[test]
    fn description_lines_wrap_at_spaces_and_keep_their_indent() {
        assert_eq!(wrap_columns("short line", 20), ["short line"]);
        assert_eq!(
            wrap_columns("one two three four five", 10),
            ["one two", "three four", "five"]
        );
        assert_eq!(
            wrap_columns("  - a list item that wraps", 12),
            ["  - a list", "  item that", "  wraps"]
        );
        assert_eq!(wrap_columns("abcdefghijkl", 5), ["abcde", "fghij", "kl"]);
    }
    use crate::diff_view::{HeaderAction, HeaderLine};

    fn render(lines: &[HeaderLine]) -> Vec<String> {
        lines
            .iter()
            .map(|line| match line {
                HeaderLine::Title { text, .. } => format!("# {text}"),
                HeaderLine::Spacer(_) => "~".to_owned(),
                HeaderLine::Description(value) => format!("| {value}"),
                HeaderLine::Blank => String::new(),
                HeaderLine::Rich(field) => format!("{}: {}", field.label, field.value),
                HeaderLine::Bookmarks { label, chips } => format!(
                    "{label}: {}",
                    chips
                        .iter()
                        .map(|chip| chip.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                HeaderLine::Inset => "[inset]".to_owned(),
                other => panic!("unexpected header line: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn the_header_is_the_title_the_description_then_one_column_of_fields() {
        let mut ui = crate::tabs::tests::app();
        crate::tabs::tests::push_tab(&mut ui, "/tmp/diffui-commit-info-test");
        let theme = ui.resolved_theme().spec();
        let look = ui.look(theme);
        let details = RevisionDetails {
            commit_id: "abcdef0123456789abcdef0123456789abcdef01".to_owned(),
            change_id: Some("zyxwvutsrqponmlkzyxwvutsrqponmlk".to_owned()),
            bookmarks: vec!["main".to_owned()],
            author: SignatureInfo {
                name: "Ada Lovelace".to_owned(),
                email: "ada@example.com".to_owned(),
                timestamp: Some("2026-09-07 10:00:00 +0000".to_owned()),
            },
            committer: None,
            signature: None,
            description: "subject\n\nbody one\nbody two\n".to_owned(),
            parents: vec![ParentBrief {
                commit_id: "1111111111111111111111111111111111111111".to_owned(),
                change_id: Some("kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk".to_owned()),
                summary: "the parent".to_owned(),
            }],
        };

        let header = super::revision_header(&ui, &details, look);
        let lines = header_lines(&ui, &header, look, true, 2000.0, true);
        let rendered = render(&lines);
        // The title, the facts, then the body: as a pull request's header.
        assert_eq!(
            rendered[..3],
            ["# subject", "~", "Author: Ada Lovelace <ada@example.com>"]
        );
        assert!(rendered[3].starts_with("Date: Sep 7, 2026, 10:00  "));
        assert_eq!(
            rendered[4..],
            [
                "Change: zyxwvutsrqponmlkzyxwvutsrqponmlk",
                "Commit: abcdef0123456789abcdef0123456789abcdef01",
                "Parent: kkkkkkkk  the parent",
                "Bookmarks: main",
                "",
                "| body one",
                "| body two",
            ]
        );
        // The copy buttons copy whole ids; the parent's id opens it.
        let HeaderLine::Rich(commit) = &lines[5] else {
            panic!("the commit line is a field");
        };
        assert_eq!(
            commit.buttons[0].1,
            HeaderAction::Copy(details.commit_id.clone())
        );
        let HeaderLine::Rich(parent) = &lines[6] else {
            panic!("the parent line is a field");
        };
        assert_eq!(
            parent.link,
            Some((8, HeaderAction::Open(details.parents[0].commit_id.clone())))
        );

        // Rendered, the body goes to the header's inset.
        ui.text_views.messages_rendered = true;
        let rendered_lines = render(&header_lines(&ui, &header, look, true, 2000.0, true));
        assert_eq!(rendered_lines[rendered_lines.len() - 2..], ["", "[inset]"]);
        // A header-only view has no inset: it shows the text as written.
        assert_eq!(
            render(&header_lines(&ui, &header, look, true, 2000.0, false)),
            rendered
        );
        ui.text_views.messages_rendered = false;

        // Folded, the diff pane's header is the title alone; the History
        // view's, which names the revision itself, keeps the rest.
        ui.commit_details_collapsed = true;
        assert_eq!(
            render(&header_lines(&ui, &header, look, true, 2000.0, true)),
            ["# subject"]
        );
        assert_eq!(
            header_lines(&ui, &header, look, false, 2000.0, false).len(),
            lines.len() - 2
        );
    }
}

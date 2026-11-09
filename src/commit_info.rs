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

use crate::diff_view::{HeaderAction, HeaderField, HeaderLine};
use crate::meta_line;
use crate::sidebar::{COMMIT_ID_CHARS, REVISION_ID_CHARS, revision_id_display_len};
use crate::theme::{self, Color, Font, ThemeSpec, Weight, emphasis_font, radius, space, text_size};
use crate::typography::Aligned;
use crate::ui::Tip as _;
use crate::{Action, Cx, Diffui, icons, typography, ui};

/// The header bar's height: a pane header's.
pub(crate) const BAR_HEIGHT: f32 = 40.0;
/// The title's step: the revision list's description size.
const TITLE_SIZE: f32 = text_size::BODY;
/// Between the header's title and what follows it.
const TITLE_GAP: f32 = 8.0;
/// Ids and counts, a step under the title.
const META_SIZE: f32 = text_size::UI;
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

/// The diff view's header for `details`: the description's body, then the
/// field column. Empty when the block is folded, if `foldable` (the diff
/// pane; the History view's headers already say what the revision is, so
/// its diffs always carry the details).
///
/// `pane_width` is the width of the diff view it heads: with line wrap on,
/// the description wraps to it, since a header line is one row.
pub(crate) fn header_lines(
    ui: &Diffui,
    details: &RevisionDetails,
    theme: ThemeSpec,
    foldable: bool,
    pane_width: f32,
) -> Vec<HeaderLine> {
    // The diff pane's bar names the revision by its ids only; the title
    // leads the header under it. The History view's own headers carry it.
    let mut lines = if foldable {
        title_lines(ui, details, pane_width)
    } else {
        Vec::new()
    };
    if foldable && ui.commit_details_collapsed {
        return lines;
    }
    let text = details
        .description
        .trim()
        .split_once('\n')
        .map_or("", |(_, body)| body)
        .trim();
    let columns = ui.diff_wrap.then(|| {
        let size = ui.config.code_type.size;
        let char_width = crate::measure::line_width("0000000000", size, ui.config.mono_font) / 10.0;
        let text_width = pane_width
            - 2.0 * crate::diff_view::HEADER_HORIZONTAL_PADDING
            - crate::scrollbar::GUTTER;
        (text_width / char_width.max(1.0)).floor().max(20.0) as usize
    });
    let body: Vec<HeaderLine> = text
        .lines()
        .flat_map(|line| match columns {
            Some(columns) => wrap_columns(line, columns),
            None => vec![line.to_owned()],
        })
        .map(|line| HeaderLine::description(&line))
        .collect();
    if !lines.is_empty() {
        lines.push(HeaderLine::spacer(TITLE_GAP));
    }
    if !body.is_empty() {
        lines.extend(body);
        lines.push(HeaderLine::blank());
    }

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
            change.chips = crate::sidebar::status_chips_for(row, theme, ui.config);
        }
        change.buttons = vec![(icons::COPY, HeaderAction::Copy(change_id.to_owned()))];
        lines.push(rich(change));
    }
    let pull_request = details.commit_id.starts_with('#');
    let mut commit = HeaderField::new(
        if pull_request {
            "Pull request"
        } else {
            "Commit"
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
        Some(row) => crate::sidebar::graph_ref_chips(session, row.index(), theme, ui.config),
        None => {
            crate::sidebar::bookmark_chips_for(&details.bookmarks, theme.accent, theme, ui.config)
        }
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
    lines
}

/// What the diff view's header links and buttons do.
pub(crate) fn header_action(action: HeaderAction) -> Action {
    match action {
        HeaderAction::Copy(value) => Action::Copy(value),
        HeaderAction::Open(commit_id) => {
            Action::History(crate::history_panel::HistoryAction::OpenRevision(commit_id))
        }
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

/// `child` with the app's hover label, for a control in a banner or a title row.
pub(crate) fn with_tooltip(
    id: &'static str,
    label: impl Into<gpui_kit::SharedString>,
    font: Font,
    theme: ThemeSpec,
    child: impl IntoElement,
) -> AnyElement {
    div()
        .id(id)
        .flex_none()
        .tip(label, font, theme)
        .child(child)
        .into_any_element()
}

/// The header bar over the diff: which revision it is (its ids), how big the
/// change is, and what can be done to it. With a panel hidden, the
/// ids and the file count open it over the diff (see [`crate::root`]).
pub(crate) fn view(ui: &Diffui, theme: ThemeSpec, _window: &mut Window, cx: &mut Cx) -> AnyElement {
    let session = &ui.active().session;
    let font = ui.config.ui_font;
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
            line.extend(diff_stats(ui, theme, cx));
        }
        return block(theme).child(bar(line, Vec::new())).into_any_element();
    }
    let Some(details) = session.revision_details.as_ref() else {
        let line = if has_files {
            diff_stats(ui, theme, cx)
        } else {
            Vec::new()
        };
        return block(theme).child(bar(line, Vec::new())).into_any_element();
    };

    let mut line = vec![revision_ids(ui, details, theme, cx)];
    if has_files {
        line.extend(diff_stats(ui, theme, cx));
    }

    let mut actions = Vec::new();
    if session.capabilities.mutate {
        actions.push(with_tooltip(
            "commit-edit-tip",
            crate::toolbar::with_shortcut(ui, "Edit description", "revision.edit-description"),
            font,
            theme,
            ui::text_button(
                "commit-edit",
                Some((icons::PENCIL, theme.muted_text)),
                "Describe",
                ui::Variant::Raised,
                ui::Size::Compact,
                font,
                theme,
                cx,
            )
            .on_click(Diffui::on_action(
                Action::EditDescription { target: None },
                cx,
            )),
        ));
    }
    let folded = ui.commit_details_collapsed;
    actions.push(with_tooltip(
        "commit-fold-tip",
        if folded {
            "Show the description and details"
        } else {
            "Show just the title"
        },
        font,
        theme,
        ui::icon_button(
            "commit-fold",
            if folded {
                icons::UNFOLD_VERTICAL
            } else {
                icons::FOLD_VERTICAL
            },
            theme.muted_text,
            ui::Size::Compact,
            theme,
            cx,
        )
        .on_click(Diffui::on_action(Action::ToggleCommitDetails, cx)),
    ));
    if let Some(more) = more_button(ui, theme, cx) {
        actions.push(more);
    }

    block(theme).child(bar(line, actions)).into_any_element()
}

/// Where the bar's first text starts: the diff header's own inset, so the
/// title sits over the description under it.
const BAR_LEAD: f32 = crate::diff_view::HEADER_HORIZONTAL_PADDING;

fn block(theme: ThemeSpec) -> gpui_kit::Div {
    div()
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(theme.panel_background)
        .border_b_1()
        .border_color(theme.border)
}

/// One row: `line` on one baseline, cut where it runs out of room, then the
/// buttons.
fn bar(line: Vec<Aligned>, actions: Vec<AnyElement>) -> gpui_kit::Div {
    div()
        .w_full()
        .h(px(BAR_HEIGHT))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::XXS))
        .pl(px(BAR_LEAD))
        .pr(px(space::SM))
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
    theme: ThemeSpec,
    cx: &mut Cx,
) -> Aligned {
    let group = typography::group(content, space::MD);
    let Some(panel) = panel else {
        return group;
    };
    let open = ui.panel_peek.is_some_and(|peek| peek.panel == panel);
    let font = ui.config.ui_font;
    let mut content = vec![group];
    content.push(Aligned::glyph(
        icons::icon(icons::CHEVRON_DOWN, 11.0, theme.muted_text),
        META_SIZE,
    ));
    let (anchor, tracker) = crate::menu::anchor_tracker();
    let on_click = Diffui::listen(cx, move |this, _: &gpui_kit::ClickEvent, _, _| {
        this.toggle_panel_peek(panel, anchor.get());
    });
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
            .tip(tooltip, font, theme)
            .on_click(on_click)
            .child(tracker)
            .child(line)
            .into_any_element()
    })
}

/// Which panel a trigger opens, if that panel is hidden.
fn hidden(ui: &Diffui, panel: crate::root::Panel) -> Option<crate::root::Panel> {
    let tab = ui.active();
    let hidden = tab.main_view == crate::MainView::Diff
        && match panel {
            crate::root::Panel::Revisions => tab.history_panel_collapsed,
            crate::root::Panel::Files => tab.files_panel_collapsed,
        };
    hidden.then_some(panel)
}

/// The change id as jj prints it (its shortest unique prefix bright), then
/// the commit hash, dimmed; for git the hash alone, as the change id. With
/// the revisions hidden, they open them.
fn revision_ids(ui: &Diffui, details: &RevisionDetails, theme: ThemeSpec, cx: &mut Cx) -> Aligned {
    let mono = ui.config.mono_font;
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
        theme,
        cx,
    )
}

/// `+N −M`, the five-cell diffstat, and the file count. With the changed
/// files hidden, the count opens them.
fn diff_stats(ui: &Diffui, theme: ThemeSpec, cx: &mut Cx) -> Vec<Aligned> {
    let document = &ui.active().session.document;
    let (additions, deletions) = (document.total_additions, document.total_deletions);
    let mono = ui.config.mono_font;
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
    let font = ui.config.ui_font;
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
            theme,
            cx,
        ),
    ]
}

/// ⋯: the revision's context menu, the one its row in the list opens. jj
/// only, as that menu is.
fn more_button(ui: &Diffui, theme: ThemeSpec, cx: &mut Cx) -> Option<AnyElement> {
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
        this.on_revision_context_menu(key.clone(), rect, cursor, cx);
    });
    Some(with_tooltip(
        "commit-more-tip",
        "More revision actions",
        ui.config.ui_font,
        theme,
        div().relative().child(tracker).child(
            ui::icon_button(
                "commit-more",
                icons::ELLIPSIS,
                theme.muted_text,
                ui::Size::Compact,
                theme,
                cx,
            )
            .on_click(open),
        ),
    ))
}

/// The revision's title as header lines, wrapped at spaces to the pane so
/// a narrow pane shows all of it.
fn title_lines(ui: &Diffui, details: &RevisionDetails, pane_width: f32) -> Vec<HeaderLine> {
    let summary = details
        .description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty());
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
                other => panic!("unexpected header line: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn the_header_is_the_title_the_description_then_one_column_of_fields() {
        let mut ui = crate::tabs::tests::app();
        crate::tabs::tests::push_tab(&mut ui, "/tmp/diffui-commit-info-test");
        let theme = ui.resolved_theme().spec();
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

        let lines = header_lines(&ui, &details, theme, true, 2000.0);
        let rendered = render(&lines);
        assert_eq!(
            rendered[..6],
            [
                "# subject",
                "~",
                "| body one",
                "| body two",
                "",
                "Author: Ada Lovelace <ada@example.com>"
            ]
        );
        assert!(rendered[6].starts_with("Date: Sep 7, 2026, 10:00  "));
        assert_eq!(
            rendered[7..],
            [
                "Change: zyxwvutsrqponmlkzyxwvutsrqponmlk",
                "Commit: abcdef0123456789abcdef0123456789abcdef01",
                "Parent: kkkkkkkk  the parent",
                "Bookmarks: main",
            ]
        );
        // The copy buttons copy whole ids; the parent's id opens it.
        let HeaderLine::Rich(commit) = &lines[8] else {
            panic!("the commit line is a field");
        };
        assert_eq!(
            commit.buttons[0].1,
            HeaderAction::Copy(details.commit_id.clone())
        );
        let HeaderLine::Rich(parent) = &lines[9] else {
            panic!("the parent line is a field");
        };
        assert_eq!(
            parent.link,
            Some((8, HeaderAction::Open(details.parents[0].commit_id.clone())))
        );

        // Folded, the diff pane's header is the title alone; the History
        // view's, which names the revision itself, keeps the rest.
        ui.commit_details_collapsed = true;
        assert_eq!(
            render(&header_lines(&ui, &details, theme, true, 2000.0)),
            ["# subject"]
        );
        assert_eq!(
            header_lines(&ui, &details, theme, false, 2000.0).len(),
            lines.len() - 2
        );
    }
}

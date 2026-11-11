use crate::chip::{self, Chip};
use crate::config::AppConfig;
use crate::field;
use crate::graph_view::RevisionGraphStyle;
use crate::measure;
use crate::repository::Vcs;
use crate::revision_list::{self, DropSpot, RevisionListStyle};
use crate::theme::{
    self, Color, Padding, ThemeSpec, Weight, chip_background, emphasis_font, text_size,
};
use crate::typography::{self, Aligned};
use crate::{Action, Cx, Diffui, HoverTarget, LoadStatus, ToolbarMenu, icons, ui};
use diffui_core::{DiffFile, FileTreeRow, RowView, file_tree_rows};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};
use std::collections::HashSet;
use std::rc::Rc;

// Public sidebar layout knobs — used by main.rs to clamp the resize handle.
// `DEFAULT_WIDTH` is a starting point; the actual floor is derived from the
// configured UI font at runtime by `min_width(...)` so it scales when the
// user picks a larger font in the future. There's no upper cap — users can
// drag the sidebar as wide as the window allows.
pub const DEFAULT_WIDTH: f32 = 340.0;
pub const RESIZE_HIT_PADDING: f32 = 2.0;

/// Minimum sidebar width for the current font config. Picked so a revision's
/// id line can always fit its load-bearing columns at this font beside a
/// one-lane gutter: the change id (8 chars), commit id (8 chars), an
/// abbreviated author, the `+N` bookmark-overflow chip and the loudest
/// status chip (`divergent`); on the working copy's row, the "Working copy"
/// label, the change id and that status chip. Below this width the chip rail
/// starts dropping rails, so the user loses the conflict/divergence signal.
pub fn min_width(config: AppConfig) -> f32 {
    let id_width =
        |content: &str| measure::line_width(content, CAPTION_TEXT_SIZE, config.mono_font);

    let change_id_w = id_width(&"a".repeat(REVISION_ID_CHARS));
    let commit_id_w = id_width(&"a".repeat(COMMIT_ID_CHARS));
    let author_w = measure::line_width("Author Name", CAPTION_TEXT_SIZE, config.ui_font);
    let plus_n_w = chip::width("+9", None, config.ui_font);
    let conflict_w = chip::width("divergent", None, config.mono_font);
    let label_w = revision_list::working_copy_label_width(config.ui_font);

    let id_gap = 8.0;
    let chip_gap = 6.0;
    let row = change_id_w
        + id_gap
        + commit_id_w
        + id_gap
        + author_w
        + chip_gap
        + plus_n_w
        + chip_gap
        + conflict_w;
    let working_copy_row = label_w + id_gap + change_id_w + chip_gap + conflict_w;
    revision_list::revision_gutter_width(1) + row.max(working_copy_row) + theme::list::PAD_END
}

const CAPTION_TEXT_SIZE: f32 = theme::text_size::CAPTION;
pub(crate) const REVISION_ID_CHARS: usize = 8;
pub(crate) const COMMIT_ID_CHARS: usize = 8;

/// The sidebar's whole-pane wash while target mode is on: a light accent
/// tint over the panel, so "a draft is in progress" reads from anywhere in
/// the pane — not just the op bar. Also fed to the revision list's own
/// background fill (the widget paints over the container).
pub(crate) fn draft_panel_background(theme: ThemeSpec) -> Color {
    theme::mix(theme.panel_background, theme.accent, 0.05)
}

fn commit_list_summary(commits: &[String]) -> String {
    match commits {
        [] => String::new(),
        [commit] => commit.clone(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
    }
}

/// The op bar's verdict for a simulation that only forecasts conflicts:
/// "No new conflicts" or which revisions would conflict, over `counts`.
fn conflict_verdict(
    new_conflicts: &[String],
    counts: Option<String>,
    theme: ThemeSpec,
) -> (Option<&'static str>, String, Color, Option<String>) {
    if new_conflicts.is_empty() {
        (
            Some(icons::CHECK),
            "No new conflicts".to_owned(),
            theme.added_text,
            counts,
        )
    } else {
        (
            Some(icons::ALERT_TRIANGLE),
            format!("Conflicts in {}", commit_list_summary(new_conflicts)),
            theme.conflict_marker,
            counts,
        )
    }
}

/// Focus target id for the revset input.
pub const REVSET_INPUT_ID: &str = "revset-input";

/// The graph's lane colours for `theme`.
pub(crate) fn revision_graph_style(theme: ThemeSpec) -> RevisionGraphStyle {
    RevisionGraphStyle {
        lanes: theme.lanes,
        missing_color: theme.subtle_text,
        neutral: theme.subtle_text,
        surface: theme.panel_background,
        accent: theme.accent,
        accent_foreground: theme.panel_background,
        conflict: theme.conflict_marker,
    }
}

/// The bookmark and tag chips the graph draws on row `index`, for views
/// that show a revision's refs the way the graph does.
pub(crate) fn graph_ref_chips(
    session: &diffui_core::Session,
    index: usize,
    theme: ThemeSpec,
    config: AppConfig,
) -> Vec<Chip> {
    let commit = session.commits.row(index);
    let lane = session.graph.frame(index, usize::MAX).node_lane;
    let mut chips = bookmark_chips_for(
        commit.bookmarks(),
        revision_graph_style(theme).lane_color(lane),
        theme,
        config,
    );
    if let Some(tags) = session.bookmarks.tags.get(commit.commit_id()) {
        chips.extend(tag_chips_for(tags, theme, config));
    }
    chips
}

/// Bookmark chips for a revision row. Shared with the source browser's
/// revision-header row and the commit header, so every rail styles them alike.
///
/// One neutral chip for every ref, so the rail stays quiet next to the status
/// chips; the bookmark glyph wears `lane_color`, tying the name to its branch
/// in the graph. Core authors the label shapes and jj forbids `@` inside
/// bookmark and remote names, so a trailing `@` can only be another
/// workspace's working copy (`name@`, a folder glyph) and an interior `@` a
/// remote bookmark (`main@origin`): outlined, with the remote in subtle text.
/// A local bookmark that differs from its remote keeps its `*` in the modified
/// colour, and a conflicted one its `??` in the conflict colour.
pub(crate) fn bookmark_chips_for(
    bookmarks: &[String],
    lane_color: Color,
    theme: ThemeSpec,
    config: AppConfig,
) -> Vec<Chip> {
    bookmarks
        .iter()
        .map(|bookmark| {
            let is_workspace = bookmark.ends_with('@');
            let remote_at = (!is_workspace).then(|| bookmark.find('@')).flatten();
            let (label, suffix) = if let Some(at) = remote_at {
                (
                    bookmark[..at].to_owned(),
                    Some((bookmark[at..].to_owned(), theme.subtle_text)),
                )
            } else if let Some(name) = bookmark.strip_suffix("??") {
                (
                    name.to_owned(),
                    Some(("??".to_owned(), theme.conflict_marker)),
                )
            } else if let Some(name) = bookmark.strip_suffix('*') {
                (
                    name.to_owned(),
                    Some(("*".to_owned(), theme.modified_token)),
                )
            } else {
                (bookmark.clone(), None)
            };
            Chip {
                label,
                font: config.ui_font,
                background: if remote_at.is_some() {
                    crate::theme::TRANSPARENT
                } else {
                    neutral_chip_background(theme)
                },
                text_color: theme.muted_text,
                border_color: remote_at.is_some().then_some(theme.border),
                border_dashed: false,
                icon: Some(if is_workspace {
                    icons::FOLDER
                } else {
                    icons::BOOKMARK
                }),
                icon_color: Some(lane_color),
                suffix,
            }
        })
        .collect()
}

/// The fill every neutral chip shares: bookmarks, tags and the `+N` overflow.
pub(crate) fn neutral_chip_background(theme: ThemeSpec) -> Color {
    chip_background(theme.muted_text)
}

/// Read-only tag chips for a revision row: neutral rather than lane-colored,
/// and wearing the tag glyph, so they never read as bookmarks. A remote tag
/// that differs from the local one (`v1@origin`) is outlined, like a remote
/// bookmark.
pub(crate) fn tag_chips_for(tags: &[String], theme: ThemeSpec, config: AppConfig) -> Vec<Chip> {
    tags.iter()
        .map(|tag| Chip {
            label: tag.clone(),
            font: config.ui_font,
            background: neutral_chip_background(theme),
            text_color: theme.muted_text,
            border_color: tag.contains('@').then_some(theme.border),
            border_dashed: false,
            icon: Some(icons::TAG),
            icon_color: None,
            suffix: None,
        })
        .collect()
}

/// Status chips (`empty`, `conflict`, `hidden`/`divergent`) for a commit row.
/// Shared with the source browser's revision-header row.
pub(crate) fn status_chips_for(commit: RowView, theme: ThemeSpec, config: AppConfig) -> Vec<Chip> {
    let mut status_chips = Vec::new();
    if commit.is_empty() == Some(true) {
        status_chips.push(Chip {
            label: "empty".to_owned(),
            font: config.mono_font,
            background: crate::theme::TRANSPARENT,
            text_color: theme.subtle_text,
            border_color: Some(theme.subtle_text),
            border_dashed: true,
            icon: None,
            icon_color: None,
            suffix: None,
        });
    }
    if commit.has_conflict() {
        status_chips.push(Chip {
            label: "conflict".to_owned(),
            font: config.mono_font,
            background: chip_background(theme.conflict_marker),
            text_color: theme.conflict_marker,
            border_color: None,
            border_dashed: false,
            icon: None,
            icon_color: None,
            suffix: None,
        });
    }
    if commit.is_hidden() {
        // jj log's "(hidden)": rewritten/abandoned, still shown because a ref
        // (e.g. a stale remote bookmark) pins it into the revset. Takes
        // precedence over the divergent chip, matching jj's log template.
        status_chips.push(Chip {
            label: "hidden".to_owned(),
            font: config.mono_font,
            background: crate::theme::TRANSPARENT,
            text_color: theme.subtle_text,
            border_color: Some(theme.subtle_text),
            border_dashed: false,
            icon: None,
            icon_color: None,
            suffix: None,
        });
    } else if commit.is_divergent() {
        // jj log flags these with a change-offset suffix: the change id maps
        // to several visible commits. Amber (not conflict-red) — it's a
        // warning about identity, not about tree state.
        status_chips.push(Chip {
            label: "divergent".to_owned(),
            font: config.mono_font,
            background: chip_background(theme.modified_token),
            text_color: theme.modified_token,
            border_color: None,
            border_dashed: false,
            icon: None,
            icon_color: None,
            suffix: None,
        });
    }
    status_chips
}

/// View-time memo for the changed-files tree. The tree is rebuilt only when the
/// document or directory-collapse set changes, rather than on every diff scroll.
#[derive(Debug, Clone, Default)]
pub(crate) struct SidebarFileCache {
    tree: Option<(TreeKey, Rc<Vec<FileTreeRow>>)>,
}

/// The tree folds in the collapse set (it prunes collapsed dirs) but, unlike the
/// widths, is independent of font and theme — so it gets its own key.
#[derive(Debug, Clone, PartialEq)]
struct TreeKey {
    document_id: u64,
    file_count: usize,
    collapsed: HashSet<String>,
}

impl SidebarFileCache {
    pub(crate) fn tree_rows(
        &mut self,
        document_id: u64,
        file_count: usize,
        collapsed: &HashSet<String>,
        files: &[DiffFile],
    ) -> Rc<Vec<FileTreeRow>> {
        if let Some((key, rows)) = &self.tree
            && key.document_id == document_id
            && key.file_count == file_count
            && &key.collapsed == collapsed
        {
            return Rc::clone(rows);
        }
        let rows = Rc::new(file_tree_rows(files, collapsed));
        self.tree = Some((
            TreeKey {
                document_id,
                file_count,
                collapsed: collapsed.clone(),
            },
            Rc::clone(&rows),
        ));
        rows
    }
}

pub(crate) fn revision_list_style(theme: ThemeSpec, config: AppConfig) -> RevisionListStyle {
    RevisionListStyle {
        graph: revision_graph_style(theme),
        background: theme.panel_background,
        selected_background: theme.selected_file,
        accent: theme.accent,
        control: theme.info,
        border: theme.border,
        muted_text: theme.muted_text,
        subtle_text: theme.subtle_text,
        accent_text: theme.accent,
        change_id: theme.change_id,
        accent_foreground: theme.panel_background,
        primary_font: config.ui_font,
        mono_font: config.mono_font,
        floating: theme,
    }
}

pub(crate) fn revision_id_display_len(unique_len: usize, revision_id: &str) -> usize {
    REVISION_ID_CHARS
        .max(unique_len)
        .min(revision_id.chars().count())
}

pub(crate) fn truncate_end(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub(crate) fn commit_description_color(commit: RowView, theme: ThemeSpec) -> Color {
    if commit.has_description() {
        return theme.text;
    }

    match commit.is_empty() {
        Some(true) => theme.added_text,
        Some(false) => theme.note_text,
        None => theme.note_text,
    }
}

pub(crate) fn build_sidebar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let look = ui.look(theme);
    // The "Changes" header is gone; the list starts flush at the top of the
    // pane. Each row carries its own internal vertical padding, so the first
    // row's content isn't cramped against the top edge — adding a panel-colored
    // gap above it instead just reads as a stray strip above the selected row.
    let revision_list = crate::revision_rows::build_revision_list(ui, theme, window, cx);

    let draft_active = ui.active().op_draft().is_some();
    let mut body = div()
        .flex()
        .flex_col()
        .size_full()
        .bg(if draft_active {
            draft_panel_background(theme)
        } else {
            theme.panel_background
        })
        // The revset / revision-range filter sits at the very top of the pane.
        .child(
            ui::panel_header()
                .pl(px(field::list_field_insets().0))
                .pr(px(field::list_field_insets().1))
                .gap(px(field::PANEL_TOGGLE_INSET))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(field::FIELD_HEIGHT))
                        .child(build_revset_filter(ui, theme, window, cx)),
                ),
        )
        .children(crate::revset_assist::error_strip(ui, theme, window, cx))
        .children(crate::history_panel::file_filter_banner(
            ui, theme, window, cx,
        ));
    // Load failures no longer have a header to live under — surface them as a
    // soft alert card above the list rather than bare red text.
    // A revset error is told once: by the field, under it, when its own
    // check has it; the card only for what that check couldn't foresee.
    if let LoadStatus::Failed(error) = &ui.active().session.status
        && !(crate::empty_states::is_revset_error(error)
            && crate::revset_assist::explains_revset_error(ui))
    {
        body = body.child(crate::empty_states::load_error(ui, look, error, cx));
    }
    let list =
        crate::empty_states::revision_list_placeholder(ui, look, cx).unwrap_or(revision_list);
    // The revset field's completions hang over the top of the list, which
    // recedes under a veil of the pane's colour while they're open. The veil
    // takes no presses: the list stays live.
    let dropdown = crate::revset_assist::dropdown(ui, theme, window, cx);
    let veil = dropdown.is_some().then(|| {
        div().absolute().inset_0().bg(Color {
            a: DROPDOWN_VEIL,
            ..theme.panel_background
        })
    });
    body = body.child(
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(list)
            .children(veil)
            .children(
                dropdown
                    .map(|dropdown| div().absolute().top_0().left_0().right_0().child(dropdown)),
            ),
    );
    // Target mode: the op bar floats under the list while a rebase/squash
    // draft is picking its destination.
    if draft_active {
        body = body.child(build_op_bar(ui, theme, window, cx));
    }
    body.into_any_element()
}

/// How much of the list the veil under the revset completions hides.
const DROPDOWN_VEIL: f32 = 0.65;

/// How a target row of the op bar reads.
#[derive(Clone, Copy, PartialEq)]
enum RowTone {
    /// What would actually happen.
    Armed,
    /// A chosen placement still waiting on a target.
    Chosen,
    Idle,
}

/// One way the operation can land: its label, keycap and what it means for the
/// armed target.
///
/// Armed is a light accent tint with accent ink, not a solid fill — a
/// full-width solid bar next to the washed sidebar was glaring. It still
/// outranks Chosen (accent ink, no row fill) by owning the only tinted row
/// surface.
#[allow(clippy::too_many_arguments)]
fn op_target_row(
    id: &'static str,
    label: &'static str,
    key: Option<String>,
    tone: RowTone,
    description: Option<String>,
    on_press: Option<Action>,
    ui: &Diffui,
    theme: ThemeSpec,
    cx: &mut Cx,
) -> AnyElement {
    let (ui_font, mono_font) = (ui.config.ui_font, ui.config.mono_font);
    let (label_color, key_color, key_bg, desc_color) = match tone {
        RowTone::Armed => (
            theme.accent,
            theme.accent,
            chip_background(theme.accent),
            theme.accent,
        ),
        RowTone::Chosen => (
            theme.accent,
            theme.accent,
            chip_background(theme.accent),
            theme.muted_text,
        ),
        RowTone::Idle => (
            theme.muted_text,
            theme.subtle_text,
            chip_background(theme.subtle_text),
            theme.subtle_text,
        ),
    };
    // The label, the keycap and the description share a baseline (see
    // `crate::typography`).
    let label_font = emphasis_font(ui_font, Weight::SEMIBOLD);
    let mut content = vec![
        Aligned::text(
            typography::line(label, label_font, text_size::BODY, label_color),
            label_font,
            text_size::BODY,
        )
        .map(|label| div().w(px(52.0)).child(label).into_any_element()),
    ];
    if let Some(key) = key {
        content.push(
            crate::ui::tag(key, key_color, crate::ui::TagSize::Regular, ui_font)
                .mono(mono_font)
                .fill(key_bg)
                .aligned(),
        );
    }
    if let Some(description) = description {
        // A long description ellipsises rather than running off the strip.
        content.push(
            Aligned::text(
                typography::line(description, ui_font, text_size::BODY, desc_color).text_ellipsis(),
                ui_font,
                text_size::BODY,
            )
            .shrink(),
        );
    }
    let shell = ui::ListRow::fit(theme::list::PAD_X, theme::radius::CONTROL);
    let mut row = if tone == RowTone::Armed {
        theme::squircle(
            div().w_full().flex().items_center().px(px(shell.pad_x)),
            shell.radius,
            Some(chip_background(theme.accent)),
            None,
        )
    } else {
        ui::list_row(shell, false, theme)
    }
    .id(id)
    .when(on_press.is_none(), |row| row.cursor_default())
    .overflow_hidden()
    .py(px(6.0))
    .test_support()
    .child(typography::baseline_row(content, 8.0).flex_1().min_w_0());
    if let Some(action) = on_press {
        row = row.on_click(Diffui::on_action(action, cx));
    }
    row.into_any_element()
}

/// The target-mode strip: what's being moved, the placement toggle
/// (rebase only), the live preview line, and the key hints.
fn build_op_bar(ui: &Diffui, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> AnyElement {
    let look = ui.look(theme);
    use diffui_core::{DraftKind, PlacementKind};
    let Some(draft_ui) = ui.active().op_draft() else {
        return div().into_any_element();
    };
    let draft = &draft_ui.draft;
    // Row-index helpers against the loaded graph.
    let commits = &ui.active().session.commits;
    let len = commits.len();
    let short_id = |index: usize| -> Option<String> {
        (index < len).then(|| {
            commits
                .row(index)
                .change_id()
                .chars()
                .take(8)
                .collect::<String>()
        })
    };
    let is_source_index =
        |index: usize| -> bool { index < len && draft.is_source(commits.row(index).commit_id()) };

    // The armed target: the row under a live drag, else the candidate
    // (hover / click / j-k all arm it). A gap spot has no single row.
    let hover_row = match draft_ui.hover_spot {
        Some(DropSpot::OnRow(index)) => Some(index),
        _ => None,
    };
    let gap_sides = match draft_ui.hover_spot {
        Some(DropSpot::Gap { above, below }) => Some((below, above)),
        _ => None,
    };
    let armed = hover_row.or(draft.candidate).filter(|&index| index < len);
    let armed_invalid = armed.is_some_and(is_source_index);
    let gap_invalid =
        gap_sides.is_some_and(|(parent, child)| is_source_index(parent) || is_source_index(child));
    // The valid target row a confirm would land on.
    let target = armed.filter(|&index| !is_source_index(index));
    let merge_ready = matches!(draft.kind, DraftKind::Merge) && draft.sources.len() >= 2;
    let parallel_ready = matches!(draft.kind, DraftKind::Parallelize) && draft.sources.len() >= 2;
    // Squash/merge gap drops resolve to the gap's parent side.
    let fold_target = target.or_else(|| {
        gap_sides
            .map(|(parent, _)| parent)
            .filter(|&parent| !is_source_index(parent))
    });

    // ---- Header: op glyph, what moves, live enrichment, state, cancel.
    let sim_rebase = match &draft_ui.preview {
        crate::DraftPreviewState::Ready(diffui_core::DraftSimulation::Rebase(preview)) => {
            Some(preview)
        }
        _ => None,
    };
    let (glyph, verb) = match draft.kind {
        DraftKind::Rebase {
            mode: diffui_core::RebaseSourceMode::Branch,
        } => (icons::GIT_MERGE, "Rebase branch of"),
        DraftKind::Rebase { .. } => (icons::GIT_MERGE, "Rebase"),
        DraftKind::Squash => (icons::FOLD_VERTICAL, "Squash"),
        DraftKind::Merge => (icons::GIT_FORK, "Merge"),
        DraftKind::New => (icons::SQUARE_PLUS, "New change"),
        DraftKind::Duplicate => (icons::COPY, "Duplicate"),
        DraftKind::Parallelize => (icons::COLUMNS_3, "Parallelize"),
    };
    let mut names = draft
        .sources
        .iter()
        .map(|source| source.label.clone())
        .collect::<Vec<_>>();
    if matches!(draft.kind, DraftKind::Merge)
        && !merge_ready
        && let Some(target_name) = target.and_then(short_id)
    {
        names.push(target_name);
    }
    let names = names.join(", ");
    // "+ N descendants": exact once the simulation lands; with-descendants
    // mode promises them even before it does.
    let descendants_suffix = match (draft.kind, sim_rebase) {
        (DraftKind::Rebase { .. } | DraftKind::Parallelize, Some(preview))
            if preview.descendants > 0 =>
        {
            Some(format!(
                "+ {} descendant{}",
                preview.descendants,
                if preview.descendants == 1 { "" } else { "s" }
            ))
        }
        (
            DraftKind::Rebase {
                mode: diffui_core::RebaseSourceMode::WithDescendants,
            },
            _,
        ) => Some("+ descendants".to_owned()),
        _ => None,
    };
    // The idle instruction lives in the verdict band below; the headline
    // only speaks up to refuse a target.
    let state_hint: Option<(&'static str, Color)> = ((armed_invalid || gap_invalid)
        && !matches!(draft.kind, DraftKind::Parallelize))
    .then_some(("Can't target the revision being moved", theme.removed_text));
    // The verb, the mono names and the suffix share a baseline; the glyph
    // and the close button centre on the caps (see `crate::typography`).
    let (ui_font, mono_font) = (ui.config.ui_font, ui.config.mono_font);
    let verb_font = emphasis_font(ui_font, Weight::SEMIBOLD);
    let names_font = emphasis_font(mono_font, Weight::MEDIUM);
    let mut operation = vec![
        Aligned::glyph(icons::icon(glyph, 15.0, theme.accent), 15.0),
        Aligned::gap(7.0),
        Aligned::text(
            typography::line(verb, verb_font, text_size::BODY, theme.text),
            verb_font,
            text_size::BODY,
        ),
        Aligned::gap(7.0),
        Aligned::text(
            typography::line(names.clone(), names_font, text_size::BODY, theme.text),
            names_font,
            text_size::BODY,
        ),
    ];
    if let Some(suffix) = descendants_suffix {
        operation.push(Aligned::gap(7.0));
        operation.push(Aligned::text(
            typography::line(suffix, ui_font, text_size::BODY, theme.muted_text),
            ui_font,
            text_size::BODY,
        ));
    }
    let operation = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::SM))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(typography::baseline_row(operation, 0.0)),
        )
        .child(
            look.icon_button("op-bar-cancel", icons::CLOSE, theme.muted_text)
                .on_click(Diffui::on_action(Action::DraftCancel, cx)),
        );
    let headline = div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(2.0))
        .child(operation)
        .children(state_hint.map(|(hint, color)| {
            div()
                .pl(px(22.0))
                .child(typography::line(hint, ui_font, text_size::BODY, color))
        }));

    // ---- Target rows: every way the op can land, each resolving what it
    // means for the armed target ("After → between llpz and kymz"). The row
    // that would actually happen wears the solid accent fill; a chosen
    // placement still waiting on a target wears a quiet tint. Keycaps ride
    // inside the rows, so the hints line below stays short.
    let mut rows_column = div().flex().flex_col().w_full().gap(px(2.0));
    match draft.kind {
        DraftKind::Rebase { .. } | DraftKind::New | DraftKind::Duplicate => {
            // While a drag is live the fill mirrors what a drop would do —
            // a row means onto, a gap means between — and reverts to the
            // sticky keyboard placement when the drag ends.
            let live_placement: Option<PlacementKind> = if gap_sides.is_some() {
                None
            } else if hover_row.is_some() {
                Some(PlacementKind::Onto)
            } else {
                Some(draft.placement)
            };
            // Display-adjacent neighbors, confirmed as real edges by the
            // same bit that legitimizes gap drops.
            let child_of = |index: usize| -> Option<usize> {
                let above = index.checked_sub(1)?;
                commits.row(above).next_row_is_parent().then_some(above)
            };
            let parent_of = |index: usize| -> Option<usize> {
                (index + 1 < len && commits.row(index).next_row_is_parent()).then_some(index + 1)
            };
            let describe = |placement: PlacementKind| -> Option<String> {
                let target = target?;
                let id = short_id(target)?;
                Some(match placement {
                    PlacementKind::Onto => format!("new child of {id}"),
                    PlacementKind::After => match child_of(target).and_then(short_id) {
                        Some(child) => format!("between {id} and {child}"),
                        None => format!("directly after {id}"),
                    },
                    PlacementKind::Before => match parent_of(target).and_then(short_id) {
                        Some(parent) => format!("between {parent} and {id}"),
                        None => format!("directly before {id}"),
                    },
                })
            };
            let tone = |placement: PlacementKind| {
                if live_placement != Some(placement) {
                    RowTone::Idle
                } else if target.is_some() {
                    RowTone::Armed
                } else {
                    RowTone::Chosen
                }
            };
            for (id, label, command, placement) in [
                (
                    "op-place-onto",
                    "Onto",
                    "draft.place.onto",
                    PlacementKind::Onto,
                ),
                (
                    "op-place-after",
                    "After",
                    "draft.place.after",
                    PlacementKind::After,
                ),
                (
                    "op-place-before",
                    "Before",
                    "draft.place.before",
                    PlacementKind::Before,
                ),
            ] {
                // The key as bound, so a rebinding shows here too.
                let key = ui
                    .keymap
                    .chord_for(crate::commands::Context::Draft, command)
                    .map(crate::keymap::Chord::display);
                rows_column = rows_column.child(op_target_row(
                    id,
                    label,
                    key,
                    tone(placement),
                    describe(placement),
                    Some(Action::DraftPlacement(placement)),
                    ui,
                    theme,
                    cx,
                ));
            }
            // A drag over a gap is its own, transient way to land: exactly
            // between the two rows around it.
            if let Some((parent, child)) = gap_sides
                && !gap_invalid
                && let (Some(parent), Some(child)) = (short_id(parent), short_id(child))
            {
                rows_column = rows_column.child(op_target_row(
                    "op-place-between",
                    "Between",
                    None,
                    RowTone::Armed,
                    Some(format!("{parent} and {child}")),
                    None,
                    ui,
                    theme,
                    cx,
                ));
            }
        }
        DraftKind::Squash => {
            let tone = if fold_target.is_some() {
                RowTone::Armed
            } else {
                RowTone::Idle
            };
            let description = fold_target
                .and_then(short_id)
                .map(|id| format!("folds {names} into {id} · descriptions combine"));
            rows_column = rows_column.child(op_target_row(
                "op-place-into",
                "Into",
                None,
                tone,
                description,
                None,
                ui,
                theme,
                cx,
            ));
        }
        DraftKind::Merge | DraftKind::Parallelize => {}
    }

    // ---- Footer band: live verdict + counts on the left, Apply on the
    // right, key hints underneath.
    let is_branch = matches!(
        draft.kind,
        DraftKind::Rebase {
            mode: diffui_core::RebaseSourceMode::Branch,
        }
    );
    let (status_icon, status_text, status_color, counts): (
        Option<&'static str>,
        String,
        Color,
        Option<String>,
    ) = match &draft_ui.preview {
        crate::DraftPreviewState::Idle => (
            None,
            match draft.kind {
                DraftKind::Squash => "Pick the revision to fold into".to_owned(),
                DraftKind::Merge => "Pick the other parent".to_owned(),
                DraftKind::New => "Pick where the new change goes".to_owned(),
                DraftKind::Duplicate => "Pick where the copies land".to_owned(),
                DraftKind::Parallelize => "Add another revision to parallelize".to_owned(),
                _ if is_branch => "Pick a destination to resolve the branch".to_owned(),
                _ => "Pick the destination".to_owned(),
            },
            theme.subtle_text,
            None,
        ),
        crate::DraftPreviewState::Loading => {
            (None, "Simulating…".to_owned(), theme.subtle_text, None)
        }
        crate::DraftPreviewState::Ready(diffui_core::DraftSimulation::Rebase(preview)) => {
            // The candidate sits inside the branch itself (a descendant of
            // the source): the branch has nothing outside it to move.
            if is_branch && preview.simulated && preview.moved == 0 {
                (
                    None,
                    "Nothing to move: the branch is already there".to_owned(),
                    theme.muted_text,
                    None,
                )
            } else {
                let total = preview.moved + preview.descendants;
                let mut counts = if is_branch && !preview.entry_points.is_empty() {
                    format!(
                        "Moves the branch from {} · {} revision{}",
                        commit_list_summary(&preview.entry_points),
                        preview.moved,
                        if preview.moved == 1 { "" } else { "s" },
                    )
                } else {
                    format!(
                        "Rebases {total} commit{}",
                        if total == 1 { "" } else { "s" }
                    )
                };
                if preview.abandoned_empty > 0 {
                    counts.push_str(&format!(" · {} emptied", preview.abandoned_empty));
                }
                if !preview.simulated {
                    (
                        None,
                        "Too large to simulate conflicts".to_owned(),
                        theme.muted_text,
                        Some(counts),
                    )
                } else if preview.new_conflicts.is_empty() {
                    (
                        Some(icons::CHECK),
                        "No new conflicts".to_owned(),
                        theme.added_text,
                        Some(counts),
                    )
                } else {
                    (
                        Some(icons::ALERT_TRIANGLE),
                        format!(
                            "Conflicts in {}",
                            commit_list_summary(&preview.new_conflicts)
                        ),
                        theme.conflict_marker,
                        Some(counts),
                    )
                }
            }
        }
        crate::DraftPreviewState::Ready(diffui_core::DraftSimulation::Merge(preview)) => {
            let parents = draft.sources.len() + usize::from(!merge_ready);
            let counts = format!("Merges {parents} parents");
            if preview.conflicts.is_empty() {
                (
                    Some(icons::CHECK),
                    "Clean merge".to_owned(),
                    theme.added_text,
                    Some(counts),
                )
            } else {
                let mut status = format!("Will conflict in {}", preview.conflicts.join(", "));
                if preview.truncated {
                    status.push_str(", …");
                }
                (
                    Some(icons::ALERT_TRIANGLE),
                    status,
                    theme.conflict_marker,
                    Some(counts),
                )
            }
        }
        crate::DraftPreviewState::Ready(diffui_core::DraftSimulation::Squash(preview)) => {
            let counts = preview
                .abandons_sources
                .then(|| format!("Abandons {names}, now empty"));
            conflict_verdict(&preview.new_conflicts, counts, theme)
        }
        crate::DraftPreviewState::Ready(diffui_core::DraftSimulation::Insert(preview)) => {
            let mut counts = format!(
                "Adds {} change{}",
                preview.created,
                if preview.created == 1 { "" } else { "s" }
            );
            if preview.rebased > 0 {
                counts.push_str(&format!(" · rebases {}", preview.rebased));
            }
            conflict_verdict(&preview.new_conflicts, Some(counts), theme)
        }
        crate::DraftPreviewState::Failed(error) => (
            Some(icons::ALERT_TRIANGLE),
            format!("Preview failed: {error}"),
            theme.removed_text,
            None,
        ),
    };
    let mut verdict = Vec::new();
    if let Some(status_glyph) = status_icon {
        verdict.push(Aligned::glyph(
            icons::icon(status_glyph, 13.0, status_color),
            13.0,
        ));
    }
    verdict.push(Aligned::text(
        typography::line(status_text, ui_font, text_size::BODY, status_color),
        ui_font,
        text_size::BODY,
    ));
    let summary = div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(typography::baseline_row(verdict, 6.0))
        .children(
            counts
                .map(|counts| typography::line(counts, ui_font, text_size::BODY, theme.muted_text)),
        );

    // Merge can be complete from its selected parents alone; other drafts
    // execute on the armed candidate. Gap spots only exist mid-drag, where
    // the drop itself is the confirm.
    let can_apply = merge_ready || parallel_ready || target.is_some();
    let apply_color = if can_apply {
        theme.background
    } else {
        theme.subtle_text
    };
    let mut apply = look
        .button("op-bar-apply", "Apply")
        .primary()
        .compact()
        .disabled(!can_apply)
        .gap(px(6.0))
        .child(icons::icon(icons::ENTER, 12.0, apply_color));
    if can_apply {
        apply = apply.on_click(Diffui::on_action(Action::DraftConfirm, cx));
    }

    // ---- The plan: what Apply will do, in words and as the jj command.
    // Until a destination is armed the command stops where it goes.
    let planned_op = if merge_ready || parallel_ready {
        draft.op_from_sources()
    } else if let Some((parent, child)) = gap_sides.filter(|_| !gap_invalid) {
        ui.selection_at_index(parent)
            .zip(ui.selection_at_index(child))
            .and_then(|(parent, child)| draft.op_for_gap(parent, child))
    } else {
        let placement = if hover_row.is_some() {
            PlacementKind::Onto
        } else {
            draft.placement
        };
        target
            .and_then(|index| ui.selection_at_index(index))
            .and_then(|target| draft.op_for(target, placement))
    };
    // Everything the simulation moves besides the picked sources.
    let descendants = sim_rebase.map(|preview| {
        (preview.moved as usize + preview.descendants as usize).saturating_sub(draft.sources.len())
    });
    let sentence = planned_op
        .as_ref()
        .and_then(|op| crate::plan::op_sentence(ui, op, descendants));
    let command = crate::plan::draft_command(ui, draft, planned_op.as_ref()).map(|command| {
        // Still missing its destination: say so.
        if command.ends_with(' ') {
            format!("{command}\u{2026}")
        } else {
            command
        }
    });
    let plan = div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(3.0))
        .children(sentence.map(|sentence| {
            div()
                .w_full()
                .text_size(px(text_size::BODY))
                .child(sentence.view(look, theme.text))
        }))
        .children(command.map(|command| {
            div()
                .w_full()
                .overflow_hidden()
                .child(crate::plan::caption_command_line(
                    ui, theme, &command, window, cx,
                ))
        }));

    // The mode's other keys are in the hint strip docked under the diff
    // (see `crate::mode_hints`); the rows carry their own keycaps and Apply
    // its ↵.
    let band = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .py(px(8.0))
        .px(px(12.0))
        .bg(theme.panel_background)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(summary)
                .child(apply),
        )
        .child(plan);

    // Hairline on top so the bar reads as its own surface pinned above the
    // footer; the rows sit on the elevated surface, the verdict band on the
    // plain panel below them.
    let shows_target_rows = !matches!(draft.kind, DraftKind::Merge | DraftKind::Parallelize);
    div()
        .flex_none()
        .w_full()
        .flex()
        .flex_col()
        .child(crate::root::horizontal_divider(theme))
        .child(
            div()
                .w_full()
                .bg(theme.panel_background_elevated)
                .child(
                    Padding {
                        top: 8.0,
                        right: 12.0,
                        bottom: if shows_target_rows { 4.0 } else { 8.0 },
                        left: 12.0,
                    }
                    .apply(div())
                    .w_full()
                    .child(headline),
                )
                .child(
                    Padding {
                        top: 0.0,
                        right: 6.0,
                        bottom: if shows_target_rows { 8.0 } else { 0.0 },
                        left: 6.0,
                    }
                    .apply(div())
                    .w_full()
                    .child(rows_column),
                ),
        )
        .child(band)
        .into_any_element()
}

/// The monospace revset (jj) / revision-range (git) input at the top of the
/// sidebar, with a caret that opens the presets menu. Submitting (Enter) or
/// picking a preset re-evaluates the log.
fn build_revset_filter(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let placeholder = match ui.active().repository.as_ref().map(|r| r.vcs) {
        Some(Vcs::Git) => "Revisions, e.g. --all",
        _ => "Revset, e.g. all()",
    };
    field::filter_field(
        theme,
        ui.config.mono_font,
        field::FilterField {
            id: REVSET_INPUT_ID,
            leading_icon: Some(icons::GIT_BRANCH),
            placeholder,
            value: &ui.active().session.revset,
            on_input: |this, value, cx| this.revset_field_edited(value, cx),
            on_submit: Some(|this, cx| this.perform(Action::SubmitRevset, cx)),
            caret: Some(field::FilterCaret {
                hovered: ui.hovered == Some(HoverTarget::RevsetCaret),
                target: HoverTarget::RevsetCaret,
                menu: ToolbarMenu::RevsetPresets,
            }),
        },
        window,
        cx,
    )
}

/// Thin status bar pinned to the bottom of the commit-log pane: the working
/// copy's branch + ahead/behind on the left, the total change count on the
/// right. Monospace, dim, with a hairline top border. The "uncommitted files"
/// count common to git tools is deliberately omitted — jj's `@` is always a
/// commit, so it would be semantically wrong here.
#[cfg(test)]
mod tests {
    use super::*;
    use diffui_core::{CommitStore, CommitStoreBuilder, CommitSummary};

    fn commit_summary(change_id: &str) -> CommitSummary {
        CommitSummary {
            change_id: change_id.to_owned(),
            commit_id: change_id.to_owned(),
            shortest_change_id_len: None,
            description: String::new(),
            author: String::new(),
            author_timestamp: None,
            has_description: false,
            is_empty: None,
            has_conflict: false,
            is_divergent: false,
            is_hidden: false,
            change_offset: None,
            is_working_copy: false,
            is_immutable: false,
            on_trunk: false,
            bookmarks: Vec::new(),
            parent_ids: Vec::new(),
        }
    }

    fn store(commits: Vec<CommitSummary>) -> CommitStore {
        let mut builder = CommitStoreBuilder::with_capacity(commits.len());
        for commit in commits {
            builder.push(commit);
        }
        builder.finish()
    }

    #[test]
    fn revision_id_prefix_uses_shortest_unique_change_id() {
        // No precomputed lengths (the git backend leaves them `None`), so this
        // exercises the sorted-neighbor derivation. "abc"/"abd" collide on
        // "ab" and need 3 chars to disambiguate; "z" is unique at 1.
        let commits = vec![
            commit_summary("abc"),
            commit_summary("abd"),
            commit_summary("z"),
        ];

        assert_eq!(store(commits).shortest_unique_prefix_lens(), vec![3, 3, 1]);
    }

    #[test]
    fn revision_id_prefix_prefers_precomputed_len() {
        // When jj supplies `shortest_change_id_len` we trust it over the
        // neighbor derivation, clamped to the id's own length.
        let mut commits = vec![commit_summary("abcdef"), commit_summary("abcxyz")];
        commits[0].shortest_change_id_len = Some(4);
        commits[1].shortest_change_id_len = Some(99);

        assert_eq!(store(commits).shortest_unique_prefix_lens(), vec![4, 6]);
    }

    #[test]
    fn revision_id_display_len_keeps_shortest_unique_prefix() {
        let long_id = "abcdefghijklmnopqrstuvwxyz";

        assert_eq!(revision_id_display_len(3, long_id), REVISION_ID_CHARS);
        assert_eq!(revision_id_display_len(16, long_id), 16);
        assert_eq!(revision_id_display_len(99, long_id), long_id.len());
    }

    #[test]
    fn commit_list_summary_keeps_only_the_first_commit() {
        let commits = (0..10)
            .map(|index| format!("commit-{index}"))
            .collect::<Vec<_>>();

        assert_eq!(commit_list_summary(&[]), "");
        assert_eq!(commit_list_summary(&commits[..1]), "commit-0");
        assert_eq!(commit_list_summary(&commits), "commit-0 and 9 more");
    }
}

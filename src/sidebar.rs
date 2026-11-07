use crate::Cx;
use crate::Diffui;
use crate::chip::{self, Chip};
use crate::config::AppConfig;
use crate::graph_view::{self, RevisionGraphStyle};
use crate::icons;
use crate::measure;
use crate::revision_list::RevisionListStyle;
use crate::revision_list::{self};
use crate::theme::Color;
use crate::theme::{self, ThemeSpec, chip_background};
use diffui_core::{DiffFile, FileTreeRow, RowView, file_tree_rows};
use gpui_kit::{AnyElement, Window};
use std::collections::HashSet;
use std::rc::Rc;

// Public sidebar layout knobs — used by main.rs to clamp the resize handle.
// `DEFAULT_WIDTH` is a starting point; the actual floor is derived from the
// configured UI font at runtime by `min_width(...)` so it scales when the
// user picks a larger font in the future. There's no upper cap — users can
// drag the sidebar as wide as the window allows.
pub const DEFAULT_WIDTH: f32 = 340.0;
pub const RESIZE_HIT_PADDING: f32 = 2.0;

/// Minimum sidebar width for the current font config. Picked so the top
/// row of a revision can always fit its load-bearing columns at this font:
/// change-id (12 chars) + commit-id (12 chars) + an abbreviated author +
/// the `+N` bookmark-overflow chip + the loudest status chip
/// (`divergent`, 9 chars). Below this width the chip rail starts dropping
/// rails, so the user loses the conflict/divergence signal.
pub fn min_width(config: AppConfig) -> f32 {
    let id_width =
        |content: &str| measure::line_width(content, CAPTION_TEXT_SIZE, config.mono_font);

    let change_id_w = id_width(&"a".repeat(REVISION_ID_CHARS));
    let commit_id_w = id_width(&"a".repeat(COMMIT_ID_CHARS));
    let at_w = id_width("@");
    let author_w = measure::line_width("Author Name", CAPTION_TEXT_SIZE, config.ui_font);
    let plus_n_w = chip::width("+9", None, config.ui_font);
    let conflict_w = chip::width("divergent", None, config.mono_font);

    let at_gap = 4.0;
    let id_gap = 8.0;
    let chip_gap = 6.0;
    // Gutter for a typical 1-lane row: `lane_strip_width(1) = LANE_WIDTH`.
    let gutter =
        revision_list::GUTTER_LEFT_PADDING + graph_view::LANE_WIDTH + revision_list::GUTTER_PADDING;

    let row_content = at_w
        + at_gap
        + change_id_w
        + id_gap
        + commit_id_w
        + id_gap
        + author_w
        + chip_gap
        + plus_n_w
        + chip_gap
        + conflict_w;
    gutter + row_content + REVISION_CONTENT_RIGHT_PAD
}

/// Mirrors `revision_list::CONTENT_PADDING` — the right-edge padding
/// inside a revision row. Kept here so `min_width` can budget the row
/// without exposing the constant through `revision_list`.
const REVISION_CONTENT_RIGHT_PAD: f32 = crate::scrollbar::GUTTER;

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

const FOOTER_TEXT_SIZE: f32 = theme::text_size::UI;
/// One control step, so the footer is as tall as the header above the list.
const FOOTER_HEIGHT: f32 = theme::control::COMPACT;

/// Group an integer with `,` thousands separators, e.g. 1410 -> "1,410".
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Bookmark chips for a revision row, wearing `lane_color`. Shared with the
/// source browser's revision-header row so both rails style identically.
///
/// Core authors the label shapes and jj forbids `@` inside bookmark and
/// remote names, so a trailing `@` can only be another workspace's working
/// copy (`name@`) and an interior `@` a remote bookmark (`main@origin`).
/// Workspace chips get a folder glyph (another working-copy *directory*) and
/// the working-copy accent — the lane palette is deliberately decoupled from
/// it — so they read as a different kind of thing than the bookmark pills
/// sharing the rail. Remotes render outlined (transparent fill + 1px
/// lane-color border) so they read as "tracking" rather than "live".
/// The graph's lane colours for `theme`.
pub(crate) fn revision_graph_style(theme: ThemeSpec) -> RevisionGraphStyle {
    RevisionGraphStyle {
        lane_base_color: theme.lane_base,
        missing_color: theme.subtle_text,
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

pub(crate) fn bookmark_chips_for(
    bookmarks: &[String],
    lane_color: Color,
    theme: ThemeSpec,
    config: AppConfig,
) -> Vec<Chip> {
    let mut chips = Vec::with_capacity(bookmarks.len());
    for bookmark in bookmarks {
        let is_workspace = bookmark.ends_with('@');
        let is_remote = !is_workspace && bookmark.contains('@');
        let chip_color = if is_workspace {
            theme.accent
        } else {
            lane_color
        };
        chips.push(Chip {
            label: bookmark.clone(),
            font: config.ui_font,
            background: if is_remote {
                crate::theme::TRANSPARENT
            } else {
                chip_background(chip_color)
            },
            text_color: chip_color,
            border_color: is_remote.then_some(lane_color),
            border_dashed: false,
            icon: is_workspace.then_some(icons::FOLDER),
        });
    }
    chips
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
            background: chip_background(theme.muted_text),
            text_color: theme.muted_text,
            border_color: tag.contains('@').then_some(theme.subtle_text),
            border_dashed: false,
            icon: Some(icons::TAG),
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

pub(crate) fn revision_list_style(
    theme: ThemeSpec,
    config: AppConfig,
    file_badge_width: f32,
) -> RevisionListStyle {
    RevisionListStyle {
        graph: RevisionGraphStyle {
            // Lane 0 (the trunk) wears `theme.lane_base` — violet by
            // design — and subsequent lanes derive from it via HSL
            // rotation in `RevisionGraphStyle::lane_color`. Kept
            // separate from `theme.accent` so the coral accent stays
            // reserved for working-copy / selection signalling and
            // doesn't compete with the diff add/del greens and reds.
            lane_base_color: theme.lane_base,
            missing_color: theme.subtle_text,
        },
        background: theme.panel_background,
        selected_background: theme.selected_file,
        accent: theme.accent,
        control: theme.info,
        border: theme.border,
        muted_text: theme.muted_text,
        subtle_text: theme.subtle_text,
        accent_text: theme.accent,
        primary_font: config.ui_font,
        mono_font: config.mono_font,
        file_badge_width,
        tooltip_background: theme.panel_background_elevated,
        tooltip_text: theme.text,
        tooltip_border: theme.border,
        scrollbar: theme::scrollbar_style(theme),
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

/// Stub: the sidebar view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_sidebar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("sidebar::build_sidebar")
}

/// Stub: the sidebar view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_collapsed_sidebar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("sidebar::build_collapsed_sidebar")
}

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

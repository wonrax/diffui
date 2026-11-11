//! The history list in the sidebar: everything that turns a loaded graph, a
//! draft and its preview into [`RevisionList`](crate::revision_list) rows, and
//! the list's gestures back into handlers.

use std::rc::Rc;

use diffui_core::history::{now_millis, relative_age};
use diffui_core::{RevisionSelection, RowView};
use gpui_kit::{AnyElement, App, Window};

use crate::chip::Chip;
use crate::config::AppConfig;
use crate::geometry::{Point, Rectangle};
use crate::graph_layout::GraphLayout;
use crate::graph_view::NodeKind;
use crate::graph_view::RevisionGraphStyle;
use crate::icons;
use crate::revision_list::{
    self, DragHooks, DropSpot, RevisionListProps, RevisionRowView, RowSelectionKey,
};
use crate::sidebar::{
    COMMIT_ID_CHARS, REVISION_ID_CHARS, bookmark_chips_for, commit_description_color,
    draft_panel_background, revision_graph_style, revision_id_display_len, revision_list_style,
    status_chips_for, tag_chips_for, truncate_end,
};
use crate::theme::ThemeSpec;
use crate::{Cx, Diffui};

/// The revision list of the sidebar, filling what it's given.
pub(crate) fn build_revision_list(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let tab = ui.active();

    let reveal_target = tab
        .op_draft()
        .and_then(|draft| draft.draft.candidate)
        .and_then(|candidate| tab.list_row(candidate));
    let row_count = tab
        .shown_preview()
        .map_or(tab.session.commits.len(), |preview| preview.len());
    let selected_row = match &tab.session.selected_revision {
        RevisionSelection::WorkingCopy => Some(RowSelectionKey::WorkingCopy),
        RevisionSelection::Commit(id) => Some(RowSelectionKey::Commit(id.clone())),
    };

    // The list fills its own background, so the target-mode wash has to reach
    // it too: a tinted container alone would be painted over.
    let mut list_style = revision_list_style(theme, ui.config, 0.0);
    if tab.op_draft().is_some() {
        list_style.background = draft_panel_background(theme);
        // The rings that cut nodes out of their strokes are the pane's colour.
        list_style.graph.surface = list_style.background;
    }

    // The per-row lane fold + prefix lengths are precomputed once and held in
    // the session; the closure below builds a single visible row's view from
    // them on demand, so the list never materializes all ~N rows.
    let this = cx.entity();
    let build = {
        let this = this.clone();
        Rc::new(move |row: usize, cx: &App| revision_row_at(this.read(cx), theme, row))
    };

    let working_copy_row = tab
        .session
        .commits
        .working_copy_index()
        .and_then(|index| tab.list_row(index));
    let selected_list_row = tab
        .session
        .selected_commit_index
        .and_then(|index| tab.list_row(index));
    let mut props = RevisionListProps::new(row_count, None, list_style)
        .revisions(
            build,
            selected_row,
            selected_list_row,
            Diffui::listen(cx, |this, key: &RowSelectionKey, window, cx| {
                // Whatever the modifier events did or didn't reach: the click
                // knows what's held.
                this.modifiers = window.modifiers().into();
                this.on_select_row_key(key.clone(), cx);
            }),
        )
        .at_window_edge()
        .working_copy(working_copy_row)
        .multi_selection_count(tab.revision_multi_selection.len())
        .reveal_selected(tab.revision_reveal_token)
        .reveal_file(tab.candidate_reveal_token, reveal_target)
        // Every tick of a scroll lands here: stored, not acted on.
        .on_scroll(cx.listener(|this: &mut Diffui, offset: &f64, _, _| {
            this.active_mut().sidebar_scroll_offset = *offset;
        }))
        .top_fade(list_style.background)
        .restore_scroll(tab.sidebar_scroll_offset, ui.scroll_restore_token)
        .on_context_menu(Diffui::listen(
            cx,
            |this, (key, rect, point): &(RowSelectionKey, Rectangle, Point), _, cx| {
                this.on_revision_context_menu(key.clone(), *rect, *point, cx);
            },
        ))
        .on_file_context_menu(Diffui::listen(
            cx,
            |this, (row, rect, point): &(usize, Rectangle, Point), _, cx| {
                this.on_sidebar_file_context_menu(*row, *rect, *point, cx);
            },
        ));

    // Drag-to-rebase, for mutable (local jj) repos only.
    if tab.session.capabilities.mutate {
        let start = Diffui::listen(cx, |this, row: &usize, window, cx| {
            // ⌥ at drag start opts into moving the whole subtree.
            this.modifiers = window.modifiers().into();
            this.on_revision_drag_start(*row, cx);
        });
        let hover = Diffui::listen(cx, |this, spot: &Option<DropSpot>, _, cx| {
            this.on_revision_drag_hover(*spot, cx);
        });
        let drop = Diffui::listen(cx, |this, spot: &Option<DropSpot>, window, _| {
            // ⌥ at the drop upgrades the move to "with descendants".
            this.modifiers = window.modifiers().into();
            this.on_revision_drag_drop(*spot);
        });
        props = props
            .on_drag(DragHooks {
                start: Rc::new(start),
                hover: Rc::new(hover),
                drop: Rc::new(drop),
            })
            .gap_edges(move |row, cx| {
                // A gap is a real parent edge between two loaded rows that are
                // neighbours in the list too.
                let tab = this.read(cx).active();
                match (tab.target_row(row), tab.target_row(row + 1)) {
                    (Some(above), Some(below)) => {
                        below == above + 1 && tab.session.commits.row(above).next_row_is_parent()
                    }
                    _ => false,
                }
            });
        if tab.op_draft().is_some() {
            props = props
                .on_target_hover(Diffui::listen(cx, |this, index: &Option<usize>, _, cx| {
                    this.on_draft_hover_candidate(*index, cx)
                }));
        }
        // A preview rearranges the rows under a still cursor: draw the drop
        // where the draft has it, not where the cursor last reported it.
        if tab.shown_preview().is_some()
            && let Some(spot) = tab.op_draft().and_then(|draft| draft.hover_spot)
        {
            let shown = match spot {
                DropSpot::OnRow(index) => tab.list_row(index).map(DropSpot::OnRow),
                DropSpot::Gap { above, below } => tab
                    .list_row(above)
                    .zip(tab.list_row(below))
                    .map(|(above, below)| DropSpot::Gap { above, below }),
            };
            props = props.drop_indicator(shown);
        }
    }
    revision_list::view("revision-list", props, window, cx)
}

/// The view of list row `row`, whatever is drawn at the moment: the loaded
/// graph, or the post-op graph while a draft is previewed.
fn revision_row_at(ui: &Diffui, theme: ThemeSpec, row: usize) -> RevisionRowView {
    let tab = ui.active();
    let config = ui.config;
    let commits = &tab.session.commits;
    if commits.is_empty() && tab.shown_preview().is_none() {
        return crate::source_panel::empty_revision_row(theme);
    }
    let graph_style = revision_graph_style(theme);
    let draft = tab.op_draft();
    let prefix_len = |index: usize| tab.session.sidebar_prefix_lens.get(index).copied();
    let slot = match tab.shown_preview() {
        Some(preview) if row < preview.len() => {
            let role = preview.rows[row];
            let commit = preview.commit(commits, row);
            RowSlot {
                commit,
                layout: &preview.layout,
                row,
                unique_len: match role.source {
                    diffui_core::RowSource::Loaded(index) => prefix_len(index),
                    diffui_core::RowSource::Extra(_) => commit.shortest_change_id_len(),
                },
                target: preview.target_at(row),
                preview: Some(role),
            }
        }
        // An event can land between a state change and the next render, with a
        // row of the graph that was drawn before. Clamp rather than panic; the
        // render that's on its way fixes it.
        _ => {
            let row = row.min(commits.len().saturating_sub(1));
            RowSlot {
                commit: commits.row(row),
                layout: &tab.session.graph,
                row,
                unique_len: prefix_len(row),
                target: Some(row),
                preview: None,
            }
        }
    };
    let ghost = slot.preview.is_some_and(|row| row.role.is_ghost());
    let commit_id = slot.commit.commit_id();
    let mut view = build_revision_row(
        slot,
        theme,
        config,
        &graph_style,
        &tab.revision_multi_selection,
        draft,
    );
    // While the palette holds a command line, the rows its focused revset
    // argument matches wear the target-mode wash, labelled with the flag.
    if draft.is_none()
        && let Some((flag, matched)) = ui.command_line().and_then(|command| command.tint())
        && !view.draft_source
        && matched.contains(commit_id)
    {
        view.draft_source = true;
        if !flag.is_empty() {
            view.status_chips.insert(
                0,
                Chip {
                    label: flag.clone(),
                    font: config.mono_font,
                    background: crate::theme::TRANSPARENT,
                    text_color: theme.accent,
                    border_color: Some(theme.accent),
                    border_dashed: true,
                    icon: None,
                    icon_color: None,
                    suffix: None,
                },
            );
        }
    }
    // Tags trail the bookmarks, so the rail's `+N` overflow sheds them first.
    if !ghost && let Some(tags) = tab.session.bookmarks.tags.get(commit_id) {
        view.bookmark_chips
            .extend(tag_chips_for(tags, theme, config));
    }
    view
}

/// One list row's inputs: its commit data, where its lanes live, and how it
/// relates to the loaded graph.
struct RowSlot<'a> {
    commit: RowView<'a>,
    layout: &'a GraphLayout,
    /// The row's index in `layout` (and in the list).
    row: usize,
    unique_len: Option<usize>,
    /// The loaded row this row aims at as a draft destination.
    target: Option<usize>,
    /// What the previewed operation does to it, while a preview is drawn.
    preview: Option<diffui_core::PreviewRow>,
}

/// Build the display view for one revision row from the precomputed fold +
/// store. Called only for on-screen rows.
fn build_revision_row(
    slot: RowSlot<'_>,
    theme: ThemeSpec,
    config: AppConfig,
    graph_style: &RevisionGraphStyle,
    multi_selection: &[String],
    draft: Option<&crate::DraftUi>,
) -> RevisionRowView {
    let RowSlot {
        commit,
        layout: graph,
        row: index,
        unique_len,
        target,
        preview,
    } = slot;
    let role = preview.map(|row| row.role);
    let ghost = role.is_some_and(diffui_core::PreviewRole::is_ghost);
    let created = role == Some(diffui_core::PreviewRole::Created);
    let change_id = if created { "" } else { commit.change_id() };
    let lane_frame = graph.frame(index, usize::MAX);
    // Warped display columns for this row and the one above (row 0 has no
    // transition, so its own packing stands in).
    let columns = lane_frame.display_columns();
    let prev_columns = if index > 0 {
        graph.frame(index - 1, usize::MAX).display_columns()
    } else {
        columns.clone()
    };
    let bookmarks = if ghost { &[] } else { commit.bookmarks() };
    let unique_len = unique_len.unwrap_or(REVISION_ID_CHARS);
    let label_len = revision_id_display_len(unique_len, change_id);
    let id_prefix: String = change_id.chars().take(unique_len).collect();
    let mut id_suffix: String = change_id
        .chars()
        .skip(unique_len)
        .take(label_len.saturating_sub(unique_len))
        .collect();
    // jj log prints divergent / hidden copies as `changeid/N`; the suffix is
    // how a revset addresses one copy (`-r xyz/1`), so render the same form.
    if let Some(offset) = commit.change_offset() {
        id_suffix.push('/');
        id_suffix.push_str(&offset.to_string());
    }
    // Git has no change ids: its rows carry the commit hash in both slots, and
    // its synthesized working copy a placeholder in both. Show the hash once,
    // and nothing but the "Working copy" label on the working copy.
    let single_id = created
        || commit.commit_id().starts_with(change_id)
        || change_id.starts_with(commit.commit_id());
    let (id_prefix, id_suffix) = if single_id && commit.is_working_copy() {
        (String::new(), String::new())
    } else {
        (id_prefix, id_suffix)
    };
    let commit_id_short = if single_id {
        String::new()
    } else {
        truncate_end(commit.commit_id(), COMMIT_ID_CHARS)
    };

    let lane_color = graph_style.lane_color(lane_frame.node_lane);
    let bookmark_chips = bookmark_chips_for(bookmarks, lane_color, theme, config);
    let mut status_chips = if ghost {
        Vec::new()
    } else {
        status_chips_for(commit, theme, config)
    };
    // A conflict the operation would cause wears the outlined (forecast)
    // form of the conflict chip.
    if preview.is_some_and(|row| row.new_conflict) {
        for chip in status_chips
            .iter_mut()
            .filter(|chip| chip.label == "conflict")
        {
            chip.background = crate::theme::TRANSPARENT;
            chip.border_color = Some(theme.conflict_marker);
            chip.border_dashed = true;
        }
    }

    let selection_key = if commit.is_working_copy() && !ghost {
        RowSelectionKey::WorkingCopy
    } else {
        RowSelectionKey::Commit(commit.commit_id().to_owned())
    };
    let multi_selected = multi_selection.iter().any(|id| id == commit.commit_id());

    // Target-mode decorations: the draft's sources wear the lifted-off wash —
    // and so does every row the last simulation resolved into the moved set,
    // so a branch-mode draft shows the whole branch that would move. The
    // keyboard candidate wears its destination marker (the widget draws live
    // *drag* indicators itself, so they're suppressed here mid-drag).
    let draft_source = !ghost
        && !created
        && draft.is_some_and(|ui| {
            ui.draft.is_source(commit.commit_id())
                || ui.moved_highlight.contains(commit.commit_id())
        });
    let draft_marker = draft.and_then(|ui| {
        use diffui_core::{DraftKind, PlacementKind};
        use revision_list::DraftMarker;
        if ui.hover_spot.is_some()
            || matches!(ui.draft.kind, DraftKind::Merge) && ui.draft.sources.len() >= 2
            || matches!(ui.draft.kind, DraftKind::Parallelize)
        {
            return None;
        }
        // Hover can arm any row as candidate, including a source (keyboard
        // nav skips them) — an invalid target gets the red op-bar hint, not
        // a destination decoration.
        if !ui.draft.target_valid(commit.commit_id()) {
            return None;
        }
        let candidate = ui.draft.candidate?;
        (Some(candidate) == target).then_some(match ui.draft.kind {
            DraftKind::Squash | DraftKind::Merge | DraftKind::Parallelize => DraftMarker::Ring,
            DraftKind::Rebase { .. } | DraftKind::New | DraftKind::Duplicate => {
                match ui.draft.placement {
                    PlacementKind::Onto => DraftMarker::Ring,
                    // After = between the target and its children ⇒ lands just
                    // above the row; Before ⇒ just below it.
                    PlacementKind::After => DraftMarker::LineAbove,
                    PlacementKind::Before => DraftMarker::LineBelow,
                }
            }
        })
    });
    // Target-mode role chips: say in words what the wash/marker mean — every
    // revision that would move wears the verb, the live destination wears the
    // op bar's destination word. Lead the status rail so the role reads first.
    // Styled apart from the bookmark pills sharing the rail: moved rows are
    // dashed-outlined (the app's transient/preview language) with a ↑ "lifting
    // off" glyph — quiet enough to repeat down a whole branch — while the one
    // destination row is the only solid-filled chip in the app, with a ↓
    // "lands here" glyph. Background-colored text on the accent fill is
    // legible in every theme because accent is itself legible on background.
    if let Some(ui) = draft {
        use diffui_core::{DraftKind, PlacementKind};
        if draft_source {
            status_chips.insert(
                0,
                Chip {
                    label: match ui.draft.kind {
                        DraftKind::Rebase { .. } => "move",
                        DraftKind::Squash => "squash",
                        DraftKind::Merge => "merge",
                        DraftKind::New => "new",
                        DraftKind::Duplicate => "copy",
                        DraftKind::Parallelize => "parallel",
                    }
                    .to_owned(),
                    font: config.mono_font,
                    background: crate::theme::TRANSPARENT,
                    text_color: theme.accent,
                    border_color: Some(theme.accent),
                    border_dashed: true,
                    icon: Some(icons::ARROW_UP),
                    icon_color: None,
                    suffix: None,
                },
            );
        } else {
            // The live destination: the drag spot's row while dragging, else
            // the armed candidate. A gap spot gets no row chip — the
            // insertion line carries the "between" meaning.
            let merge_ready = matches!(ui.draft.kind, DraftKind::Merge)
                && ui.draft.sources.len() >= 2
                || matches!(ui.draft.kind, DraftKind::Parallelize);
            let destination_word = if merge_ready {
                None
            } else {
                match ui.hover_spot {
                    Some(revision_list::DropSpot::OnRow(spot)) if Some(spot) == target => {
                        Some(match ui.draft.kind {
                            // Drops land Onto regardless of the armed placement.
                            DraftKind::Rebase { .. }
                            | DraftKind::New
                            | DraftKind::Duplicate
                            | DraftKind::Parallelize => "onto",
                            DraftKind::Squash => "into",
                            DraftKind::Merge => "with",
                        })
                    }
                    Some(_) => None,
                    None => (ui
                        .draft
                        .candidate
                        .is_some_and(|candidate| Some(candidate) == target))
                    .then_some(match ui.draft.kind {
                        DraftKind::Rebase { .. }
                        | DraftKind::New
                        | DraftKind::Duplicate
                        | DraftKind::Parallelize => match ui.draft.placement {
                            PlacementKind::Onto => "onto",
                            PlacementKind::After => "after",
                            PlacementKind::Before => "before",
                        },
                        DraftKind::Squash => "into",
                        DraftKind::Merge => "with",
                    }),
                }
            };
            if let Some(word) = destination_word {
                status_chips.insert(
                    0,
                    Chip {
                        label: word.to_owned(),
                        font: config.mono_font,
                        background: theme.accent,
                        text_color: theme.background,
                        border_color: None,
                        border_dashed: false,
                        icon: Some(icons::ARROW_DOWN),
                        icon_color: None,
                        suffix: None,
                    },
                );
            }
        }
    }
    // A commit the operation creates says which kind, in the same outlined
    // language as the moving rows.
    if created && let Some(ui) = draft {
        use diffui_core::DraftKind;
        status_chips.insert(
            0,
            Chip {
                label: match ui.draft.kind {
                    DraftKind::Merge => "merge",
                    DraftKind::Duplicate => "copy",
                    _ => "new",
                }
                .to_owned(),
                font: config.mono_font,
                background: crate::theme::TRANSPARENT,
                text_color: theme.accent,
                border_color: Some(theme.accent),
                border_dashed: true,
                icon: None,
                icon_color: None,
                suffix: None,
            },
        );
    }

    let lane = graph.fold(index, usize::MAX);
    RevisionRowView {
        selection_key,
        change_id_prefix: id_prefix,
        change_id_suffix: id_suffix,
        commit_id_short,
        author: commit.author().to_owned(),
        age: commit
            .author_timestamp()
            .map(|then| relative_age(then, now_millis()))
            .unwrap_or_default(),
        description: commit.description().to_owned(),
        description_color: commit_description_color(commit, theme),
        bookmark_chips,
        status_chips,
        lane_color,
        frame: lane_frame,
        columns,
        prev_columns,
        lane_labels: lane.labels,
        lane_segments_before: lane.segments_before,
        lane_segments_after: lane.segments_after,
        collapse_chevron: None,
        draft_source,
        multi_selected: multi_selected && !ghost,
        draft_marker,
        inert: role.is_some_and(|role| !role.is_target()),
        ghost,
        node: if ghost {
            NodeKind::Ghost
        } else if commit.is_working_copy() {
            NodeKind::WorkingCopy
        } else if commit.has_conflict() {
            NodeKind::Conflict
        } else if commit.is_immutable() {
            NodeKind::Immutable
        } else {
            NodeKind::Mutable
        },
    }
}

#[cfg(test)]
pub(crate) mod scenes {
    use gpui_kit::base::TestSupportExt as _;
    use gpui_kit::prelude::*;
    use gpui_kit::test::{ClickOptions, TestEventExt as _, TestWindowExt as _};
    use gpui_kit::{InputEvent as _, Modifiers, MouseButton, Pixels, ScrollDelta, div, point, px};

    use super::*;
    use crate::Action;
    use crate::harness::{Harness, fixture_repo};
    use crate::theme::ThemePreference;

    /// Where the list starts in the baseline shots: under the tab strip, the
    /// toolbar and the revset field.
    const LIST_TOP: f32 = 80.0;
    const LANE: &str = "history";
    const SIZE: (f32, f32) = (1440.0, 900.0);
    const FRAME: &str = "revision-list-frame";

    /// The list where the sidebar puts it, alone in the window: the sidebar
    /// around it is the panels lane's.
    pub(crate) fn show_list(h: &mut Harness) {
        h.show(|ui, theme, window, cx| {
            let list = build_revision_list(ui, theme, window, cx);
            div()
                .size_full()
                .flex()
                .flex_row()
                .child(
                    div()
                        .flex_none()
                        .w(px(ui.sidebar_width))
                        .h_full()
                        .flex()
                        .flex_col()
                        .pt(px(LIST_TOP))
                        .bg(theme.panel_background)
                        .child(
                            div()
                                .id(FRAME)
                                .test_support()
                                .flex_1()
                                .min_h_0()
                                .child(list),
                        ),
                )
                .child(crate::root::vertical_divider(theme))
                .into_any_element()
        });
    }

    /// The middle of list row `row` in the window, with nothing scrolled.
    pub(crate) fn row_at(row: usize) -> gpui_kit::Point<Pixels> {
        point(
            px(150.0),
            px(LIST_TOP + (row as f32 + 0.5) * revision_list::REVISION_ROW_HEIGHT),
        )
    }

    fn open_repo(repo: &std::path::Path, revset: &str) -> Harness {
        let mut h = Harness::open_sized(Some(repo), SIZE);
        h.wait_for_graph();
        show_list(&mut h);
        if !revset.is_empty() {
            h.perform(Action::SetRevset(revset.to_owned()));
            h.wait_for_graph();
        }
        h
    }

    fn open(revset: &str) -> Harness {
        open_repo(&fixture_repo(), revset)
    }

    /// The row of the first revision whose description holds `text`.
    pub(crate) fn row_of(h: &mut Harness, text: &str) -> usize {
        h.read(|ui| {
            let commits = &ui.active().session.commits;
            commits
                .iter()
                .position(|row| row.description().contains(text))
                .unwrap_or_else(|| panic!("no revision says {text:?}"))
        })
    }

    /// A click on list row `row` with modifiers held, through the real dispatch.
    fn click_row(h: &mut Harness, row: usize, modifiers: Modifiers) {
        let position = row_at(row);
        h.with_window(|window, cx| {
            window.click_with_options(
                FRAME,
                ClickOptions::new()
                    .with_offset(point(position.x, position.y - px(LIST_TOP)))
                    .with_modifiers(modifiers),
                cx,
            );
        });
        h.settle();
    }

    fn cmd() -> Modifiers {
        Modifiers {
            platform: true,
            ..Modifiers::default()
        }
    }

    fn select(h: &mut Harness, text: &str) {
        let row = row_of(h, text);
        click_row(h, row, Modifiers::default());
    }

    #[test]
    fn graph_all() {
        let mut h = open("all()");
        h.shot(LANE, "graph-all");
        // Scrolled to the middle: thumb, gutter, rows short of it.
        h.with_window(|window, cx| {
            window.scroll(FRAME, ScrollDelta::Pixels(point(px(0.0), px(-1200.0))), cx);
        });
        let offset = h.read(|ui| ui.active().sidebar_scroll_offset);
        assert_eq!(
            offset, 1200.0,
            "the wheel scrolled the list and reported it"
        );
        h.shot(LANE, "graph-all-scrolled");
    }

    #[test]
    fn graph_presets() {
        let mut h = open("bookmarks() | remote_bookmarks()");
        h.shot(LANE, "graph-bookmark-states");
        h.perform(Action::SetRevset("conflicts()".to_owned()));
        h.wait_for_graph();
        h.shot(LANE, "graph-conflict-preset");
    }

    #[test]
    fn marks_are_distinct_from_the_selection() {
        let mut h = open("");
        click_row(&mut h, 0, Modifiers::default());
        for row in [1, 2, 4] {
            click_row(&mut h, row, cmd());
        }
        assert_eq!(h.read(|ui| ui.active().revision_multi_selection.len()), 3);
        assert_eq!(
            h.read(|ui| ui.active().session.selected_commit_index),
            Some(0),
            "a mark doesn't navigate"
        );
        h.shot(LANE, "graph-marked-rows");
    }

    #[test]
    fn a_plain_click_selects_on_release_and_a_drift_aborts() {
        let mut h = open("");
        let from = row_at(3);
        let elsewhere = row_at(6);
        h.with_window(|window, cx| {
            window.pointer_down(from, MouseButton::Left, cx);
            window.pointer_up(from, MouseButton::Left, cx);
        });
        h.settle();
        assert_eq!(
            h.read(|ui| ui.active().session.selected_commit_index),
            Some(3)
        );
        // Under the drag threshold, on another row: nothing happens. (Rows are
        // farther apart than the threshold only vertically, so go sideways out
        // of the list instead of crossing it.)
        let before = h.read(|ui| ui.active().session.selected_commit_index);
        h.with_window(|window, cx| {
            window.pointer_down(elsewhere, MouseButton::Left, cx);
            window.pointer_up(point(px(1000.0), elsewhere.y), MouseButton::Left, cx);
        });
        h.settle();
        assert_eq!(
            h.read(|ui| ui.active().session.selected_commit_index),
            before
        );
    }

    #[test]
    fn hovering_a_row_and_a_lane() {
        let mut h = open("");
        h.hover_point(row_at(2));
        h.shot(LANE, "hover-row");
        // The gutter over a lane with labels shows the branch's name.
        h.hover_point(point(px(13.0), row_at(1).y));
        h.shot(LANE, "hover-lane-tooltip");
    }

    #[test]
    fn long_text_at_the_minimum_width() {
        let mut h = open("all()");
        h.act(|ui, _| ui.sidebar_width = crate::sidebar::min_width(ui.config));
        h.shot(LANE, "longtext-graph-narrow-sidebar");
    }

    /// j/k walk the rows in target mode (outside it they walk files), keeping
    /// the candidate on screen.
    #[test]
    fn keyboard_row_navigation_reveals_the_candidate() {
        let mut h = open("all()");
        click_row(&mut h, 0, Modifiers::default());
        start(&mut h, "revision.rebase.start");
        for _ in 0..25 {
            h.press("j");
        }
        h.settle();
        let (candidate, offset) = h.read(|ui| {
            let tab = ui.active();
            (
                tab.op_draft().expect("target mode").draft.candidate,
                tab.sidebar_scroll_offset,
            )
        });
        let candidate = candidate.expect("a candidate");
        assert!(candidate >= 20, "{candidate}");
        let row_h = f64::from(revision_list::REVISION_ROW_HEIGHT);
        let top = candidate as f64 * row_h;
        let view = f64::from(SIZE.1 - LIST_TOP);
        assert!(
            offset <= top && top + row_h <= offset + view,
            "row {candidate} at offset {offset}"
        );
    }

    fn start(h: &mut Harness, command: &'static str) {
        h.act(|ui, cx| ui.run_command(command, crate::commands::CommandArg::None, cx));
        h.settle();
    }

    #[test]
    fn draft_scenes() {
        for (scene, command, keys) in [
            ("draft-rebase-start", "revision.rebase.start", &[][..]),
            (
                "draft-rebase-candidate",
                "revision.rebase.start",
                &["j", "j"],
            ),
            (
                "draft-rebase-descendants",
                "revision.rebase.descendants.start",
                &[],
            ),
            ("draft-rebase-branch", "revision.rebase.branch.start", &[]),
            ("draft-squash", "revision.squash.start", &[]),
            ("draft-merge", "revision.merge.start", &["j", "space"]),
            ("draft-new", "revision.new.start", &[]),
            ("draft-duplicate", "revision.duplicate.start", &[]),
            ("draft-parallelize", "revision.parallelize.start", &[]),
        ] {
            let mut h = open("");
            select(&mut h, "clamp the sample count");
            start(&mut h, command);
            for key in keys {
                h.press(key);
                h.settle();
            }
            assert!(
                h.read(|ui| ui.active().op_draft().is_some()),
                "{scene} is in target mode"
            );
            h.shot(LANE, scene);
        }
        // The state the scene names, placement armed. (`a` and `b` apply on the
        // spot, which is what the baseline shots of these two caught.)
        for (scene, placement) in [
            ("draft-rebase-after", diffui_core::PlacementKind::After),
            ("draft-rebase-before", diffui_core::PlacementKind::Before),
        ] {
            let mut h = open("");
            select(&mut h, "clamp the sample count");
            start(&mut h, "revision.rebase.start");
            h.press("j");
            h.settle();
            h.perform(Action::DraftPlacement(placement));
            h.shot(LANE, scene);
        }
        let mut h = open("");
        select(&mut h, "clamp the sample count");
        start(&mut h, "revision.rebase.start");
        h.press("j");
        h.press("j");
        h.settle();
        h.perform(Action::SetTheme(ThemePreference::Dark));
        h.shot(LANE, "draft-dark");
    }

    #[test]
    fn dragging_a_row_arms_the_spot_under_the_cursor() {
        let mut h = open("all()");
        let from = row_at(row_of(&mut h, "clamp the sample count"));
        let to = row_at(row_of(&mut h, "docs(changelog)"));
        h.with_window(|window, cx| {
            window.pointer_down(from, MouseButton::Left, cx);
            window.pointer_move(to, Some(MouseButton::Left), cx);
        });
        h.settle();
        let (source_count, spot) = h.read(|ui| {
            let draft = ui.active().op_draft().expect("the drag started a draft");
            (draft.draft.sources.len(), draft.hover_spot)
        });
        assert_eq!(source_count, 1);
        assert!(matches!(spot, Some(DropSpot::OnRow(_))), "{spot:?}");
        h.shot(LANE, "draft-drag");
        // Released outside any spot: stays in target mode.
        h.with_window(|window, cx| {
            window.pointer_move(point(px(1000.0), to.y), Some(MouseButton::Left), cx);
            window.pointer_up(point(px(1000.0), to.y), MouseButton::Left, cx);
        });
        h.settle();
        let draft = h.read(|ui| ui.active().op_draft().map(|draft| draft.hover_spot));
        assert_eq!(draft, Some(None));
    }

    #[test]
    fn a_marked_batch_drags_as_one_and_names_its_size() {
        let mut h = open("all()");
        let top = row_of(&mut h, "clamp the sample count");
        let next = row_of(&mut h, "cache resolved bindings");
        click_row(&mut h, top, cmd());
        click_row(&mut h, next, cmd());
        let to = row_at(row_of(&mut h, "docs(changelog)"));
        h.with_window(|window, cx| {
            window.pointer_down(row_at(top), MouseButton::Left, cx);
            window.pointer_move(to, Some(MouseButton::Left), cx);
        });
        h.settle();
        let (sources, marks) = h.read(|ui| {
            let tab = ui.active();
            (
                tab.op_draft().expect("a draft").draft.sources.len(),
                tab.revision_multi_selection.len(),
            )
        });
        assert_eq!(sources, 2);
        assert_eq!(marks, 0, "the draft's wash replaced the marks");
        h.shot(LANE, "draft-batch-sources");
    }

    /// A private copy of the fixture, for a test that mutates it.
    fn scratch_copy(name: &str) -> std::path::PathBuf {
        let scratch = crate::harness::scratch_home().join(name);
        let _ = std::fs::remove_dir_all(&scratch);
        let copied = std::process::Command::new("cp")
            .arg("-R")
            .arg(fixture_repo())
            .arg(&scratch)
            .status()
            .expect("copy the fixture");
        assert!(copied.success());
        scratch
    }

    /// What `jj log` says of `revset` in `repo`.
    fn jj(repo: &std::path::Path, revset: &str, template: &str) -> String {
        let program = std::env::var("JJ").unwrap_or_else(|_| "/tmp/diffui-fix/uikit/jj".into());
        let output = std::process::Command::new(program)
            .args(["--ignore-working-copy", "--no-pager", "log", "--no-graph"])
            .args(["-r", revset, "-T", template])
            .current_dir(repo)
            .output()
            .expect("run jj");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn parent_title(repo: &std::path::Path, revset: &str) -> String {
        jj(
            repo,
            &format!("parents({revset})"),
            "description.first_line()",
        )
    }

    fn settled_after_drop(h: &mut Harness) {
        h.wait_until("the draft to end", |ui| ui.active().op_draft().is_none());
        h.wait_until("the rebase and its reload", |ui| {
            !ui.active().session.busy()
        });
        h.settle();
    }

    /// ⌥ at the drop upgrades the move to "with descendants".
    #[test]
    fn option_at_the_drop_brings_the_descendants() {
        let repo = scratch_copy("drag-option");
        let mut h = open_repo(&repo, "main | main..@");
        let main = h.read(|ui| {
            ui.active()
                .session
                .commits
                .iter()
                .position(|row| row.bookmarks().iter().any(|name| name == "main"))
                .expect("main is in the graph")
        });
        let source = row_of(&mut h, "clamp the sample count");
        h.with_window(|window, cx| {
            window.pointer_down(row_at(source), MouseButton::Left, cx);
            window.pointer_move(row_at(main), Some(MouseButton::Left), cx);
        });
        h.settle();
        h.with_window(|window, cx| {
            window.dispatch_event(
                gpui_kit::ModifiersChangedEvent {
                    modifiers: Modifiers {
                        alt: true,
                        ..Modifiers::default()
                    },
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.pointer_up(row_at(main), MouseButton::Left, cx);
        });
        settled_after_drop(&mut h);
        assert!(
            parent_title(&repo, "@").contains("clamp the sample count"),
            "@ came along"
        );
        assert!(
            parent_title(&repo, r#"description(substring:"clamp the sample count")"#)
                .contains("record release candidate 40"),
            "onto the destination"
        );
    }

    /// Esc during a drag ends it: the release that follows does nothing.
    #[test]
    fn escape_abandons_a_drag() {
        let repo = scratch_copy("drag-escape");
        let mut h = open_repo(&repo, "");
        let head = jj(&repo, "@", "commit_id");
        let (from, to) = (row_at(1), row_at(3));
        h.with_window(|window, cx| {
            window.pointer_down(from, MouseButton::Left, cx);
            window.pointer_move(to, Some(MouseButton::Left), cx);
        });
        h.settle();
        assert!(h.read(|ui| ui.active().op_draft().is_some()));
        h.press("escape");
        h.settle();
        h.with_window(|window, cx| window.pointer_up(to, MouseButton::Left, cx));
        h.settle();
        assert!(h.read(|ui| ui.active().op_draft().is_none()));
        assert_eq!(jj(&repo, "@", "commit_id"), head, "nothing moved");
    }

    /// The list's other shape, as the file trees use it: its rows, its clicks
    /// and what a right-click anchors to.
    #[test]
    fn a_files_only_list_reports_display_rows_and_its_card() {
        use std::cell::RefCell;

        use revision_list::{FILE_ROW_HEIGHT, FileRowView};

        #[derive(Default)]
        struct Log {
            selected: Vec<usize>,
            checked: Vec<usize>,
            menus: Vec<(usize, Rectangle)>,
        }
        let log = Rc::new(RefCell::new(Log::default()));

        let mut h = open("");
        let seen = log.clone();
        h.show(move |ui, theme, window, cx| {
            let style = revision_list_style(theme, ui.config, 24.0);
            let added = theme.added_text;
            let build = Rc::new(move |row: usize, _: &App| FileRowView {
                primary: if row == 0 {
                    "src".to_owned()
                } else {
                    format!("a_rather_long_module_name_that_will_not_fit_in_this_pane_{row}.rs")
                },
                raw_path: format!("src/a_rather_long_module_name_{row}.rs"),
                status_label: if row == 0 {
                    String::new()
                } else {
                    "M".to_owned()
                },
                status_background: crate::theme::chip_background(added),
                status_text: added,
                additions: row * 3,
                deletions: row,
                additions_text: added,
                deletions_text: added,
                continuation: Rc::from(Vec::new()),
                columns: Rc::from(Vec::new()),
                additions_width: 30.0,
                deletions_width: 24.0,
                primary_color: added,
                icon_color: added,
                indent: if row == 0 {
                    0.0
                } else {
                    revision_list::FILE_TREE_INDENT
                },
                chevron: (row == 0).then_some(false),
                file_index: if row == 0 { usize::MAX } else { row - 1 },
                lane_labels: Rc::from(Vec::new()),
                lane_segments: Rc::from(Vec::new()),
                check: (row > 0).then_some(match row % 3 {
                    0 => crate::diff_view::CheckState::On,
                    1 => crate::diff_view::CheckState::Off,
                    _ => crate::diff_view::CheckState::Partial,
                }),
                tag: None,
                struck: false,
            });
            let (selected, checked, menus) = (seen.clone(), seen.clone(), seen.clone());
            let props = RevisionListProps::new(0, Some((0, 8)), style)
                .files(build, Some(1), move |row, _, _| {
                    selected.borrow_mut().selected.push(*row);
                })
                .on_file_check(move |row, _, _| checked.borrow_mut().checked.push(*row))
                .on_file_context_menu(move |(row, rect, _), _, _| {
                    menus.borrow_mut().menus.push((*row, *rect));
                });
            let list = revision_list::view("file-tree-scene", props, window, cx);
            div()
                .w(px(320.0))
                .h_full()
                .pt(px(LIST_TOP))
                .bg(theme.panel_background)
                .child(div().size_full().child(list))
                .into_any_element()
        });
        h.settle();
        h.hover_point(point(px(200.0), px(LIST_TOP + 1.5 * FILE_ROW_HEIGHT)));
        h.shot(LANE, "list-file-rows");

        let y = |row: f32| px(LIST_TOP + (row + 0.5) * FILE_ROW_HEIGHT);
        h.click_point(point(px(200.0), y(2.0)));
        assert_eq!(log.borrow().selected, vec![2], "by display row");
        // The icon column: the gutter's padding, one indent and the chevron's.
        let icon_x = revision_list::GUTTER_LEFT_PADDING
            + revision_list::GUTTER_PADDING
            + revision_list::FILE_TREE_INDENT
            + 14.0
            + 9.0;
        h.click_point(point(px(icon_x), y(3.0)));
        assert_eq!(log.borrow().checked, vec![3]);
        assert_eq!(log.borrow().selected, vec![2], "the box isn't a selection");
        h.with_window(|window, cx| {
            window.pointer_down(point(px(200.0), y(1.0)), MouseButton::Right, cx);
            window.pointer_up(point(px(200.0), y(1.0)), MouseButton::Right, cx);
        });
        let (row, rect) = log.borrow().menus[0];
        assert_eq!(row, 1);
        assert_eq!(
            rect.y,
            LIST_TOP + FILE_ROW_HEIGHT + revision_list::ROW_CARD_INSET_Y
        );
        assert_eq!(
            rect.height,
            FILE_ROW_HEIGHT - 2.0 * revision_list::ROW_CARD_INSET_Y
        );
        assert_eq!(rect.x, revision_list::ROW_CARD_INSET_X);
    }

    /// A tab switch's restore lands the list where the tab left it.
    #[test]
    fn a_restore_token_jumps_to_the_saved_offset() {
        let mut h = open("all()");
        let row_h = f64::from(revision_list::REVISION_ROW_HEIGHT);
        h.act(|ui, _| {
            ui.active_mut().sidebar_scroll_offset = 10.0 * row_h;
            ui.scroll_restore_token = ui.scroll_restore_token.wrapping_add(1);
        });
        h.settle();
        // The second row: the working copy's pin floats over the first.
        click_row(&mut h, 1, Modifiers::default());
        assert_eq!(
            h.read(|ui| ui.active().session.selected_commit_index),
            Some(11),
            "the row under the top one is the eleventh"
        );
    }

    /// With the working copy scrolled away, its pin at the top of the list
    /// brings it back.
    #[test]
    fn the_working_copy_pin_scrolls_its_row_back() {
        let mut h = open("all()");
        let row_h = f64::from(revision_list::REVISION_ROW_HEIGHT);
        h.act(|ui, _| {
            ui.active_mut().sidebar_scroll_offset = 10.0 * row_h;
            ui.scroll_restore_token = ui.scroll_restore_token.wrapping_add(1);
        });
        h.settle();
        let width = h.read(|ui| ui.sidebar_width);
        h.click_point(point(px(width / 2.0), px(LIST_TOP + 24.0)));
        h.settle();
        assert_eq!(h.read(|ui| ui.active().sidebar_scroll_offset), 0.0);
    }

    /// A marked stack dragged onto the newest commit at the base lands on it, in
    /// its own order, with the newest of the stack on top, whatever order the
    /// rows were marked in. Through the real repository.
    #[test]
    fn a_marked_stack_lands_on_the_newest_commit_at_the_base() {
        let scratch = scratch_copy("drag-rebase");
        // The stack and the base it's behind, every row on screen.
        let mut h = open_repo(&scratch, "main | main..@");

        let main = h.read(|ui| {
            ui.active()
                .session
                .commits
                .iter()
                .position(|row| row.bookmarks().iter().any(|name| name == "main"))
                .expect("main is in the graph")
        });
        // Oldest first: the mark order must not decide the result.
        let older = row_of(&mut h, "cache resolved bindings");
        let newer = row_of(&mut h, "clamp the sample count");
        assert!(newer < older, "newest at the top");
        click_row(&mut h, older, cmd());
        click_row(&mut h, newer, cmd());

        h.with_window(|window, cx| {
            window.pointer_down(row_at(newer), MouseButton::Left, cx);
            window.pointer_move(row_at(main), Some(MouseButton::Left), cx);
        });
        h.settle();
        assert_eq!(
            h.read(|ui| ui
                .active()
                .op_draft()
                .map(|draft| draft.draft.sources.len())),
            Some(2),
            "the marked row carried the batch"
        );
        h.with_window(|window, cx| {
            window.pointer_up(row_at(main), MouseButton::Left, cx);
        });
        settled_after_drop(&mut h);

        let title = |text: &str| format!("description(substring:{text:?})");
        assert_eq!(
            parent_title(&scratch, &title("cache resolved bindings")),
            jj(&scratch, "main", "description.first_line()"),
            "the stack's oldest sits on the destination"
        );
        assert!(
            parent_title(&scratch, &title("clamp the sample count"))
                .contains("cache resolved bindings"),
            "the stack's newest stays on top of it"
        );
        // Out from under @, which kept what wasn't marked. It's on the graph at
        // the new place.
        h.perform(Action::SetRevset("main::".to_owned()));
        h.wait_for_graph();
        h.shot(LANE, "drag-marked-stack-landed");
    }

    /// A graph with the shape of a busy repository: a trunk, a merge every few
    /// rows and a long-lived side lane now and then, with bookmarks, long titles
    /// and conflicts sprinkled in.
    pub(crate) fn large_graph(
        rows: usize,
    ) -> (diffui_core::CommitStore, crate::graph_layout::GraphLayout) {
        use jj_lib::graph::GraphEdge;
        let mut lanes = diffui_core::graph::LaneAssigner::<usize>::new();
        let mut layout = crate::graph_layout::GraphLayoutBuilder::new();
        let mut store = diffui_core::CommitStoreBuilder::with_capacity(rows);
        for row in 0..rows {
            let mut parents = vec![row + 1];
            if row % 9 == 0 {
                parents.push(row + 5);
            }
            if row % 31 == 0 {
                parents.push(row + 40);
            }
            parents.retain(|parent| *parent < rows);
            let edges: Vec<_> = parents.iter().copied().map(GraphEdge::direct).collect();
            let frame = lanes.push(&row, &edges);
            let bookmarks = if row % 13 == 0 {
                vec![
                    format!("feature/branch-{row}"),
                    format!("feature/branch-{row}@origin"),
                ]
            } else {
                Vec::new()
            };
            layout.push(&frame, &bookmarks);
            store.push(diffui_core::CommitSummary {
                change_id: format!("{row:032x}").replace('0', "k"),
                commit_id: format!("{:040x}", row * 7 + 1),
                shortest_change_id_len: Some(6),
                description: format!(
                    "refactor(area-{}): a change whose title runs longer than the pane is wide, number {row}",
                    row % 17
                ),
                author: "Ada Lovelace".to_owned(),
                author_timestamp: None,
                has_description: true,
                is_empty: Some(row % 19 == 0),
                has_conflict: row % 23 == 0,
                is_divergent: false,
                is_hidden: false,
                change_offset: None,
                is_working_copy: row == 0,
                is_immutable: false,
                on_trunk: false,
                bookmarks,
                parent_ids: Vec::new(),
            });
        }
        (store.finish(), layout.finish())
    }

    fn report(name: &str, mut samples: Vec<std::time::Duration>, paints: usize) {
        samples.sort();
        let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize].as_secs_f64() * 1e3;
        println!(
            "{name:<30} n={:<5} paints={paints:<5} p50={:>6.2}ms p95={:>6.2}ms p99={:>6.2}ms max={:>6.2}ms",
            samples.len(),
            at(0.5),
            at(0.95),
            at(0.99),
            at(1.0),
        );
    }

    fn paints() -> usize {
        revision_list::PAINTS.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Times real frames through the real event path, the way the spike's
    /// `scrollbench` does: the draw flushes when the update returns, and the
    /// paint count says it happened. Run it optimized:
    /// `cargo test --profile fastrel --bin diffui frame_timing -- --ignored --nocapture`
    #[test]
    #[ignore = "a benchmark: run on demand, optimized"]
    fn frame_timing_over_a_large_graph() {
        for rows in [200_000, 1_000_000] {
            let mut h = open("");
            let built = std::time::Instant::now();
            let (store, layout) = large_graph(rows);
            println!("\n== {rows} rows, graph built in {:.0?}", built.elapsed());
            h.act(|ui, _| {
                let session = &mut ui.active_mut().session;
                session.commits = store;
                session.graph = layout;
                session.sidebar_prefix_lens = Vec::new();
            });
            h.settle();
            let inside = row_at(3);

            let scroll = |h: &mut Harness, by: f32| {
                h.timed(|window, cx| {
                    window.dispatch_event(
                        gpui_kit::ScrollEvent {
                            position: inside,
                            delta: ScrollDelta::Pixels(point(px(0.0), px(-by))),
                            ..Default::default()
                        }
                        .to_platform_input(),
                        cx,
                    );
                })
            };

            let before = paints();
            let steps: Vec<_> = (0..600).map(|_| scroll(&mut h, 30.0)).collect();
            report("scroll +30px", steps, paints() - before);
            let offset = h.read(|ui| ui.active().sidebar_scroll_offset);
            assert_eq!(offset, 18_000.0, "the frames really scrolled");

            let before = paints();
            let total = rows as f32 * revision_list::REVISION_ROW_HEIGHT - 900.0;
            let mut state = 0x2545_f491_u64;
            let mut at = offset as f32;
            let jumps: Vec<_> = (0..200)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    let target = (state % total as u64) as f32;
                    let by = target - at;
                    at = target;
                    scroll(&mut h, by)
                })
                .collect();
            report("jump to random offset", jumps, paints() - before);

            let before = paints();
            let hovers: Vec<_> = (0..300)
                .map(|step| {
                    // Raw, since the test helper draws around its event.
                    h.timed(|window, cx| {
                        window.dispatch_event(
                            gpui_kit::MouseMoveEvent {
                                position: row_at(step % 15),
                                ..Default::default()
                            }
                            .to_platform_input(),
                            cx,
                        );
                    })
                })
                .collect();
            report("hover to another row", hovers, paints() - before);

            let before = paints();
            let idle: Vec<_> = (0..100)
                .map(|_| h.timed(|window, cx| window.render_frame(cx)))
                .collect();
            report("redraw, nothing changed", idle, paints() - before);
        }
    }
}

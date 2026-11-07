//! The multi-repo title-bar strip: one tab per open repository plus the
//! controls that manage them.
//!
//! Layout, left → right: a tab for each open repo (VCS badge, `owner/name`
//! with the owner dimmed, an optional uncommitted-changes dot, and a close
//! `×`), a `+` button that opens the path dialog, and — pinned right — a
//! `⌘K` control that opens the command palette (the design's repo-search
//! `⌘P` is folded into our existing palette per the spec). Dragging a tab
//! along the strip reorders it — see [`TabStrip`].
//!
//! The strip and the path dialog both read straight off `&Diffui`, mirroring
//! `sidebar` / `palette` / `find`, so there is no intermediate view model to
//! keep in sync.

use crate::Cx;
use crate::Diffui;
use crate::chrome;
use crate::geometry::Rectangle;
use crate::modes::ModeKind;
use crate::theme::{self, ThemeSpec, radius, text_size};
use gpui_kit::{AnyElement, Window};

/// Focus target id for the open-repository dialog's path field.
pub const OPEN_REPO_INPUT_ID: &str = "open-repo-input";

const TAB_TEXT_SIZE: f32 = text_size::UI;

/// Fixed tab height: tall enough for the label row plus the hover wash's
/// breathing room, and the unit the strip's other controls center against.
/// Tabs run flush to the strip's bottom edge (browser-style), so this — plus
/// the strip's top padding — decides the whole strip's height.
const TAB_HEIGHT: f32 = 29.0;

/// Gap between the inactive hover wash's bottom edge and the strip floor —
/// what keeps the wash reading as floating rather than connected like the
/// active tab.
const WASH_BOTTOM_GAP: f32 = 3.0;

/// Gap between neighboring tabs, and between the final tab and the add button.
const TAB_GAP: f32 = 3.0;

/// Label padding inside a tab's fill / hover wash.
const LABEL_PAD_X: f32 = 8.0;

/// The close button's square, which also holds the dirty dot.
const CLOSE_SIDE: f32 = 18.0;

/// Horizontal travel before a press on a tab becomes a drag — the revision
/// list's threshold, so a click that wobbles stays a click in both places.
/// Vertical travel never counts: the strip only reorders along its own axis.
const DRAG_THRESHOLD: f32 = 6.0;

/// How far past a neighbor's center the dragged tab's leading edge has to go
/// before the two swap. Without it the swap point going right and the one
/// coming back sit a `TAB_GAP` apart, and a hand resting there flips the pair
/// on every tremor.
const SWAP_HYSTERESIS: f32 = 4.0;

/// Time constant of the slide that carries a displaced (or dropped) tab to
/// its slot: each `SLIDE_TAU` closes ~63% of the remaining distance, so a
/// slide is visually done in about three of them.
const SLIDE_TAU: f32 = 0.05;

/// The open-repo card, sized and inset like a palette column.
const OPEN_DIALOG_WIDTH: f32 = 520.0;
/// Height of the card's input band.
const OPEN_INPUT_BAND: f32 = 52.0;
/// Shared left edge of the input text, the section label and row glyphs.
const OPEN_CONTENT_X: f32 = theme::space::XL;
/// Inset of the recent list's hover washes inside the card.
const OPEN_LIST_INSET: f32 = theme::space::XS;
const OPEN_SECTION_HEIGHT: f32 = 28.0;
const OPEN_SECTION_PILL: f32 = 22.0;
const OPEN_SECTION_PILL_PAD_X: f32 = theme::space::SM;
const OPEN_ROW_HEIGHT: f32 = 34.0;
/// Rows the card's list shows before it scrolls: as many as the folder
/// suggestions can be, so typing a path and clearing it again keeps the card
/// the same size once the recents fill it.
const OPEN_LIST_ROWS: usize = crate::path_browse::MAX_SUGGESTIONS;
const OPEN_GLYPH_BOX: f32 = 16.0;
const OPEN_GLYPH_GAP: f32 = 10.0;
const OPEN_REMOVE_SIDE: f32 = 24.0;

/// The strip's height: the title-bar height where it stands in for one,
/// otherwise its top padding plus the tabs.
pub fn height() -> f32 {
    chrome::title_bar_height().unwrap_or(STRIP_TOP_PAD + TAB_HEIGHT)
}

/// Top padding of the strip where it sizes to its own content.
const STRIP_TOP_PAD: f32 = 6.0;

/// The slot a dragged tab belongs in, given `widths` in strip order, the
/// dragged tab's current `index` with its slot starting at `slot`, and the
/// position `x` its left edge has been dragged to.
///
/// It trades places with a neighbor once its leading edge is
/// [`SWAP_HYSTERESIS`] past that neighbor's center, however wide either tab
/// is — center against center would have a wide tab cover a narrow one
/// completely before they swapped. A fast drag can cross several neighbors
/// between two events, so this walks until the next one holds.
fn drop_index(widths: &[f32], index: usize, slot: f32, x: f32) -> usize {
    let mut widths = widths.to_vec();
    let (mut index, mut slot) = (index, slot);
    // Rightward: its right edge against the next tab's center.
    while let Some(next) = widths.get(index + 1).copied() {
        let center = slot + widths[index] + TAB_GAP + next / 2.0;
        if x + widths[index] <= center + SWAP_HYSTERESIS {
            break;
        }
        slot += next + TAB_GAP;
        widths.swap(index, index + 1);
        index += 1;
    }
    // Leftward: its left edge against the previous tab's center.
    while index > 0 {
        let previous = widths[index - 1];
        let center = slot - TAB_GAP - previous / 2.0;
        if x >= center - SWAP_HYSTERESIS {
            break;
        }
        slot -= previous + TAB_GAP;
        widths.swap(index, index - 1);
        index -= 1;
    }
    index
}

/// The square outside one of the active tab's bottom corners that its concave
/// join to the toolbar band fills.
fn join_bounds(tab: Rectangle, left: bool) -> Rectangle {
    Rectangle {
        x: if left {
            tab.x - radius::BUTTON
        } else {
            tab.x + tab.width
        },
        y: tab.y + tab.height - radius::BUTTON,
        width: radius::BUTTON,
        height: radius::BUTTON,
    }
}

/// The tab's hover label: where it points, and what the ring means when it
/// shows.
fn tab_tooltip(tab: &crate::Tab) -> String {
    let place = match &tab.source {
        crate::TabSource::Repo { root, .. } => crate::contract_user_path(&root.to_string_lossy()),
        crate::TabSource::GitHubPr(_) => format!("{}/{}", tab.owner, tab.name),
    };
    if tab_is_dirty(tab) {
        format!("{place}\nWorking copy (@) has changes")
    } else {
        place
    }
}

/// True when the repo behind `tab` has a non-empty working copy. Best-effort:
/// the working copy's emptiness may still be unresolved (it resolves lazily),
/// in which case we draw no dot rather than guess.
fn tab_is_dirty(tab: &crate::Tab) -> bool {
    // PR tabs have no working copy; their synthetic "All changes" row would
    // otherwise read as permanently dirty.
    if matches!(tab.source, crate::TabSource::GitHubPr(_)) {
        return false;
    }
    tab.state
        .session
        .commits
        .working_copy()
        .and_then(|wc| wc.is_empty())
        == Some(false)
}

/// Stub: the tab_bar view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_tab_bar(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::bar("tab_bar::build_tab_bar", 33.0)
}

/// Stub: the tab_bar view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_confirm_dialog(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::overlay(
        "tab_bar::build_confirm_dialog",
        ui.mode(ModeKind::Confirm).is_some(),
    )
}

/// Stub: the tab_bar view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_open_repo_dialog(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::overlay(
        "tab_bar::build_open_repo_dialog",
        ui.mode(ModeKind::OpenRepo).is_some() && !ui.tabs.is_empty(),
    )
}

/// Stub: the tab_bar view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn open_repo_card(
    ui: &Diffui,
    theme: ThemeSpec,
    dialog: &crate::OpenRepoDialog,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::panel("tab_bar::open_repo_card")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where tab `index`'s slot starts in a strip that begins at 0.
    fn slot(widths: &[f32], index: usize) -> f32 {
        widths[..index].iter().map(|width| width + TAB_GAP).sum()
    }

    #[test]
    fn a_tab_at_rest_belongs_where_it_is() {
        let widths = [120.0, 40.0, 90.0];
        for index in 0..widths.len() {
            let at_rest = slot(&widths, index);
            assert_eq!(drop_index(&widths, index, at_rest, at_rest), index);
        }
    }

    #[test]
    fn a_tab_swaps_once_its_leading_edge_clears_the_neighbors_center() {
        let widths = [100.0, 60.0, 80.0];
        // The neighbor spans 103..163, so its center is 133; the dragged tab's
        // right edge is `x + 100`.
        let swap_at = 133.0 + SWAP_HYSTERESIS - 100.0;
        assert_eq!(drop_index(&widths, 0, 0.0, swap_at), 0);
        assert_eq!(drop_index(&widths, 0, 0.0, swap_at + 1.0), 1);
    }

    /// The reason for the hysteresis: right after a swap the dragged tab must
    /// not already be on the far side of the way back, or a resting hand flips
    /// the pair on every tremor.
    #[test]
    fn a_swap_holds_until_the_tab_retreats_past_the_way_back() {
        // The strip after the swap above, the dragged tab now second.
        let widths = [60.0, 100.0, 80.0];
        let dragged_slot = slot(&widths, 1);
        let swapped_at = 133.0 + SWAP_HYSTERESIS - 100.0 + 1.0;
        assert_eq!(drop_index(&widths, 1, dragged_slot, swapped_at), 1);

        // The neighbor now spans 0..60: back across its center, less the margin.
        let back_at = 30.0 - SWAP_HYSTERESIS;
        assert_eq!(drop_index(&widths, 1, dragged_slot, back_at), 1);
        assert_eq!(drop_index(&widths, 1, dragged_slot, back_at - 1.0), 0);
        assert!(swapped_at - back_at > 2.0 * SWAP_HYSTERESIS);
    }

    #[test]
    fn a_fast_drag_crosses_several_tabs_in_one_step() {
        let widths = [50.0; 4];
        assert_eq!(drop_index(&widths, 0, 0.0, slot(&widths, 3)), 3);
        assert_eq!(drop_index(&widths, 3, slot(&widths, 3), 0.0), 0);
        assert_eq!(
            drop_index(&widths, 1, slot(&widths, 1), slot(&widths, 2) + 1.0),
            2
        );
    }

    /// The dragged tab is held inside the run of tabs, so both end slots have
    /// to be reachable from inside it — even for a wide tab next to a narrow one.
    #[test]
    fn both_ends_of_the_strip_are_reachable_within_the_clamp() {
        let widths = [200.0, 40.0, 40.0];
        let run: f32 = widths.iter().sum::<f32>() + 2.0 * TAB_GAP;
        assert_eq!(drop_index(&widths, 0, 0.0, run - 200.0), 2);

        let widths = [40.0, 40.0, 200.0];
        assert_eq!(drop_index(&widths, 2, slot(&widths, 2), 0.0), 0);
    }
}

//! Forges in the app: each tab's pull-request decorations (the badge on a
//! bookmark chip, the commit info block's field), when they refresh, and the
//! menu a badge opens. Creating, updating and stacking pull requests is
//! [`crate::forge_dialog`].
//!
//! A refresh asks the forge once for every local bookmark's pull request,
//! then for the checks of the open ones. Its answer carries the tab, the
//! repository and the refresh's generation, so it lands only on the tab that
//! asked, only if no newer refresh superseded it, and a bookmark that moved
//! meanwhile keeps its pull request but loses checks that were read for
//! another commit.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diffui_core::forge::{
    CheckState, Checks, ForgeError, ForgeKind, ForgeProvider, ForgeRemote, Forges, PrState,
    PullRequest,
};
use diffui_core::repo::CancelFlag;
use diffui_core::{BookmarksInfo, JobId, RepoId, RevisionSelection, Vcs};
use futures::StreamExt as _;

use crate::chip::Chip;
use crate::commands::{Availability, CommandArg, require};
use crate::geometry::Rectangle;
use crate::palette::ResultRef;
use crate::root::PaneSet;
use crate::theme::ThemeSpec;
use crate::{Action, Cx, Diffui, TabId, icons};

/// How often regaining the window's focus may refresh a tab's pull requests.
const FOCUS_REFRESH_EVERY: Duration = Duration::from_secs(60);

/// How many checks a refresh asks for at once.
const CHECKS_AT_ONCE: usize = 4;

/// The app's forges.
#[derive(Debug)]
pub(crate) struct ForgeUi {
    pub(crate) forges: Forges,
    /// Whether the window had the focus at the last reconcile: regaining it
    /// refreshes.
    was_focused: bool,
}

impl Default for ForgeUi {
    fn default() -> Self {
        Self {
            forges: Forges::builtin(),
            was_focused: true,
        }
    }
}

/// One tab's pull requests.
#[derive(Debug, Default)]
pub(crate) struct TabForge {
    /// The repository's `(name, url)` remotes, as last read.
    pub(crate) remotes: Vec<(String, String)>,
    /// The remote pull requests go to when nothing says otherwise, and its
    /// project. `None`: no forge hosts any remote.
    pub(crate) home: Option<(String, ForgeRemote)>,
    /// What each local bookmark's pull request is, by bookmark.
    pub(crate) decorations: HashMap<String, Decoration>,
    /// The graph load the last sync saw, and where each local bookmark
    /// pointed then.
    seen_graph: Option<JobId>,
    targets: HashMap<String, String>,
    synced: bool,
    /// A refresh is due at the next reconcile.
    pub(crate) wanted: bool,
    generation: u64,
    in_flight: Option<CancelFlag>,
    last_refresh: Option<Instant>,
    /// Why the last refresh failed, if it did.
    pub(crate) last_error: Option<ForgeError>,
    /// The create, update or stack submission under way.
    pub(crate) submission: Option<crate::forge_dialog::Submission>,
}

impl Drop for TabForge {
    /// A closed tab stops its refresh from asking the forge anything more.
    fn drop(&mut self) {
        if let Some(flag) = &self.in_flight {
            flag.cancel();
        }
    }
}

impl TabForge {
    /// The forge pull requests go to, if any.
    pub(crate) fn kind(&self) -> Option<ForgeKind> {
        self.home.as_ref().map(|(_, remote)| remote.kind)
    }

    /// The pull request of the local bookmark `name`.
    pub(crate) fn of(&self, name: &str) -> Option<&Decoration> {
        self.decorations.get(name)
    }
}

/// A local bookmark's pull request, as the last refresh read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decoration {
    pub(crate) remote: ForgeRemote,
    pub(crate) pr: PullRequest,
    /// The checks on its head, unless the bookmark moved since they were read.
    pub(crate) checks: Option<Checks>,
    /// The commit the bookmark pointed at when this was read.
    pub(crate) target: String,
}

impl Decoration {
    /// `#12`, `!12`.
    pub(crate) fn reference(&self) -> String {
        format!("{}{}", self.remote.kind.sigil(), self.pr.number)
    }

    /// The hover tip: `#12 Title · checks passing`.
    pub(crate) fn tip(&self) -> String {
        let mut tip = format!("{} {}", self.reference(), self.pr.title);
        if self.pr.state != PrState::Open {
            tip.push_str(&format!(" \u{b7} {}", self.pr.state.label()));
        }
        if let Some(checks) = self.checks.filter_state() {
            tip.push_str(&format!(" \u{b7} {}", checks.state.phrase()));
        }
        tip
    }
}

trait ChecksExt {
    /// The checks, when there were any to read.
    fn filter_state(&self) -> Option<&Checks>;
}

impl ChecksExt for Option<Checks> {
    fn filter_state(&self) -> Option<&Checks> {
        self.as_ref()
            .filter(|checks| checks.state != CheckState::None)
    }
}

/// The glyph and colour a pull request's state wears.
pub(crate) fn state_mark(state: PrState, theme: ThemeSpec) -> (&'static str, crate::theme::Color) {
    match state {
        PrState::Open => (icons::PULL_REQUEST, theme.added_text),
        PrState::Draft => (icons::PULL_REQUEST_DRAFT, theme.subtle_text),
        PrState::Merged => (icons::GIT_MERGE, theme.renamed),
        PrState::Closed => (icons::PULL_REQUEST_CLOSED, theme.removed_text),
    }
}

/// The glyph and colour checks wear, when there are any.
pub(crate) fn checks_mark(
    state: CheckState,
    theme: ThemeSpec,
) -> Option<(&'static str, crate::theme::Color)> {
    match state {
        CheckState::Pending => Some((icons::CIRCLE_DOT, theme.modified_token)),
        CheckState::Success => Some((icons::CHECK, theme.added_text)),
        CheckState::Failure => Some((icons::CLOSE, theme.removed_text)),
        CheckState::None => None,
    }
}

/// Give the bookmark chips of a row (built from `bookmarks`, in order) their
/// pull request's marks and tip.
pub(crate) fn decorate_chips(
    chips: &mut [Chip],
    bookmarks: &[String],
    forge: &TabForge,
    theme: ThemeSpec,
) {
    if forge.decorations.is_empty() {
        return;
    }
    for (chip, bookmark) in chips.iter_mut().zip(bookmarks) {
        let Some(decoration) = local_name(bookmark).and_then(|name| forge.of(name)) else {
            continue;
        };
        chip.marks.push(state_mark(decoration.pr.state, theme));
        if let Some(mark) = decoration
            .checks
            .filter_state()
            .and_then(|checks| checks_mark(checks.state, theme))
        {
            chip.marks.push(mark);
        }
        chip.tip = Some(decoration.tip());
    }
}

/// The local bookmark a graph label names: `main` and `main*` do, `main@origin`
/// (a remote copy), `ws@` (a workspace) and `main??` (conflicted) don't.
pub(crate) fn local_name(label: &str) -> Option<&str> {
    if label.ends_with('@') || label.ends_with("??") {
        return None;
    }
    let name = label.strip_suffix('*').unwrap_or(label);
    (!name.contains('@')).then_some(name)
}

/// Where each local bookmark points: the ones with one target.
pub(crate) fn bookmark_targets(info: &BookmarksInfo) -> HashMap<String, String> {
    info.bookmarks
        .iter()
        .filter_map(|entry| match entry.local_targets.as_slice() {
            [target] => Some((entry.name.clone(), target.clone())),
            _ => None,
        })
        .collect()
}

/// What one refresh read.
struct Refreshed {
    found: Vec<(ForgeRemote, PullRequest, Option<Checks>)>,
    error: Option<ForgeError>,
}

impl Diffui {
    /// Keep every tab's pull requests current: refresh once a repository's
    /// graph first loads, when its bookmarks move, when a hook asked for it,
    /// and on regaining the window's focus (at most once a minute). Runs on
    /// every reconcile; it only compares when a graph load landed.
    pub(crate) fn sync_forge(&mut self, cx: &mut Cx) {
        let regained = self.app_focused && !self.forge.was_focused;
        self.forge.was_focused = self.app_focused;
        let active = self.active_tab_id();
        let ids: Vec<TabId> = self.tabs.iter().map(|tab| tab.id).collect();
        for tab in ids {
            let Some(state) = self.tab_mut(tab) else {
                continue;
            };
            if !state
                .repository
                .as_ref()
                .is_some_and(|repo| repo.vcs == Vcs::Jj)
            {
                continue;
            }
            let graph = state.session.graph_version();
            if graph.is_some() && graph != state.forge.seen_graph {
                state.forge.seen_graph = graph;
                let targets = bookmark_targets(&state.session.bookmarks);
                if !state.forge.synced {
                    state.forge.synced = true;
                    state.forge.wanted = true;
                } else if targets != state.forge.targets {
                    let forge = &mut state.forge;
                    let before = forge.decorations.clone();
                    forge
                        .decorations
                        .retain(|name, _| targets.contains_key(name));
                    for (name, decoration) in &mut forge.decorations {
                        if targets.get(name) != Some(&decoration.target) {
                            decoration.checks = None;
                        }
                    }
                    if forge.decorations != before {
                        state.session.repaint_version += 1;
                    }
                    state.forge.wanted = true;
                }
                state.forge.targets = targets;
            }
            if regained
                && Some(tab) == active
                && state.forge.synced
                && state
                    .forge
                    .last_refresh
                    .is_none_or(|at| at.elapsed() >= FOCUS_REFRESH_EVERY)
            {
                state.forge.wanted = true;
            }
            self.advance_submission(tab, cx);
            if let Some(state) = self.tab_mut(tab)
                && std::mem::take(&mut state.forge.wanted)
            {
                self.refresh_pull_requests(tab, false, cx);
            }
        }
    }

    /// Ask the forge for the pull requests of `tab`'s local bookmarks, and
    /// the checks of the open ones. `manual` (the command) reports a failure;
    /// a refresh nobody asked for only remembers it.
    pub(crate) fn refresh_pull_requests(&mut self, tab: TabId, manual: bool, cx: &mut Cx) {
        let forges = self.forge.forges.clone();
        let Some(repo) = self
            .tabs
            .iter()
            .find(|candidate| candidate.id == tab)
            .and_then(|candidate| candidate.repo_id())
        else {
            return;
        };
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        let Some(root) = state
            .repository
            .as_ref()
            .filter(|repository| repository.vcs == Vcs::Jj)
            .map(|repository| repository.root.clone())
        else {
            return;
        };
        let remotes = diffui_core::forge::remotes(&root);
        let home = forges.pick_remote(&remotes, None);
        state.forge.remotes = remotes.clone();
        state.forge.home = home.clone();
        if home.is_none() {
            if !state.forge.decorations.is_empty() {
                state.forge.decorations.clear();
                state.session.repaint_version += 1;
            }
            state.forge.last_error = None;
            if manual {
                self.push_error_toast(
                    "No forge remote",
                    "None of this repository's remotes is on GitHub or GitLab.",
                );
            }
            return;
        }

        // One question per project, for all of its bookmarks.
        let mut asked: HashMap<String, String> = HashMap::new();
        let mut groups: Vec<(ForgeRemote, Vec<String>)> = Vec::new();
        for entry in &state.session.bookmarks.bookmarks {
            let [target] = entry.local_targets.as_slice() else {
                continue;
            };
            let Some((_, remote)) = forges.pick_remote(&remotes, entry.tracked_remote()) else {
                continue;
            };
            asked.insert(entry.name.clone(), target.clone());
            match groups.iter_mut().find(|(project, _)| *project == remote) {
                Some((_, names)) => names.push(entry.name.clone()),
                None => groups.push((remote, vec![entry.name.clone()])),
            }
        }
        let jobs: Vec<(Arc<dyn ForgeProvider>, ForgeRemote, Vec<String>)> = groups
            .into_iter()
            .filter_map(|(remote, names)| {
                forges
                    .provider(remote.kind)
                    .map(|provider| (provider, remote, names))
            })
            .collect();

        let forge = &mut state.forge;
        if let Some(previous) = forge.in_flight.take() {
            previous.cancel();
        }
        forge.generation += 1;
        let generation = forge.generation;
        let cancel = CancelFlag::default();
        forge.in_flight = Some(cancel.clone());
        forge.last_refresh = Some(Instant::now());
        Self::run_async_on(
            read_pull_requests(jobs, cancel),
            PaneSet::LEFT | PaneSet::MAIN,
            cx,
            move |this, refreshed, _| {
                this.apply_refresh(tab, &repo, generation, &asked, refreshed, manual)
            },
        );
    }

    /// Fold a refresh's answer into the tab that asked, if it is still open,
    /// still the same repository, and nothing newer superseded it.
    fn apply_refresh(
        &mut self,
        tab: TabId,
        repo: &RepoId,
        generation: u64,
        asked: &HashMap<String, String>,
        refreshed: Refreshed,
        manual: bool,
    ) {
        let Some(target) = self.tabs.iter_mut().find(|candidate| candidate.id == tab) else {
            return;
        };
        if target.repo_id().as_ref() != Some(repo) {
            return;
        }
        let state = &mut target.state;
        if state.forge.generation != generation {
            return;
        }
        state.forge.in_flight = None;
        let Refreshed { found, error } = refreshed;
        // Offline, or signed out: keep what the badges said.
        if error.is_none() || !found.is_empty() {
            let now = bookmark_targets(&state.session.bookmarks);
            let mut decorations = HashMap::new();
            for (remote, pr, checks) in found {
                let (Some(was), Some(is)) = (asked.get(&pr.head), now.get(&pr.head)) else {
                    continue;
                };
                decorations.insert(
                    pr.head.clone(),
                    Decoration {
                        remote,
                        checks: checks.filter(|_| was == is),
                        pr,
                        target: is.clone(),
                    },
                );
            }
            if decorations != state.forge.decorations {
                state.forge.decorations = decorations;
                state.session.repaint_version += 1;
            }
        }
        if let Some(error) = &error {
            tracing::info!("pull request refresh failed: {error}");
        }
        state.forge.last_error = error.clone();
        if manual && let Some(error) = error {
            let noun = state.forge.kind().unwrap_or(ForgeKind::GitHub).noun();
            self.push_error_toast(format!("Couldn't refresh {noun}s"), &error.to_string());
        }
    }

    /// A fetch finished: the forge may know more now.
    pub(crate) fn forge_after_fetch(&mut self, tab: TabId, ok: bool) {
        if ok && let Some(state) = self.tab_mut(tab) {
            state.forge.wanted = true;
        }
    }

    /// A mutation finished: a push refreshes the pull requests, and a
    /// submission waiting on this push goes on (or stops).
    pub(crate) fn forge_after_mutation(
        &mut self,
        tab: TabId,
        pending: &crate::PendingMutation,
        result: &Result<diffui_core::MutationOutcome, diffui_core::RepoError>,
    ) {
        let Some(state) = self.tab_mut(tab) else {
            return;
        };
        if result.is_ok() && matches!(pending.op, diffui_core::MutationOp::Push(_)) {
            state.forge.wanted = true;
        }
        self.submission_push_finished(tab, pending, result);
    }

    /// The pull request of the bookmark `arg` names, or of the first local
    /// bookmark on the revision it names (the selected one for `None`).
    pub(crate) fn decoration_for(&self, arg: &CommandArg) -> Option<&Decoration> {
        let forge = &self.active().forge;
        match &picked(self, arg) {
            CommandArg::Bookmark { name, .. } => forge.of(name),
            _ => self
                .bookmarks_on(arg)
                .iter()
                .find_map(|name| forge.of(name)),
        }
    }

    /// The local bookmarks on the revision `arg` names (the selected one for
    /// `None`), in graph order.
    pub(crate) fn bookmarks_on(&self, arg: &CommandArg) -> Vec<String> {
        let tab = self.active();
        let selection = match picked(self, arg) {
            CommandArg::Revision(selection) => selection,
            CommandArg::Bookmark { name, .. } => return vec![name],
            _ => tab.session.selected_revision.clone(),
        };
        tab.session
            .commits
            .find(&selection)
            .map(|row| {
                row.bookmarks()
                    .iter()
                    .filter_map(|label| local_name(label))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The badge's menu, at the chip a click landed on.
    pub(crate) fn open_pull_request_menu(
        &mut self,
        bookmark: String,
        anchor: Rectangle,
        cx: &mut Cx,
    ) {
        let selection = self.active().session.selected_revision.clone();
        let tree = self.pull_request_rows(&selection, Some(&bookmark));
        if tree.is_empty() {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            let _ = anchor;
            Self::popup_tree(&tree, None, cx);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cx;
            self.push_mode(crate::modes::Mode::Menu(crate::menu::OverlayMenu::new(
                tree,
                crate::menu::AnchorSpec::Below(anchor),
                Some(gpui_kit::MouseButton::Left),
            )));
        }
    }

    /// A click on a bookmark chip that has a badge: its row's bookmark at
    /// `index`, and the menu for its pull request.
    pub(crate) fn on_badge_click(
        &mut self,
        key: &crate::revision_list::RowSelectionKey,
        index: usize,
        anchor: Rectangle,
        cx: &mut Cx,
    ) {
        let selection = crate::selection_from_key(key);
        let label = self
            .active()
            .session
            .commits
            .find(&selection)
            .and_then(|row| row.bookmarks().get(index).cloned());
        if let Some(name) = label.as_deref().and_then(local_name) {
            self.open_pull_request_menu(name.to_owned(), anchor, cx);
        }
    }
}

/// Ask each project for its bookmarks' pull requests, then the checks of the
/// open ones, a few at a time. Stops asking once `cancel` is raised.
async fn read_pull_requests(
    jobs: Vec<(Arc<dyn ForgeProvider>, ForgeRemote, Vec<String>)>,
    cancel: CancelFlag,
) -> Refreshed {
    let mut found: Vec<(Arc<dyn ForgeProvider>, ForgeRemote, PullRequest)> = Vec::new();
    let mut error = None;
    for (provider, remote, names) in jobs {
        if cancel.is_cancelled() {
            return Refreshed {
                found: Vec::new(),
                error: Some(ForgeError::Cancelled),
            };
        }
        match provider.pull_requests(remote.clone(), names).await {
            Ok(prs) => found.extend(
                prs.into_iter()
                    .map(|pr| (provider.clone(), remote.clone(), pr)),
            ),
            Err(failure) => error = Some(failure),
        }
    }
    type Ask = std::pin::Pin<Box<dyn std::future::Future<Output = Option<Checks>> + Send>>;
    let asks: Vec<Ask> = found
        .iter()
        .map(|(provider, remote, pr)| {
            let sha = (pr.state.is_open() && !pr.head_sha.is_empty()).then(|| pr.head_sha.clone());
            let (provider, remote, cancel) = (provider.clone(), remote.clone(), cancel.clone());
            Box::pin(async move {
                let sha = sha.filter(|_| !cancel.is_cancelled())?;
                provider.checks(remote, sha).await.ok()
            }) as Ask
        })
        .collect();
    let checks: Vec<Option<Checks>> = futures::stream::iter(asks)
        .buffered(CHECKS_AT_ONCE)
        .collect()
        .await;
    Refreshed {
        found: found
            .into_iter()
            .zip(checks)
            .map(|((_, remote, pr), checks)| (remote, pr, checks))
            .collect(),
        error,
    }
}

// ── Commands ────────────────────────────────────────────────────────────────

/// Some remote is on a forge.
pub(crate) fn has_forge(ui: &Diffui, _arg: &CommandArg) -> Availability {
    require(
        ui.active().forge.home.is_some(),
        "No GitHub or GitLab remote",
    )
}

/// The bookmark (or the revision's) has a pull request.
pub(crate) fn has_pull_request(ui: &Diffui, arg: &CommandArg) -> Availability {
    has_forge(ui, arg)?;
    let none = match ui.active().forge.kind() {
        Some(ForgeKind::GitLab) => "No merge request",
        _ => "No pull request",
    };
    require(ui.decoration_for(arg).is_some(), none)
}

/// The bookmark (or the revision's) has a pull request that's still open.
pub(crate) fn has_open_pull_request(ui: &Diffui, arg: &CommandArg) -> Availability {
    has_pull_request(ui, arg)?;
    let closed = match ui.active().forge.kind() {
        Some(ForgeKind::GitLab) => "Merge request is closed",
        _ => "Pull request is closed",
    };
    require(
        ui.decoration_for(arg)
            .is_some_and(|decoration| decoration.pr.state.is_open()),
        closed,
    )
}

/// The pull request's page.
pub(crate) fn open_in_browser(ui: &Diffui, arg: CommandArg) -> Option<Action> {
    ui.decoration_for(&arg)
        .map(|decoration| Action::OpenUrl(decoration.pr.url.clone()))
}

/// The pull request in a tab of its own.
pub(crate) fn open_as_tab(ui: &Diffui, arg: CommandArg) -> Option<Action> {
    ui.decoration_for(&arg)
        .map(|decoration| Action::OpenRepo(decoration.pr.url.clone()))
}

pub(crate) fn copy_url(ui: &Diffui, arg: CommandArg) -> Option<Action> {
    ui.decoration_for(&arg)
        .map(|decoration| Action::Copy(decoration.pr.url.clone()))
}

/// What a bookmark row's `noun` says on this tab: `pull request`, or
/// `merge request` on GitLab.
pub(crate) fn noun(ui: &Diffui) -> &'static str {
    ui.active().forge.kind().unwrap_or(ForgeKind::GitHub).noun()
}

/// A forge command's label as the active tab's forge words it: `Create pull
/// request` says `Create merge request` on GitLab.
pub(crate) fn relabel(ui: &Diffui, label: &str) -> String {
    match ui.active().forge.kind() {
        Some(ForgeKind::GitLab) => label
            .replace("pull request", "merge request")
            .replace("Pull request", "Merge request"),
        _ => label.to_owned(),
    }
}

impl Diffui {
    /// The command palette's "Refresh pull requests".
    pub(crate) fn refresh_pull_requests_now(&mut self, cx: &mut Cx) {
        if let Some(tab) = self.active_tab_id() {
            self.refresh_pull_requests(tab, true, cx);
        }
    }
}

/// `arg` with a palette result read as the local bookmark or the revision it
/// names, so a forge command run on a picked result acts on that and not on
/// the selected row.
pub(crate) fn picked(ui: &Diffui, arg: &CommandArg) -> CommandArg {
    let CommandArg::Result(result) = arg else {
        return arg.clone();
    };
    if let ResultRef::Bookmark(label) = result
        && let Some(name) = crate::bookmarks::local_bookmark_label(label)
    {
        return CommandArg::Bookmark {
            name: name.to_owned(),
            remote: None,
            to: None,
        };
    }
    crate::commands::target_revision(ui, arg).map_or(CommandArg::None, CommandArg::Revision)
}

/// The selection a menu acts on, for the revision rows of the pull-request
/// menu.
pub(crate) fn selection_of(arg: &CommandArg, ui: &Diffui) -> RevisionSelection {
    match &picked(ui, arg) {
        CommandArg::Revision(selection) => selection.clone(),
        CommandArg::Bookmark { name, .. } => ui
            .active()
            .session
            .bookmarks
            .entry(name)
            .and_then(|entry| entry.local_target())
            .map(|target| RevisionSelection::Commit(target.to_owned()))
            .unwrap_or_else(|| ui.active().session.selected_revision.clone()),
        _ => ui.active().session.selected_revision.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::tests::{app, push_tab};
    use diffui_core::{BookmarkEntry, RemoteBookmarkRef};

    #[test]
    fn local_names_are_read_off_graph_labels() {
        assert_eq!(local_name("main"), Some("main"));
        assert_eq!(local_name("main*"), Some("main"));
        assert_eq!(local_name("main@origin"), None);
        assert_eq!(local_name("default@"), None);
        assert_eq!(local_name("main??"), None);
    }

    fn pr(head: &str, number: u64, state: PrState) -> PullRequest {
        PullRequest {
            number,
            url: format!("https://github.com/team/app/pull/{number}"),
            title: format!("PR {number}"),
            head: head.to_owned(),
            base: "main".to_owned(),
            head_sha: format!("sha{number}"),
            state,
            review: None,
        }
    }

    fn entry(name: &str, target: &str) -> BookmarkEntry {
        BookmarkEntry {
            name: name.to_owned(),
            local_targets: vec![target.to_owned()],
            remotes: vec![RemoteBookmarkRef {
                remote: "origin".to_owned(),
                target: target.to_owned(),
                tracked: true,
            }],
        }
    }

    fn remote() -> ForgeRemote {
        ForgeRemote::new(ForgeKind::GitHub, "github.com", "team", "app")
    }

    fn answer(prs: Vec<PullRequest>) -> Refreshed {
        Refreshed {
            found: prs
                .into_iter()
                .map(|pr| {
                    (
                        remote(),
                        pr,
                        Some(Checks {
                            state: CheckState::Success,
                            url: None,
                        }),
                    )
                })
                .collect(),
            error: None,
        }
    }

    /// A tab with bookmarks `feature` and `other`, as a refresh asked for
    /// them, and the generation that refresh had.
    fn asked(ui: &mut Diffui, tab: TabId) -> (RepoId, u64, HashMap<String, String>) {
        let state = ui.tab_mut(tab).unwrap();
        state.session.bookmarks.bookmarks = vec![entry("feature", "aaa"), entry("other", "bbb")];
        state.forge.generation += 1;
        let asked = bookmark_targets(&state.session.bookmarks);
        let repo = ui
            .tabs
            .iter()
            .find(|candidate| candidate.id == tab)
            .and_then(|candidate| candidate.repo_id())
            .unwrap();
        (repo, ui.tab(tab).unwrap().forge.generation, asked)
    }

    #[test]
    fn a_refresh_lands_on_the_tab_that_asked_even_after_a_switch() {
        let mut ui = app();
        let first = push_tab(&mut ui, "/tmp/forge-first");
        let second = push_tab(&mut ui, "/tmp/forge-second");
        let (repo, generation, asked) = asked(&mut ui, first);
        ui.active = ui.tabs.iter().position(|tab| tab.id == second).unwrap();
        ui.apply_refresh(
            first,
            &repo,
            generation,
            &asked,
            answer(vec![pr("feature", 3, PrState::Open)]),
            false,
        );
        let decoration = ui
            .tab(first)
            .unwrap()
            .forge
            .of("feature")
            .expect("decorated");
        assert_eq!(decoration.pr.number, 3);
        assert!(decoration.checks.is_some());
        assert!(ui.tab(second).unwrap().forge.decorations.is_empty());

        // A refresh for another repository under the same tab id (or a tab
        // that has gone) is dropped.
        let elsewhere = RepoId("/tmp/somewhere-else".to_owned());
        ui.apply_refresh(
            second,
            &elsewhere,
            1,
            &asked,
            answer(vec![pr("feature", 4, PrState::Open)]),
            false,
        );
        assert!(ui.tab(second).unwrap().forge.decorations.is_empty());
    }

    #[test]
    fn a_refresh_for_a_closed_tab_or_an_old_generation_is_dropped() {
        let mut ui = app();
        let tab = push_tab(&mut ui, "/tmp/forge-closed");
        let (repo, generation, asked) = asked(&mut ui, tab);
        ui.apply_refresh(
            tab,
            &repo,
            generation - 1,
            &asked,
            answer(vec![pr("feature", 3, PrState::Open)]),
            false,
        );
        assert!(
            ui.tab(tab).unwrap().forge.decorations.is_empty(),
            "superseded"
        );
        ui.tabs.retain(|candidate| candidate.id != tab);
        ui.apply_refresh(
            tab,
            &repo,
            generation,
            &asked,
            answer(vec![pr("feature", 3, PrState::Open)]),
            false,
        );
        assert!(ui.tab(tab).is_none());
    }

    #[test]
    fn a_bookmark_that_moved_keeps_its_pull_request_but_not_its_checks() {
        let mut ui = app();
        let tab = push_tab(&mut ui, "/tmp/forge-moved");
        let (repo, generation, asked) = asked(&mut ui, tab);
        // `feature` moves and `other` goes while the refresh is out.
        ui.tab_mut(tab).unwrap().session.bookmarks.bookmarks = vec![entry("feature", "ccc")];
        ui.apply_refresh(
            tab,
            &repo,
            generation,
            &asked,
            answer(vec![
                pr("feature", 3, PrState::Open),
                pr("other", 5, PrState::Merged),
            ]),
            false,
        );
        let forge = &ui.tab(tab).unwrap().forge;
        let feature = forge.of("feature").expect("still its pull request");
        assert_eq!(feature.target, "ccc");
        assert_eq!(feature.checks, None, "the checks were for the old commit");
        assert!(
            forge.of("other").is_none(),
            "a deleted bookmark has no badge"
        );
    }

    #[test]
    fn a_failed_refresh_keeps_the_badges() {
        let mut ui = app();
        let tab = push_tab(&mut ui, "/tmp/forge-offline");
        let (repo, generation, asked) = asked(&mut ui, tab);
        ui.apply_refresh(
            tab,
            &repo,
            generation,
            &asked,
            answer(vec![pr("feature", 3, PrState::Open)]),
            false,
        );
        ui.tab_mut(tab).unwrap().forge.generation += 1;
        ui.apply_refresh(
            tab,
            &repo,
            generation + 1,
            &asked,
            Refreshed {
                found: Vec::new(),
                error: Some(ForgeError::Unreachable {
                    host: "github.com".to_owned(),
                }),
            },
            false,
        );
        let forge = &ui.tab(tab).unwrap().forge;
        assert!(forge.of("feature").is_some());
        assert!(matches!(
            forge.last_error,
            Some(ForgeError::Unreachable { .. })
        ));
    }

    #[test]
    fn when_clauses_read_the_forge_and_the_bookmark_s_pull_request() {
        use crate::when::{AppKeys, When};
        let mut ui = app();
        let tab = push_tab(&mut ui, "/tmp/forge-when");
        ui.active = ui.tabs.iter().position(|state| state.id == tab).unwrap();
        let holds = |ui: &Diffui, clause: &str, name: &str| {
            let arg = CommandArg::Bookmark {
                name: name.to_owned(),
                remote: None,
                to: None,
            };
            When::parse(clause)
                .expect("parses")
                .eval(&AppKeys { ui, arg: &arg })
        };
        assert!(!holds(&ui, "repo.forge", "feature"), "no forge yet");
        assert!(!holds(&ui, "bookmark.pr", "feature"));
        let (repo, generation, asked) = asked(&mut ui, tab);
        ui.tab_mut(tab).unwrap().forge.home = Some(("origin".to_owned(), remote()));
        ui.apply_refresh(
            tab,
            &repo,
            generation,
            &asked,
            answer(vec![pr("feature", 3, PrState::Draft)]),
            false,
        );
        assert!(holds(&ui, "repo.forge == 'github'", "feature"));
        assert!(holds(&ui, "bookmark.pr == 'draft'", "feature"));
        assert!(holds(&ui, "bookmark.pr", "feature"));
        assert!(!holds(&ui, "bookmark.pr", "other"), "no pull request");
    }

    #[test]
    fn forge_commands_say_merge_request_on_gitlab() {
        let mut ui = app();
        let tab = push_tab(&mut ui, "/tmp/forge-noun");
        ui.active = ui.tabs.iter().position(|state| state.id == tab).unwrap();
        let label = |ui: &Diffui, id: &str| {
            ui.registry
                .get(id)
                .expect("registered")
                .palette_label_for(ui)
                .into_owned()
        };
        ui.tab_mut(tab).unwrap().forge.home = Some(("origin".to_owned(), remote()));
        assert_eq!(label(&ui, "forge.create"), "Create pull request");
        ui.tab_mut(tab).unwrap().forge.home = Some((
            "origin".to_owned(),
            ForgeRemote::new(ForgeKind::GitLab, "gitlab.com", "team", "app"),
        ));
        assert_eq!(label(&ui, "forge.create"), "Create merge request");
        assert_eq!(label(&ui, "forge.refresh"), "Refresh merge requests");
        assert_eq!(label(&ui, "forge.copy-url"), "Copy merge request URL");
        assert_eq!(
            ui.registry
                .get("forge.open")
                .unwrap()
                .enabled(&ui, &CommandArg::None),
            Err("No merge request".into())
        );
    }

    #[test]
    fn a_palette_bookmark_result_names_its_own_pull_request() {
        let mut ui = app();
        let tab = push_tab(&mut ui, "/tmp/forge-picked");
        ui.active = ui.tabs.iter().position(|state| state.id == tab).unwrap();
        let (repo, generation, asked) = asked(&mut ui, tab);
        ui.apply_refresh(
            tab,
            &repo,
            generation,
            &asked,
            answer(vec![
                pr("feature", 3, PrState::Open),
                pr("other", 5, PrState::Open),
            ]),
            false,
        );
        let result = |label: &str| CommandArg::Result(ResultRef::Bookmark(label.to_owned()));
        let number = |ui: &Diffui, arg| ui.decoration_for(&arg).map(|found| found.pr.number);
        assert_eq!(number(&ui, result("other*")), Some(5));
        assert_eq!(number(&ui, result("feature")), Some(3));
        assert_eq!(number(&ui, result("other@origin")), None, "a remote copy");
    }

    #[test]
    fn badges_go_on_local_bookmark_chips_only() {
        let ui = app();
        let theme = ui.resolved_theme().spec();
        let mut forge = TabForge::default();
        forge.decorations.insert(
            "feature".to_owned(),
            Decoration {
                remote: remote(),
                pr: pr("feature", 3, PrState::Draft),
                checks: Some(Checks {
                    state: CheckState::Failure,
                    url: None,
                }),
                target: "aaa".to_owned(),
            },
        );
        let look = ui.look(theme);
        let labels = vec!["feature*".to_owned(), "feature@origin".to_owned()];
        let mut chips = crate::sidebar::bookmark_chips_for(&labels, theme.accent, look);
        decorate_chips(&mut chips, &labels, &forge, theme);
        assert_eq!(chips[0].marks.len(), 2, "state and checks");
        assert_eq!(
            chips[0].tip.as_deref(),
            Some("#3 PR 3 \u{b7} draft \u{b7} checks failing")
        );
        assert!(chips[1].marks.is_empty() && chips[1].tip.is_none());
    }
}

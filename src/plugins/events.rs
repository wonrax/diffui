//! Telling plugins what happened: a repository opened, its bookmarks moved,
//! it reached another operation, the selection changed, a fetch or a push
//! finished, a command ran.
//!
//! Nothing here waits on a plugin. An event is handed to the scripts that
//! subscribed to it (their registration says which) and forgotten; what a
//! listener throws lands in that plugin's log. Each event carries the handle
//! of the tab it happened in, as that tab is when the event is sent.

use std::hash::{Hash as _, Hasher as _};
use std::time::Duration;

use diffui_core::{JobId, RevisionSelection};
use diffui_plugins::protocol::{Event, PluginCall, RepoHandle};

use super::State;
use crate::{Cx, Diffui, TabId};

/// How long the selection must rest before plugins hear it moved: walking
/// the log with the arrow keys is one change, not twenty.
const SELECTION_SETTLE: Duration = Duration::from_millis(150);

/// What a tab last told plugins.
#[derive(Debug, Default)]
pub(crate) struct TabPlugins {
    /// The graph load the last look saw.
    graph: Option<JobId>,
    opened: bool,
    /// Where every bookmark pointed then, local and remote, as a hash.
    refs: u64,
    /// The jj operation that load was at; empty in a git repository.
    op: String,
    selection: Option<(RevisionSelection, Vec<String>, usize)>,
    /// Fetches and pushes that finished. They are told once the repository
    /// has settled, so the handle they carry is at the operation that shows
    /// what they brought.
    due: Vec<Due>,
}

/// Something to tell: the tab it happened in, the event's name, and the
/// event for a handle.
type Told = (TabId, &'static str, fn(RepoHandle) -> Event);

#[derive(Debug)]
enum Due {
    Fetched,
    Pushed(Vec<String>),
}

fn refs_hash(bookmarks: &diffui_core::BookmarksInfo) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for entry in &bookmarks.bookmarks {
        entry.name.hash(&mut hasher);
        entry.local_targets.hash(&mut hasher);
        for remote in &entry.remotes {
            (&remote.remote, &remote.target, remote.tracked).hash(&mut hasher);
        }
    }
    hasher.finish()
}

impl Diffui {
    /// What must hold for plugins after any state change: the permission
    /// dialog is up when one is due, a prompt nobody can answer any more is
    /// dismissed, and what changed in each tab since the last look is told.
    /// Runs on every reconcile; it compares, and only hashes when a graph
    /// load landed.
    pub(crate) fn sync_plugins(&mut self, cx: &mut Cx) {
        self.raise_permission_dialog();
        self.sweep_plugin_prompt();

        let active = self.active_tab_id();
        let mut events: Vec<Told> = Vec::new();
        let mut pushed: Vec<(TabId, Vec<String>)> = Vec::new();
        let mut selection_moved = false;
        for tab in &mut self.tabs {
            let state = &mut tab.state;
            if state.repository.is_none() {
                continue;
            }
            let seen = &mut state.plugins;
            let graph = state.session.graph_version();
            if graph.is_some() && graph != seen.graph {
                seen.graph = graph;
                let refs = refs_hash(&state.session.bookmarks);
                if !seen.opened {
                    seen.opened = true;
                    seen.op = state
                        .session
                        .graph_operation()
                        .unwrap_or_default()
                        .to_owned();
                    events.push((tab.id, "repoOpened", |repo| Event::RepoOpened { repo }));
                } else if refs != seen.refs {
                    events.push((tab.id, "refsChanged", |repo| Event::RefsChanged { repo }));
                }
                seen.refs = refs;
            }
            // Looked at on every pass, not only when a graph lands: a load
            // can land at the operation the last one did (another revset),
            // and the operation moves without a load when a working-copy
            // edit is snapshotted and its row patched in place.
            let op = state.session.graph_operation().unwrap_or_default();
            if seen.opened && op != seen.op {
                seen.op = op.to_owned();
                events.push(
                    (tab.id, "operationChanged", |repo| Event::OperationChanged {
                        repo,
                    }),
                );
            }
            if !seen.due.is_empty() && !state.session.busy() {
                for due in seen.due.drain(..) {
                    match due {
                        Due::Fetched => {
                            events.push((tab.id, "fetched", |repo| Event::Fetched { repo }))
                        }
                        Due::Pushed(bookmarks) => pushed.push((tab.id, bookmarks)),
                    }
                }
            }
            if Some(tab.id) == active && seen.opened {
                let now = (
                    state.session.selected_revision.clone(),
                    state.revision_multi_selection.clone(),
                    state.selected_file,
                );
                if seen.selection.as_ref() != Some(&now) {
                    selection_moved = seen.selection.is_some();
                    seen.selection = Some(now);
                }
            }
        }
        for (tab, name, event) in events {
            self.emit(tab, None, name, &event);
        }
        for (tab, bookmarks) in pushed {
            self.emit(tab, None, "pushed", &|repo| Event::Pushed {
                repo,
                bookmarks: bookmarks.clone(),
            });
        }
        if selection_moved && self.anyone_hears("selectionChanged") {
            self.plugins
                .selection_changed
                .arm(SELECTION_SETTLE, cx, |this, _| {
                    if let Some(tab) = this.active_tab_id() {
                        this.emit(tab, None, "selectionChanged", &|repo| {
                            Event::SelectionChanged { repo }
                        });
                    }
                });
        }
    }

    /// A fetch finished on `tab`.
    pub(crate) fn plugins_after_fetch(&mut self, tab: TabId, ok: bool) {
        if ok && let Some(state) = self.tab_mut(tab) {
            state.plugins.due.push(Due::Fetched);
        }
    }

    /// A mutation finished on `tab`: a push names the bookmarks it pushed.
    pub(crate) fn plugins_after_mutation(
        &mut self,
        tab: TabId,
        pending: &crate::PendingMutation,
        result: &Result<diffui_core::MutationOutcome, diffui_core::RepoError>,
    ) {
        if let Ok(outcome) = result
            && pending.op.pushes()
            && let Some(state) = self.tab_mut(tab)
        {
            state
                .plugins
                .due
                .push(Due::Pushed(outcome.bookmarks.clone()));
        }
    }

    /// The command `id` ran on the active tab.
    pub(crate) fn plugins_after_command(&mut self, id: &str) {
        if !self.anyone_hears("commandInvoked") {
            return;
        }
        if let Some(tab) = self.active_tab_id() {
            self.emit(tab, None, "commandInvoked", &|repo| Event::CommandInvoked {
                repo,
                id: id.to_owned(),
            });
        }
    }

    /// A plugin loaded: the repositories already open are news to it.
    pub(super) fn tell_open_repositories(&mut self, plugin: &str) {
        let open: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|tab| tab.state.plugins.opened)
            .map(|tab| tab.id)
            .collect();
        for tab in open {
            self.emit(tab, Some(plugin), "repoOpened", &|repo| Event::RepoOpened {
                repo,
            });
        }
    }

    fn anyone_hears(&self, event: &str) -> bool {
        self.plugins
            .found
            .iter()
            .any(|found| hears(&found.state, event))
    }

    /// Send the event `name`, which `make` builds for `tab`, to the plugins
    /// that subscribed to it (`only`: to that one, if it did).
    fn emit(
        &mut self,
        tab: TabId,
        only: Option<&str>,
        name: &str,
        make: &dyn Fn(RepoHandle) -> Event,
    ) {
        let listeners: Vec<String> = self
            .plugins
            .found
            .iter()
            .filter(|found| only.is_none_or(|only| found.id == only) && hears(&found.state, name))
            .map(|found| found.id.clone())
            .collect();
        for id in listeners {
            // The handle is the plugin's own: it names that plugin's forge.
            let Some(repo) = self.repo_handle(tab, &id) else {
                return;
            };
            if let Some(plugin) = self.plugins.plugin(&id) {
                let event = make(repo);
                debug_assert_eq!(event.name(), name);
                plugin.call::<serde_json::Value>(PluginCall::Event { event }, |_| {});
            }
        }
    }
}

fn hears(state: &State, event: &str) -> bool {
    matches!(state, State::Running(registration) if registration.events.iter().any(|name| name == event))
}

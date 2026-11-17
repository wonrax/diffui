//! The app's hold on its hosts' connections.
//!
//! A link lives while something holds it. The app holds every one it starts,
//! so a connection that takes minutes to come up (a server being fetched or
//! built) isn't thrown away when the prompt that asked for it is closed, and
//! so there is one place that knows how each host stands:
//! [`Diffui::host_state`], which Settings and the open prompt show.
//!
//! A connection nothing has used for [`IDLE_LIMIT`] is hung up. One that
//! failed leaves what it ran and what that wrote ([`Diffui::host_transcript`])
//! until the host is tried again.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diffui_core::remote::{ConnectError, HostId, HostInfo, HostLink, LinkState, Transcript};
use tokio::sync::watch;

use crate::modes::ModeKind;
use crate::prose::Prose;
use crate::{Cx, Diffui};

/// How long a connection nothing uses is kept: long enough to close a tab
/// and open another on the same host without starting over.
const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);

/// The holds on a link that are the app's own: the registry's and its
/// watcher's.
const OWN_HOLDS: usize = 2;

/// A link the app holds.
pub(crate) struct Connection {
    pub(crate) link: HostLink,
    /// Since when nothing but the app has held it.
    idle_since: Option<Instant>,
    /// It came up, and that was noticed.
    was_up: bool,
}

/// How the last try at a host ended, when it failed.
pub(crate) struct Failure {
    pub(crate) error: ConnectError,
    pub(crate) transcript: Transcript,
}

/// The app's connections, and what became of the ones that failed.
#[derive(Default)]
pub(crate) struct Connections {
    pub(crate) live: BTreeMap<HostId, Connection>,
    pub(crate) failed: BTreeMap<HostId, Failure>,
}

/// How a host stands, for a row to show.
#[derive(Debug, Clone)]
pub(crate) enum HostState {
    Connecting {
        since: Instant,
        /// The last thing the program that reaches it wrote.
        said: Option<String>,
    },
    Up(Arc<HostInfo>),
    Failed(String),
}

impl HostState {
    /// "Connecting, 1m 12s": the word a row's tag carries.
    pub(crate) fn word(&self) -> String {
        match self {
            HostState::Connecting { since, .. } => {
                format!("Connecting, {}", elapsed(since.elapsed()))
            }
            HostState::Up(_) => "Connected".to_owned(),
            HostState::Failed(_) => "Failed".to_owned(),
        }
    }
}

/// Whole seconds: "8s", "1m 12s". A connection's wait isn't timed finer.
fn elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

impl Diffui {
    /// The link to `host`: the one up or coming up, else a new one. The app
    /// holds it from here on, whatever the caller does with its own.
    pub(crate) fn host_link(&mut self, host: &HostId, cx: &mut Cx) -> HostLink {
        let link = self.remote.hosts.link(host);
        let held = self
            .remote
            .connections
            .live
            .get(host)
            .is_some_and(|connection| connection.link.same(&link));
        if !held {
            self.remote.connections.failed.remove(host);
            self.remote.connections.live.insert(
                host.clone(),
                Connection {
                    link: link.clone(),
                    idle_since: None,
                    was_up: false,
                },
            );
            let state = link.state();
            self.watch_connection(link.clone(), state, cx);
        }
        link
    }

    /// Wake when `link`'s state moves, or after a moment: its wait is
    /// counted in seconds while it comes up, and an idle one is noticed.
    fn watch_connection(
        &mut self,
        link: HostLink,
        mut state: watch::Receiver<LinkState>,
        cx: &mut Cx,
    ) {
        Self::run_async(
            async move {
                let connecting = matches!(&*state.borrow(), LinkState::Connecting { .. });
                let tick = Duration::from_secs(if connecting { 1 } else { 15 });
                let _ = tokio::time::timeout(tick, state.changed()).await;
                (link, state)
            },
            cx,
            |this, (link, state), cx| this.on_connection_tick(link, state, cx),
        );
    }

    fn on_connection_tick(
        &mut self,
        link: HostLink,
        state: watch::Receiver<LinkState>,
        cx: &mut Cx,
    ) {
        let host = link.host().clone();
        let Some(connection) = self
            .remote
            .connections
            .live
            .get_mut(&host)
            .filter(|connection| connection.link.same(&link))
        else {
            // Hung up here, or another link took its place.
            return;
        };
        let now = state.borrow().clone();
        match now {
            LinkState::Connecting { .. } => {}
            LinkState::Up(_) => {
                let came_up = !std::mem::replace(&mut connection.was_up, true);
                let unused = link.holders() <= OWN_HOLDS;
                match (unused, connection.idle_since) {
                    (false, _) => connection.idle_since = None,
                    (true, None) => connection.idle_since = Some(Instant::now()),
                    (true, Some(since)) if since.elapsed() >= IDLE_LIMIT => {
                        tracing::info!(%host, "hanging up an idle host link");
                        self.remote.connections.live.remove(&host);
                        return;
                    }
                    (true, Some(_)) => {}
                }
                if came_up && !self.host_watched(&host) {
                    self.push_info_toast(
                        Prose::from("Connected to ").code(host.0.clone()),
                        "Pick it in the open dialog to open a repository there.",
                    );
                }
            }
            LinkState::Down(error) => {
                self.remote.connections.live.remove(&host);
                if !self.host_watched(&host) {
                    self.push_error_toast(
                        Prose::from("Couldn't reach ").code(host.0.clone()),
                        &error.to_string(),
                    );
                }
                self.remote.connections.failed.insert(
                    host,
                    Failure {
                        error,
                        transcript: link.transcript(),
                    },
                );
                return;
            }
        }
        self.watch_connection(link, state, cx);
    }

    /// Whether something on screen already says how `host` stands: its tab's
    /// strip, the open prompt pointed at it, or Settings.
    fn host_watched(&self, host: &HostId) -> bool {
        self.mode(ModeKind::Settings).is_some()
            || self
                .open_repo_dialog()
                .is_some_and(|dialog| dialog.host.as_ref() == Some(host))
            || self.tabs.iter().any(|tab| {
                tab.state
                    .remote
                    .as_ref()
                    .is_some_and(|remote| &remote.host == host)
            })
    }

    /// Hang up on `host` now. A tab on it finds its connection gone, and
    /// offers to reconnect.
    pub(crate) fn disconnect_host(&mut self, host: &HostId) {
        self.remote.connections.failed.remove(host);
        if let Some(connection) = self.remote.connections.live.remove(host) {
            connection.link.stop();
        }
    }

    /// Let go of `host`'s connection if nothing uses it, and of what its
    /// last failure left: its settings changed, so neither says anything
    /// about the next try.
    pub(crate) fn forget_host_connection(&mut self, host: &HostId) {
        self.remote.connections.failed.remove(host);
        let unused = self
            .remote
            .connections
            .live
            .get(host)
            .is_some_and(|connection| connection.link.holders() <= OWN_HOLDS);
        if unused {
            self.disconnect_host(host);
        }
    }

    /// How `host` stands: `None` when it hasn't been tried, or was let go.
    pub(crate) fn host_state(&self, host: &HostId) -> Option<HostState> {
        if let Some(connection) = self.remote.connections.live.get(host) {
            let link = &connection.link;
            return Some(match &*link.state().borrow() {
                LinkState::Connecting { detail } => HostState::Connecting {
                    since: link.started(),
                    said: detail.clone(),
                },
                LinkState::Up(info) => HostState::Up(info.clone()),
                LinkState::Down(error) => HostState::Failed(error.to_string()),
            });
        }
        let failure = self.remote.connections.failed.get(host)?;
        Some(HostState::Failed(failure.error.to_string()))
    }

    /// What was run to reach `host` and what it wrote: of the connection it
    /// has, else of the try that failed.
    pub(crate) fn host_transcript(&self, host: &HostId) -> Option<Transcript> {
        let transcript = match self.remote.connections.live.get(host) {
            Some(connection) => connection.link.transcript(),
            None => self.remote.connections.failed.get(host)?.transcript.clone(),
        };
        (transcript.command.is_some() || !transcript.output.is_empty()).then_some(transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_reads_in_whole_seconds() {
        assert_eq!(elapsed(Duration::from_millis(900)), "0s");
        assert_eq!(elapsed(Duration::from_secs(42)), "42s");
        assert_eq!(elapsed(Duration::from_secs(72)), "1m 12s");
    }
}

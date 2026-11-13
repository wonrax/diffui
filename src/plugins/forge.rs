//! A plugin's forge behind [`ForgeProvider`], so the badges, the dialog and
//! stack submission treat it like the built-in ones.
//!
//! The provider is built when the plugin has loaded, from what its script
//! registered: a method it left out answers [`ForgeError::Unsupported`], and
//! [`ForgeProvider::can`] says so up front. Script forges go after the
//! built-in providers (see [`Forges::with_scripts`]), so a plugin answers for
//! the hosts its manifest names only where no built-in forge does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use diffui_core::forge::{
    CheckState, Checks, CreatePr, ForgeAbility, ForgeError, ForgeFuture, ForgeKind, ForgeProvider,
    ForgeRemote, PrState, PullRequest, ReviewDecision, UpdatePr, parse_remote_url,
};
use diffui_plugins::protocol::{self, ForgeCall, PluginCall, RepoHandle};
use diffui_plugins::{ForgeTerm, HostPattern, Plugin, PluginError};
use serde::de::DeserializeOwned;

use super::State;
use crate::{Diffui, TabId};

/// The repository handle a forge call carries, by the project's page. The
/// provider is called off the UI thread with a remote and nothing else, so
/// the app leaves the handle of the tab it asks for here first.
pub(super) type Handles = Arc<Mutex<HashMap<String, RepoHandle>>>;

pub(super) struct ScriptForge {
    plugin: Arc<Plugin>,
    kind: ForgeKind,
    hosts: Vec<HostPattern>,
    /// The provider methods the script registered.
    methods: Vec<String>,
    handles: Handles,
}

impl ScriptForge {
    fn has(&self, method: &str) -> bool {
        self.methods.iter().any(|registered| registered == method)
    }

    /// Ask the script's provider, as a future the trait can return.
    fn ask<T: DeserializeOwned + Send + 'static>(
        &self,
        remote: &ForgeRemote,
        call: ForgeCall,
    ) -> ForgeFuture<'_, T> {
        if !self.has(call.method()) {
            return Box::pin(async { Err(ForgeError::Unsupported) });
        }
        let repo = RepoHandle {
            forge_remote: Some(to_script_remote(remote)),
            ..lock(&self.handles)
                .get(&remote.web_url)
                .cloned()
                // Nothing on screen shows this project: a handle that names
                // no repository, which every repository call refuses.
                .unwrap_or(RepoHandle {
                    id: String::new(),
                    op: String::new(),
                    root: String::new(),
                    forge_remote: None,
                })
        };
        let (answer, answered) = futures::channel::oneshot::channel();
        self.plugin
            .call::<T>(PluginCall::Forge { repo, call }, move |result| {
                let _ = answer.send(result);
            });
        let name = self.kind.name();
        let host = remote.host.clone();
        Box::pin(async move {
            match answered.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(failure(error, name, host)),
                Err(_) => Err(ForgeError::Cancelled),
            }
        })
    }
}

fn lock(handles: &Handles) -> std::sync::MutexGuard<'_, HashMap<String, RepoHandle>> {
    handles
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Why a provider call has no answer, as the app's forge errors.
fn failure(error: PluginError, name: &str, host: String) -> ForgeError {
    match error {
        PluginError::NotRegistered(_) => ForgeError::Unsupported,
        PluginError::TimedOut(_) => ForgeError::TimedOut { host, seconds: 60 },
        PluginError::Gone => ForgeError::Cancelled,
        // What the script threw is written for the user ("Forgejo 401: …").
        PluginError::Threw(thrown) => ForgeError::Failed(thrown.message),
        PluginError::Failed(why) => ForgeError::Failed(format!("The {name} plugin stopped: {why}")),
        PluginError::BadReply(why) => ForgeError::Failed(format!("The {name} plugin: {why}")),
    }
}

impl ForgeProvider for ScriptForge {
    fn kind(&self) -> ForgeKind {
        self.kind
    }

    fn can(&self, ability: ForgeAbility) -> bool {
        match ability {
            ForgeAbility::Create => self.has("createPullRequest"),
            ForgeAbility::Update => self.has("updatePullRequest"),
            ForgeAbility::Tab => false,
        }
    }

    fn claims(&self, remote_url: &str) -> Option<ForgeRemote> {
        claim(remote_url, self.kind, &self.hosts)
    }

    fn pull_requests(
        &self,
        remote: ForgeRemote,
        branches: Vec<String>,
    ) -> ForgeFuture<'_, Vec<PullRequest>> {
        let asked = self.ask::<Vec<protocol::PullRequest>>(
            &remote,
            ForgeCall::PullRequests {
                branches: branches.clone(),
            },
        );
        Box::pin(async move {
            // Only what was asked for: a script that returns every pull
            // request of the project badges nothing it shouldn't.
            Ok(asked
                .await?
                .into_iter()
                .filter(|pr| branches.contains(&pr.head))
                .map(pull_request)
                .collect())
        })
    }

    fn checks(&self, remote: ForgeRemote, sha: String) -> ForgeFuture<'_, Checks> {
        let asked = self.ask::<protocol::Checks>(&remote, ForgeCall::Checks { sha });
        Box::pin(async move {
            let checks = asked.await?;
            Ok(Checks {
                state: match checks.state {
                    protocol::CheckState::Pending => CheckState::Pending,
                    protocol::CheckState::Success => CheckState::Success,
                    protocol::CheckState::Failure => CheckState::Failure,
                    protocol::CheckState::None => CheckState::None,
                },
                url: checks.url,
            })
        })
    }

    fn create_pull_request(
        &self,
        remote: ForgeRemote,
        request: CreatePr,
    ) -> ForgeFuture<'_, PullRequest> {
        let asked = self.ask::<protocol::PullRequest>(
            &remote,
            ForgeCall::CreatePullRequest {
                request: protocol::CreatePullRequest {
                    head: request.head,
                    base: request.base,
                    title: request.title,
                    body: request.body,
                    draft: request.draft,
                },
            },
        );
        Box::pin(async move { asked.await.map(pull_request) })
    }

    fn update_pull_request(
        &self,
        remote: ForgeRemote,
        number: u64,
        patch: UpdatePr,
    ) -> ForgeFuture<'_, PullRequest> {
        let asked = self.ask::<protocol::PullRequest>(
            &remote,
            ForgeCall::UpdatePullRequest {
                number,
                patch: protocol::UpdatePullRequest {
                    title: patch.title,
                    body: patch.body,
                    base: patch.base,
                },
            },
        );
        Box::pin(async move { asked.await.map(pull_request) })
    }
}

fn pull_request(pr: protocol::PullRequest) -> PullRequest {
    PullRequest {
        number: pr.number,
        url: pr.url,
        title: pr.title,
        head: pr.head,
        base: pr.base,
        head_sha: pr.head_sha,
        state: match pr.state {
            protocol::PrState::Open => PrState::Open,
            protocol::PrState::Draft => PrState::Draft,
            protocol::PrState::Closed => PrState::Closed,
            protocol::PrState::Merged => PrState::Merged,
        },
        review: pr.review.map(|review| match review {
            protocol::Review::Approved => ReviewDecision::Approved,
            protocol::Review::ChangesRequested => ReviewDecision::ChangesRequested,
            protocol::Review::ReviewRequired => ReviewDecision::ReviewRequired,
        }),
    }
}

fn to_script_remote(remote: &ForgeRemote) -> protocol::ForgeRemote {
    protocol::ForgeRemote {
        host: remote.host.clone(),
        owner: remote.owner.clone(),
        name: remote.name.clone(),
        web_url: remote.web_url.clone(),
    }
}

/// `remote` as the plugin `id` sees it, when it is that plugin's forge that
/// claimed it.
pub(super) fn remote_of(remote: &ForgeRemote, id: &str) -> Option<protocol::ForgeRemote> {
    matches!(remote.kind, ForgeKind::Script(script) if script.id == id)
        .then(|| to_script_remote(remote))
}

/// The project `url` names, if its host is one of `hosts`.
///
/// An `http(s)://` remote names the web server itself, so its scheme and port
/// are the project page's. Any other remote (ssh) is taken to be served over
/// https on the default port.
fn claim(url: &str, kind: ForgeKind, hosts: &[HostPattern]) -> Option<ForgeRemote> {
    let (host, owner, name) = parse_remote_url(url)?;
    let (scheme, port) = ["https", "http"]
        .into_iter()
        .find_map(|scheme| {
            let rest = url.trim().strip_prefix(scheme)?.strip_prefix("://")?;
            let authority = rest.split('/').next()?.rsplit('@').next()?;
            let port = authority
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse::<u16>().ok());
            Some((scheme, port))
        })
        .unwrap_or(("https", None));
    let default = if scheme == "http" { 80 } else { 443 };
    if !hosts
        .iter()
        .any(|pattern| pattern.matches(&host, port.unwrap_or(default)))
    {
        return None;
    }
    let host = match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };
    Some(ForgeRemote {
        kind,
        web_url: format!("{scheme}://{host}/{owner}/{name}"),
        host,
        owner,
        name,
    })
}

impl Diffui {
    /// Put the forges of the plugins that are running (and registered a
    /// provider) behind the built-in ones, and have every tab look at its
    /// remotes again.
    pub(super) fn refresh_script_forges(&mut self) {
        let handles = self.plugins.forge_handles.clone();
        let scripts: Vec<Arc<dyn ForgeProvider>> = self
            .plugins
            .found
            .iter()
            .filter_map(|found| {
                let State::Running(registration) = &found.state else {
                    return None;
                };
                let manifest = found.manifest.as_ref()?;
                let claim = manifest.forge.as_ref()?;
                let provider = ScriptForge {
                    plugin: found.running.as_ref()?.plugin.clone(),
                    kind: ForgeKind::Script(diffui_core::forge::ScriptForge::new(
                        &manifest.id,
                        &manifest.name,
                        claim.term == ForgeTerm::MergeRequest,
                    )),
                    hosts: claim.hosts.clone(),
                    methods: registration.forge.clone()?,
                    handles: handles.clone(),
                };
                Some(Arc::new(provider) as Arc<dyn ForgeProvider>)
            })
            .collect();
        self.forge.forges = self.forge.forges.with_scripts(scripts);
        for tab in &mut self.tabs {
            tab.state.forge.wanted = true;
        }
    }

    /// `tab` is about to ask the forges about `remotes`: leave its handle for
    /// the plugins' forges among them.
    pub(crate) fn note_forge_handles(&mut self, tab: TabId, remotes: &[(String, String)]) {
        for (_, url) in remotes {
            let Some(remote) = self.forge.forges.claim(url) else {
                continue;
            };
            let ForgeKind::Script(script) = remote.kind else {
                continue;
            };
            if let Some(handle) = self.repo_handle(tab, script.id) {
                lock(&self.plugins.forge_handles).insert(remote.web_url, handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind() -> ForgeKind {
        ForgeKind::Script(diffui_core::forge::ScriptForge::new(
            "forgejo", "Forgejo", false,
        ))
    }

    fn hosts(patterns: &[&str]) -> Vec<HostPattern> {
        patterns
            .iter()
            .map(|pattern| HostPattern::parse(pattern).expect("a host"))
            .collect()
    }

    #[test]
    fn a_remote_is_claimed_by_host_and_keeps_its_web_address() {
        let codeberg = hosts(&["codeberg.org"]);
        let remote = claim("git@codeberg.org:team/app.git", kind(), &codeberg).unwrap();
        assert_eq!(remote.host, "codeberg.org");
        assert_eq!(
            (remote.owner.as_str(), remote.name.as_str()),
            ("team", "app")
        );
        assert_eq!(remote.web_url, "https://codeberg.org/team/app");
        assert_eq!(remote.kind.id(), "forgejo");
        assert!(claim("https://github.com/team/app", kind(), &codeberg).is_none());

        // A self-hosted instance on plain http and its own port.
        let local = hosts(&["127.0.0.1:3000"]);
        let remote = claim("http://127.0.0.1:3000/team/app.git", kind(), &local).unwrap();
        assert_eq!(remote.host, "127.0.0.1:3000");
        assert_eq!(remote.web_url, "http://127.0.0.1:3000/team/app");
        assert!(claim("http://127.0.0.1:4000/team/app.git", kind(), &local).is_none());
    }

    #[test]
    fn a_missing_method_is_unsupported_and_a_thrown_error_is_what_the_user_reads() {
        assert_eq!(
            failure(
                PluginError::NotRegistered("createPullRequest".into()),
                "Forgejo",
                "codeberg.org".into()
            ),
            ForgeError::Unsupported
        );
        let thrown = PluginError::Threw(diffui_plugins::JsError {
            name: "Error".into(),
            message: "Forgejo 401: token required".into(),
            stack: String::new(),
        });
        assert_eq!(
            failure(thrown, "Forgejo", "codeberg.org".into()).to_string(),
            "Forgejo 401: token required"
        );
    }
}

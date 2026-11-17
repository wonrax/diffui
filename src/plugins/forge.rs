//! A plugin's forge behind [`ForgeProvider`], for the badges, the dialog,
//! stack submission and the pull request tab.
//!
//! The provider exists from the moment the plugin starts loading, so remotes
//! are claimed from the manifest without waiting for the script (calls made
//! meanwhile wait for the load). Once loaded, a method the script left out
//! answers [`ForgeError::Unsupported`], and [`ForgeProvider::can`] says so
//! up front.
//!
//! A plugin that replaces a bundled one goes first, then the bundled ones,
//! then the rest: a remote goes to the first that claims it, so a plugin
//! from the plugins folder never takes a host a bundled forge claims.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use diffui_core::forge::{
    CheckState, Checks, CreatePr, DiffParts, ForgeAbility, ForgeError, ForgeFuture, ForgeKind,
    ForgePr, ForgeProvider, ForgeRemote, ForgeSpec, Forges, OnFile, PrCommit, PrInfo, PrState,
    PullRequest, ReviewComment, ReviewDecision, ReviewThread, UpdatePr, latest_per_branch,
    parse_remote_url,
};
use diffui_core::model::{DiffDocument, DiffFile, DiffFileStatus, RevisionDetails, SignatureInfo};
use diffui_core::repo::CancelFlag;
use diffui_plugins::protocol::{self, ForgeCall, PluginCall, RepoHandle, TAB_METHODS};
use diffui_plugins::{ForgeTerm, HostPattern, Manifest, Plugin, PluginError, Reply};
use serde::de::DeserializeOwned;

use super::State;
use crate::{Diffui, TabId};

/// How long a forge call may go without an answer or, streaming, without a
/// part: the runtime's limit, said in errors.
const CALL_SECONDS: u64 = 60;

/// The repository handle a forge call carries, by the project's page. The
/// provider is called off the UI thread with a remote and nothing else, so
/// the app leaves the handle of the tab it asks for here first.
pub(super) type Handles = Arc<Mutex<HashMap<String, RepoHandle>>>;

pub(super) struct ScriptForge {
    plugin: Arc<Plugin>,
    kind: ForgeKind,
    hosts: Vec<HostPattern>,
    /// The provider methods the script registered; `None` while it loads,
    /// when every method is assumed (a call then waits for the load).
    methods: Option<Vec<String>>,
    handles: Handles,
}

/// What the app says about `manifest`'s forge.
#[cfg(test)]
impl ScriptForge {
    pub(crate) fn plugin(&self) -> &Arc<Plugin> {
        &self.plugin
    }
}

pub(super) fn spec_of(manifest: &Manifest) -> Option<ForgeSpec> {
    let claim = manifest.forge.as_ref()?;
    Some(ForgeSpec {
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        merge_requests: claim.term == ForgeTerm::MergeRequest,
        sigil: claim.sigil,
        short: claim.short.clone(),
        cli: claim.cli.clone().unwrap_or_default(),
        default_host: claim.default_host.clone().unwrap_or_default(),
        host_env: claim.host_env.clone(),
        pull_request_path: claim.pull_request_path.clone(),
        subgroups: claim.subgroups,
        tokens: manifest.secret("token").is_some_and(|field| field.per_host),
    })
}

/// The forge the bundled plugin `id` provides, from the build's own copy.
#[cfg(test)]
pub(crate) fn bundled_kind(id: &str) -> ForgeKind {
    let plugin = super::bundled::built_in()
        .into_iter()
        .find(|plugin| plugin.id == id)
        .expect("a bundled plugin");
    let manifest =
        Manifest::bundled(&super::bundled::place(id), plugin.files).expect("its manifest reads");
    ForgeKind::new(spec_of(&manifest).expect("a forge"))
}

/// The handle a call about no repository carries: one every repository call
/// refuses, with `forgeRemote` set when there is a project to name.
pub(super) fn no_repository(forge_remote: Option<protocol::ForgeRemote>) -> RepoHandle {
    RepoHandle {
        id: String::new(),
        op: String::new(),
        root: String::new(),
        forge_remote,
    }
}

fn pr_ref(pr: &ForgePr) -> protocol::PrRef {
    let remote = pr.remote();
    protocol::PrRef {
        host: remote.host,
        owner: pr.owner.clone(),
        name: pr.name.clone(),
        number: pr.number,
        web_url: remote.web_url,
    }
}

impl ScriptForge {
    fn has(&self, method: &str) -> bool {
        self.methods
            .as_ref()
            .is_none_or(|methods| methods.iter().any(|registered| registered == method))
    }

    /// Ask the script something about a pull request, not a repository.
    fn ask_about<T: DeserializeOwned + Send + 'static>(
        &self,
        pr: &ForgePr,
        call: ForgeCall,
    ) -> ForgeFuture<'_, T> {
        if !self.has(call.method()) {
            return Box::pin(async { Err(ForgeError::Unsupported) });
        }
        let (answer, answered) = futures::channel::oneshot::channel();
        let repo = no_repository(Some(to_script_remote(&pr.remote())));
        self.plugin
            .call::<T>(PluginCall::Forge { repo, call }, move |result| {
                let _ = answer.send(result);
            });
        let (kind, host, what) = (self.kind, pr.host_label(), pr.reference());
        Box::pin(async move {
            match answered.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(failure(error, kind, host, Some(what))),
                Err(_) => Err(ForgeError::Cancelled),
            }
        })
    }

    /// Ask for a diff that streams: each part is parsed as it arrives and
    /// every whole file goes to `on_file`; the part is acknowledged once
    /// parsed, which is what lets the script send the next. A raised
    /// `cancel` aborts the call, which aborts the script's signal and the
    /// program or download behind it.
    async fn stream(
        &self,
        pr: &ForgePr,
        call: ForgeCall,
        cancel: &CancelFlag,
        on_file: &mut (dyn FnMut(DiffFile) + Send),
    ) -> Result<serde_json::Value, ForgeError> {
        use futures::{FutureExt as _, StreamExt as _};
        if !self.has(call.method()) {
            return Err(ForgeError::Unsupported);
        }
        let (part_to, mut parts) =
            futures::channel::mpsc::unbounded::<(Vec<serde_json::Value>, Reply)>();
        let (answer, answered) = futures::channel::oneshot::channel();
        let repo = no_repository(Some(to_script_remote(&pr.remote())));
        let id = self.plugin.call_streaming::<serde_json::Value>(
            PluginCall::Forge { repo, call },
            move |sent, reply| {
                // A host that stopped listening drops the reply, which
                // rejects the script's wait and ends its loop.
                let _ = part_to.unbounded_send((sent, reply));
            },
            move |result| {
                let _ = answer.send(result);
            },
        );
        let mut diff = DiffParts::default();
        let mut answered = answered.fuse();
        let failed =
            |error: PluginError| failure(error, self.kind, pr.host_label(), Some(pr.reference()));
        let result = loop {
            let tick = tokio::time::sleep(std::time::Duration::from_millis(25)).fuse();
            futures::pin_mut!(tick);
            futures::select! {
                sent = parts.next() => {
                    if let Some((sent, reply)) = sent {
                        match take_parts(&mut diff, sent, on_file) {
                            Ok(()) => reply.ok(serde_json::Value::Null),
                            Err(error) => reply.err(format!("not a diff part: {error}")),
                        }
                    }
                }
                result = answered => break result,
                () = tick => {
                    if cancel.is_cancelled() {
                        self.plugin.abort(id);
                        return Err(ForgeError::Cancelled);
                    }
                }
            }
        };
        // Every part was acknowledged before the script could settle.
        while let Ok((sent, reply)) = parts.try_recv() {
            let _ = take_parts(&mut diff, sent, on_file);
            reply.ok(serde_json::Value::Null);
        }
        diff.finish(on_file);
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(failed(error)),
            Err(_) => Err(ForgeError::Cancelled),
        }
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
                .unwrap_or_else(|| no_repository(None))
        };
        let (answer, answered) = futures::channel::oneshot::channel();
        self.plugin
            .call::<T>(PluginCall::Forge { repo, call }, move |result| {
                let _ = answer.send(result);
            });
        let kind = self.kind;
        let host = remote.host.clone();
        Box::pin(async move {
            match answered.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(failure(error, kind, host, None)),
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

/// A batch of a streamed diff's parts into `diff`, up to the first that
/// isn't one.
fn take_parts(
    diff: &mut DiffParts,
    sent: Vec<serde_json::Value>,
    on_file: &mut dyn FnMut(DiffFile),
) -> Result<(), serde_json::Error> {
    for part in sent {
        take_part(diff, serde_json::from_value(part)?, on_file);
    }
    Ok(())
}

/// One part of a streamed diff into `diff`.
fn take_part(diff: &mut DiffParts, part: protocol::DiffPart, on_file: &mut dyn FnMut(DiffFile)) {
    match part {
        protocol::DiffPart::Text(text) => diff.push_text(&text, on_file),
        protocol::DiffPart::File(file) => {
            let status = match file.status {
                protocol::FileStatus::Added => DiffFileStatus::Added,
                protocol::FileStatus::Deleted => DiffFileStatus::Deleted,
                // A copy shows its source path, as a rename does.
                protocol::FileStatus::Renamed | protocol::FileStatus::Copied => {
                    DiffFileStatus::Renamed
                }
                protocol::FileStatus::Modified => DiffFileStatus::Modified,
            };
            let counts = file
                .additions
                .zip(file.deletions)
                .map(|(added, deleted)| (added as usize, deleted as usize));
            diff.push_file(
                file.path,
                file.old_path,
                status,
                file.patch.as_deref(),
                counts,
                on_file,
            );
        }
    }
}

/// Why a provider call has no answer, as the app's forge errors. `host` is
/// the remote's or the pull request's; `what` names the pull request.
fn failure(error: PluginError, kind: ForgeKind, host: String, what: Option<String>) -> ForgeError {
    let name = kind.name();
    match error {
        PluginError::NotRegistered(_) => ForgeError::Unsupported,
        PluginError::TimedOut(_) => ForgeError::TimedOut {
            host,
            seconds: CALL_SECONDS,
        },
        PluginError::Gone => ForgeError::Cancelled,
        PluginError::Threw(thrown) => match thrown.forge {
            Some(forge) => typed(kind, *forge, thrown.message, host, what),
            // What the script threw is written for the user ("Forgejo 401: …").
            None => ForgeError::Failed(thrown.message),
        },
        PluginError::Failed(why) => ForgeError::Failed(format!("The {name} plugin stopped: {why}")),
        PluginError::BadReply(why) => ForgeError::Failed(format!("The {name} plugin: {why}")),
    }
}

/// A `ForgeError` the script threw, as the app's own. The app's words for
/// "install it" and "sign in" name the forge's CLI; a forge without one
/// keeps the script's message for those.
fn typed(
    kind: ForgeKind,
    thrown: diffui_plugins::ThrownForgeError,
    message: String,
    host: String,
    what: Option<String>,
) -> ForgeError {
    let host = thrown.host.unwrap_or(host);
    let own_words = kind.cli().is_empty() && !message.is_empty();
    match thrown.kind.as_str() {
        "missing-cli" | "auth" if own_words => ForgeError::Failed(message),
        "missing-cli" => ForgeError::MissingCli { kind },
        "auth" => ForgeError::NotAuthenticated { kind, host },
        "not-found" => ForgeError::NotFound {
            kind,
            host,
            what: thrown
                .what
                .or(what)
                .unwrap_or_else(|| "The project".to_owned()),
        },
        "rate-limited" => ForgeError::RateLimited { kind, host },
        "unreachable" => ForgeError::Unreachable { host },
        "timeout" => ForgeError::TimedOut {
            host,
            seconds: CALL_SECONDS,
        },
        _ if message.is_empty() => ForgeError::Failed(format!("{} failed", kind.name())),
        _ => ForgeError::Failed(message),
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
            ForgeAbility::Tab => TAB_METHODS.iter().all(|method| self.has(method)),
            ForgeAbility::ReviewComments => self.has("reviewComments"),
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
            // One per branch asked for: a script that returns every pull
            // request of the project badges nothing it shouldn't, and a
            // branch with several gets its open one, else its latest.
            Ok(latest_per_branch(
                asked.await?.into_iter().map(pull_request),
                &branches,
            ))
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

    fn view(&self, pr: ForgePr) -> ForgeFuture<'_, PrInfo> {
        let asked = self.ask_about::<protocol::PrInfo>(&pr, ForgeCall::View { pr: pr_ref(&pr) });
        Box::pin(async move {
            let info = asked.await?;
            Ok(PrInfo {
                number: info.number,
                title: info.title,
                author: info.author,
                state: state(info.state),
                base_ref: info.base,
                head_ref: info.head,
                totals: info
                    .additions
                    .zip(info.deletions)
                    .map(|(added, deleted)| (added as usize, deleted as usize)),
                changed_files: info.changed_files.unwrap_or(0) as usize,
                url: info.url,
                body: info.body.unwrap_or_default(),
            })
        })
    }

    fn commits(&self, pr: ForgePr) -> ForgeFuture<'_, Vec<PrCommit>> {
        let asked =
            self.ask_about::<Vec<protocol::PrCommit>>(&pr, ForgeCall::Commits { pr: pr_ref(&pr) });
        Box::pin(async move {
            Ok(asked
                .await?
                .into_iter()
                .map(|commit| PrCommit {
                    authored_millis: commit
                        .authored_at
                        .as_deref()
                        .and_then(diffui_core::history::parse_timestamp_millis),
                    counts: commit
                        .additions
                        .zip(commit.deletions)
                        .map(|(added, deleted)| (added as usize, deleted as usize)),
                    message: commit.message.unwrap_or_default(),
                    oid: commit.oid,
                    author: commit.author,
                    headline: commit.headline,
                })
                .collect())
        })
    }

    fn commit_diff(
        &self,
        pr: ForgePr,
        oid: String,
        cancel: CancelFlag,
    ) -> ForgeFuture<'_, (DiffDocument, Option<RevisionDetails>)> {
        Box::pin(async move {
            let call = ForgeCall::CommitDiff {
                pr: pr_ref(&pr),
                oid: oid.clone(),
            };
            let mut files = Vec::new();
            let value = self
                .stream(&pr, call, &cancel, &mut |file| files.push(file))
                .await?;
            let commit: protocol::CommitDiff = serde_json::from_value(value).map_err(|error| {
                ForgeError::Failed(format!(
                    "The {} plugin: commitDiff returned the wrong shape: {error}",
                    self.kind.name()
                ))
            })?;
            let (total_additions, total_deletions) = commit
                .additions
                .zip(commit.deletions)
                .map(|(added, deleted)| (added as usize, deleted as usize))
                .unwrap_or_else(|| {
                    (
                        files.iter().map(|file| file.additions).sum(),
                        files.iter().map(|file| file.deletions).sum(),
                    )
                });
            let details = commit.details.map(|details| RevisionDetails {
                commit_id: oid,
                change_id: None,
                bookmarks: Vec::new(),
                author: SignatureInfo {
                    name: details.author,
                    email: details.email,
                    timestamp: details.date,
                },
                committer: None,
                signature: None,
                description: details.message,
                parents: Vec::new(),
            });
            Ok((
                DiffDocument {
                    files,
                    total_additions,
                    total_deletions,
                    range: None,
                },
                details,
            ))
        })
    }

    fn stream_diff<'a>(
        &'a self,
        pr: ForgePr,
        cancel: CancelFlag,
        mut on_file: OnFile<'a>,
    ) -> ForgeFuture<'a, ()> {
        Box::pin(async move {
            let call = ForgeCall::Diff { pr: pr_ref(&pr) };
            self.stream(&pr, call, &cancel, &mut on_file)
                .await
                .map(|_| ())
        })
    }

    fn review_comments(&self, pr: ForgePr) -> ForgeFuture<'_, Vec<ReviewThread>> {
        let asked = self.ask_about::<Vec<protocol::ReviewThread>>(
            &pr,
            ForgeCall::ReviewComments { pr: pr_ref(&pr) },
        );
        Box::pin(async move { Ok(asked.await?.into_iter().map(review_thread).collect()) })
    }
}

fn review_thread(thread: protocol::ReviewThread) -> ReviewThread {
    let line = |line: Option<u64>| line.map(|line| line as usize);
    ReviewThread {
        path: thread.path,
        side: match thread.side {
            protocol::ReviewSide::Old => diffui_core::notes::Side::Old,
            protocol::ReviewSide::New => diffui_core::notes::Side::New,
        },
        line: line(thread.line),
        start_line: line(thread.start_line),
        original_line: line(thread.original_line),
        text: thread.text,
        outdated: thread.outdated,
        resolved: thread.resolved,
        url: thread.url,
        comments: thread
            .comments
            .into_iter()
            .map(|comment| ReviewComment {
                created_millis: comment
                    .created_at
                    .as_deref()
                    .and_then(diffui_core::history::parse_timestamp_millis),
                author: comment.author,
                body: comment.body,
            })
            .collect(),
    }
}

fn state(state: protocol::PrState) -> PrState {
    match state {
        protocol::PrState::Open => PrState::Open,
        protocol::PrState::Draft => PrState::Draft,
        protocol::PrState::Closed => PrState::Closed,
        protocol::PrState::Merged => PrState::Merged,
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
        state: state(pr.state),
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
    (remote.kind.id() == id).then(|| to_script_remote(remote))
}

/// The hosts the user added to a forge's claim: `[plugins.<id>] hosts = [...]`
/// in config.toml.
pub(super) fn user_hosts(config: Option<&toml::Table>) -> Vec<HostPattern> {
    config
        .and_then(|table| table.get(diffui_plugins::HOSTS))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|host| HostPattern::parse_claim(host.as_str()?).ok())
        .collect()
}

/// The project `url` names, if its host is one of `hosts` and its owner is
/// one segment on a forge without subgroups.
///
/// An `http(s)://` remote names the web server itself, so its scheme and port
/// are the project page's. Any other remote (ssh) is taken to be served over
/// https on the default port.
fn claim(url: &str, kind: ForgeKind, hosts: &[HostPattern]) -> Option<ForgeRemote> {
    let (host, owner, name) = parse_remote_url(url)?;
    if owner.contains('/') && !kind.subgroups {
        return None;
    }
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
    /// Make the forges of the plugins that are running or loading the app's,
    /// in claiming order, and have every tab look at its remotes again.
    pub(super) fn refresh_script_forges(&mut self) {
        // The same moments change which transports run.
        self.refresh_hosts();
        let handles = self.plugins.forge_handles.clone();
        let mut ranked: Vec<(u8, Arc<dyn ForgeProvider>)> = self
            .plugins
            .found
            .iter()
            .filter_map(|found| {
                let methods = match &found.state {
                    State::Running(registration) => Some(registration.forge.clone()?),
                    State::Loading => None,
                    _ => return None,
                };
                let manifest = found.manifest.as_ref()?;
                let mut hosts = manifest.forge.as_ref()?.hosts.clone();
                hosts.extend(found.extra_hosts.iter().cloned());
                hosts.extend(user_hosts(Some(&self.plugin_config(&found.id))));
                let plugin = found.running.as_ref()?.plugin.clone();
                // What `{forge.hosts}` lets it reach.
                plugin.set_forge_hosts(hosts.clone());
                let provider = ScriptForge {
                    plugin,
                    kind: ForgeKind::new(spec_of(manifest)?),
                    hosts,
                    methods,
                    handles: handles.clone(),
                };
                Some((
                    found.origin.rank(),
                    Arc::new(provider) as Arc<dyn ForgeProvider>,
                ))
            })
            .collect();
        ranked.sort_by_key(|(rank, _)| *rank);
        let scripts = ranked.into_iter().map(|(_, provider)| provider).collect();
        self.forge.forges = Forges::new(scripts);
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
            if let Some(handle) = self.repo_handle(tab, remote.kind.id()) {
                lock(&self.plugins.forge_handles).insert(remote.web_url, handle);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod gitlab;

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn kind() -> ForgeKind {
        kind_with_cli("")
    }

    fn kind_with_cli(cli: &str) -> ForgeKind {
        ForgeKind::new(ForgeSpec {
            id: "forgejo".into(),
            name: "Forgejo".into(),
            sigil: '#',
            short: "PR".into(),
            cli: cli.into(),
            ..ForgeSpec::default()
        })
    }

    fn thrown(message: &str, forge: Option<(&str, Option<&str>)>) -> PluginError {
        PluginError::Threw(diffui_plugins::JsError {
            name: "Error".into(),
            message: message.into(),
            stack: String::new(),
            forge: forge.map(|(kind, host)| {
                Box::new(diffui_plugins::ThrownForgeError {
                    kind: kind.into(),
                    host: host.map(str::to_owned),
                    what: None,
                })
            }),
            ai: None,
        })
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
        // An owner of several segments is a subgroup, which this forge has none of.
        assert!(claim("git@codeberg.org:team/sub/app.git", kind(), &codeberg).is_none());

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
                kind(),
                "codeberg.org".into(),
                None
            ),
            ForgeError::Unsupported
        );
        assert_eq!(
            failure(
                thrown("Forgejo 401: token required", None),
                kind(),
                "codeberg.org".into(),
                None
            )
            .to_string(),
            "Forgejo 401: token required"
        );
    }

    #[test]
    fn a_thrown_forge_error_is_worded_by_the_app() {
        let gh = kind_with_cli("gh");
        let host = || "github.acme.com".to_owned();
        assert_eq!(
            failure(thrown("", Some(("auth", None))), gh, host(), None),
            ForgeError::NotAuthenticated {
                kind: gh,
                host: host()
            }
        );
        assert!(
            failure(
                thrown("x", Some(("auth", Some("other.test")))),
                gh,
                host(),
                None
            )
            .to_string()
            .contains("gh auth login --hostname other.test")
        );
        assert_eq!(
            failure(
                thrown("gone", Some(("missing-cli", None))),
                gh,
                host(),
                None
            ),
            ForgeError::MissingCli { kind: gh }
        );
        assert_eq!(
            failure(
                thrown("", Some(("not-found", None))),
                gh,
                host(),
                Some("team/app#7".into())
            ),
            ForgeError::NotFound {
                kind: gh,
                host: host(),
                what: "team/app#7".into()
            }
        );
        assert!(matches!(
            failure(thrown("", Some(("rate-limited", None))), gh, host(), None),
            ForgeError::RateLimited { .. }
        ));
        // Without a CLI to name, the forge's own words explain signing in.
        assert_eq!(
            failure(
                thrown("Store a token with git credential", Some(("auth", None))),
                kind(),
                host(),
                None
            ),
            ForgeError::Failed("Store a token with git credential".into())
        );
    }

    /// The test forge plugin of `fixtures/plugins`, running as a forge would
    /// in the app, its program a copy of the fake CLI in a scratch directory
    /// (where the CLI notes the pids of endless diffs).
    #[cfg(unix)]
    pub(crate) struct TestForge {
        pub(crate) provider: ScriptForge,
        pub(crate) dir: std::path::PathBuf,
    }

    struct Quiet;

    impl diffui_plugins::Host for Quiet {
        fn call(&self, _plugin: &str, _call: protocol::HostCall, _reply: Reply) {}
        fn event(&self, _plugin: &str, _event: diffui_plugins::PluginEvent) {}
    }

    pub(crate) fn fixture_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/plugins")
    }

    /// A copy of the fake CLI, executable, in a fresh directory of its own.
    #[cfg(unix)]
    pub(crate) fn install_cli(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(dir).expect("make the CLI's directory");
        let program = dir.join("testforge");
        std::fs::copy(fixture_dir().join("testforge-cli.sh"), &program).expect("copy the CLI");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make the CLI executable");
        program
    }

    /// A directory of the system's temp dir for one test, empty. Not under
    /// the harness's home, which launches clear.
    pub(crate) fn scratch_dir(name: &str) -> std::path::PathBuf {
        crate::harness::without_forge_tokens();
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "diffui-testforge-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// `manifest`'s forge running as the app runs it, its `program` setting
    /// `program`, its data under `dir`.
    fn spawned(
        manifest: Manifest,
        program: &std::path::Path,
        dir: &std::path::Path,
    ) -> ScriptForge {
        let mut config = toml::Table::new();
        config.insert("program".into(), program.display().to_string().into());
        let plugin = Plugin::spawn(
            manifest.clone(),
            diffui_plugins::Options {
                config,
                data_dir: dir.join("data"),
                limits: Default::default(),
                secrets: None,
            },
            Arc::new(Quiet),
        );
        ScriptForge {
            plugin: Arc::new(plugin),
            kind: ForgeKind::new(spec_of(&manifest).expect("a forge")),
            hosts: manifest.forge.as_ref().unwrap().hosts.clone(),
            methods: None,
            handles: Handles::default(),
        }
    }

    /// The bundled plugin `id` with `config` as its `[plugins.<id>]`, its
    /// secrets in `secrets`, and `hosts` claimed (and reachable) beyond its
    /// manifest's: the token path against a local server.
    pub(crate) fn bundled_with(
        id: &str,
        config: toml::Table,
        secrets: Arc<dyn diffui_plugins::secrets::SecretStore>,
        hosts: &[&str],
        dir: &std::path::Path,
    ) -> ScriptForge {
        let plugin = super::super::bundled::built_in()
            .into_iter()
            .find(|plugin| plugin.id == id)
            .expect("bundled");
        let manifest = Manifest::bundled(&super::super::bundled::place(id), plugin.files)
            .expect("its manifest");
        let extra: Vec<HostPattern> = hosts
            .iter()
            .map(|host| HostPattern::parse_claim(host).expect("a host"))
            .collect();
        let plugin = Plugin::spawn(
            manifest.clone(),
            diffui_plugins::Options {
                config,
                data_dir: dir.join("data"),
                limits: Default::default(),
                secrets: Some(secrets),
            },
            Arc::new(Quiet),
        );
        plugin.set_forge_hosts(extra.clone());
        let mut claimed = manifest.forge.as_ref().unwrap().hosts.clone();
        claimed.extend(extra);
        ScriptForge {
            plugin: Arc::new(plugin),
            kind: ForgeKind::new(spec_of(&manifest).expect("a forge")),
            hosts: claimed,
            methods: None,
            handles: Handles::default(),
        }
    }

    /// The bundled GitHub plugin, from the build's own copy whatever a test
    /// put under `~/.bundled-test`, its `gh` being `program`.
    pub(crate) fn bundled_github(program: &std::path::Path, dir: &std::path::Path) -> ScriptForge {
        let github = super::super::bundled::built_in()
            .into_iter()
            .find(|plugin| plugin.id == "github")
            .expect("GitHub is bundled");
        let manifest = Manifest::bundled(&super::super::bundled::place("github"), github.files)
            .expect("GitHub's manifest");
        spawned(manifest, program, dir)
    }

    #[cfg(unix)]
    fn test_forge(name: &str) -> TestForge {
        let dir = scratch_dir(name);
        let program = install_cli(&dir);
        let manifest =
            Manifest::load(&fixture_dir().join("testforge")).expect("the fixture's manifest");
        TestForge {
            provider: spawned(manifest, &program, &dir),
            dir,
        }
    }

    #[cfg(unix)]
    fn pr(forge: &TestForge, number: u64) -> ForgePr {
        let mut pr = forge
            .provider
            .claims("git@testforge.invalid:team/app.git")
            .expect("claimed")
            .pull_request(number);
        pr.host = Some("testforge.invalid".into());
        pr
    }

    pub(crate) fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        crate::spawn::runtime().block_on(future)
    }

    #[test]
    #[cfg(unix)]
    fn a_plugins_pull_request_tab_reads_its_header_commits_and_commit_diffs() {
        let forge = test_forge("tab");
        let provider = &forge.provider;
        assert!(provider.can(ForgeAbility::Tab));
        let info = block_on(provider.view(pr(&forge, 1))).expect("the header");
        assert_eq!(
            (
                info.number,
                info.title.as_str(),
                info.state.label(),
                info.totals
            ),
            (1, "Stream it", "open", Some((3, 1)))
        );
        assert_eq!(info.url, "https://testforge.invalid/team/app/pull/1");
        let commits = block_on(provider.commits(pr(&forge, 1))).expect("the commits");
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].oid, "aaa111");
        assert!(commits[0].authored_millis.is_some());
        let (document, details) =
            block_on(provider.commit_diff(pr(&forge, 1), "aaa111".into(), CancelFlag::default()))
                .expect("a commit's diff");
        assert_eq!(document.files[0].path, "one.txt");
        assert_eq!((document.total_additions, document.total_deletions), (1, 0));
        let details = details.expect("its details");
        assert_eq!(details.description, "First\n\nIts body");
        assert_eq!(details.author.email, "ada@example.com");
    }

    #[test]
    #[cfg(unix)]
    fn a_plugins_diff_streams_file_by_file() {
        let forge = test_forge("stream");
        let started = std::time::Instant::now();
        let arrived: Mutex<Vec<(String, std::time::Duration)>> = Mutex::default();
        block_on(forge.provider.stream_diff(
            pr(&forge, 1),
            CancelFlag::default(),
            Box::new(|file| arrived.lock().unwrap().push((file.path, started.elapsed()))),
        ))
        .expect("the diff");
        let arrived = arrived.into_inner().unwrap();
        let paths: Vec<&str> = arrived.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt", "c.txt"]);
        // The CLI pauses between its pieces: the first file was handed on
        // while the rest was still on its way.
        let (first, last) = (arrived[0].1, arrived[2].1);
        assert!(
            last - first >= std::time::Duration::from_millis(150),
            "{first:?} then {last:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_cancel_stops_the_plugins_diff_and_its_program() {
        let forge = test_forge("cancel");
        let cancel = CancelFlag::default();
        let files = std::sync::atomic::AtomicUsize::new(0);
        let result = block_on(forge.provider.stream_diff(
            pr(&forge, 2),
            cancel.clone(),
            Box::new(|_| {
                if files.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 20 {
                    cancel.cancel();
                }
            }),
        ));
        assert_eq!(result, Err(ForgeError::Cancelled));
        let pid = std::fs::read_to_string(forge.dir.join("pids")).expect("the CLI ran");
        let alive = || {
            std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .status()
                .is_ok_and(|status| status.success())
        };
        let since = std::time::Instant::now();
        while alive() {
            assert!(
                since.elapsed().as_secs() < 20,
                "the CLI outlived the cancel"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_plugins_typed_errors_read_like_the_built_in_forges() {
        let forge = test_forge("errors");
        let error = block_on(forge.provider.view(pr(&forge, 3))).unwrap_err();
        assert_eq!(
            error,
            ForgeError::NotAuthenticated {
                kind: forge.provider.kind,
                host: "testforge.invalid".into()
            }
        );
        assert!(
            error
                .to_string()
                .contains("Run `testforge auth login --hostname testforge.invalid`"),
            "{error}"
        );
        let error = block_on(forge.provider.stream_diff(
            pr(&forge, 3),
            CancelFlag::default(),
            Box::new(|_| {}),
        ))
        .unwrap_err();
        assert!(
            matches!(error, ForgeError::NotAuthenticated { .. }),
            "{error:?}"
        );
        // A program that isn't there is the forge's CLI missing.
        std::fs::remove_file(forge.dir.join("testforge")).unwrap();
        let error = block_on(forge.provider.commits(pr(&forge, 1))).unwrap_err();
        assert_eq!(
            error,
            ForgeError::MissingCli {
                kind: forge.provider.kind
            }
        );
        assert!(
            error.to_string().contains("brew install testforge"),
            "{error}"
        );
    }

    /// Not a check: the numbers in the forge-platform report. One pull
    /// request's diff of `MEASURE_LINES` lines (50 000 by default) streamed
    /// through the bundled GitHub plugin (its `gh pr diff` path) and through
    /// the test forge plugin, both reading the same file from a stand-in
    /// program. Run each path (`MEASURE_PATH` = `github`, `plugin` or `none`
    /// for the baseline) in a process of its own under `/usr/bin/time -l`
    /// for its peak memory.
    #[test]
    #[ignore = "a measurement, run by hand"]
    #[cfg(unix)]
    fn measure_a_pull_requests_diff() {
        let lines: usize = std::env::var("MEASURE_LINES")
            .ok()
            .and_then(|lines| lines.parse().ok())
            .unwrap_or(50_000);
        let path = std::env::var("MEASURE_PATH").unwrap_or_else(|_| "plugin".into());
        let forge = test_forge("measure");
        let diff = forge.dir.join("big.diff");
        let made = std::process::Command::new(forge.dir.join("testforge"))
            .args(["diff", "team/app", &lines.to_string()])
            .output()
            .expect("make the diff");
        std::fs::write(&diff, &made.stdout).expect("write the diff");
        let program = forge.dir.join("cat-diff");
        std::fs::write(
            &program,
            format!("#!/bin/sh\nexec cat '{}'\n", diff.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A new program's first start pays for the system's check of it.
        std::process::Command::new(&program)
            .output()
            .expect("run it once");
        let mut config = toml::Table::new();
        config.insert("program".into(), program.display().to_string().into());
        let manifest = Manifest::load(&fixture_dir().join("testforge")).unwrap();
        let plugin = ScriptForge {
            plugin: Arc::new(Plugin::spawn(
                manifest.clone(),
                diffui_plugins::Options {
                    config,
                    data_dir: forge.dir.join("data"),
                    limits: Default::default(),
                    secrets: None,
                },
                Arc::new(Quiet),
            )),
            kind: forge.provider.kind,
            hosts: forge.provider.hosts.clone(),
            methods: None,
            handles: Handles::default(),
        };
        let github = bundled_github(&program, &forge.dir.join("github"));
        let provider: &dyn ForgeProvider = match path.as_str() {
            "github" => &github,
            "plugin" => &plugin,
            _ => return,
        };
        let spec = ForgePr {
            kind: provider.kind(),
            host: Some("testforge.invalid".into()),
            owner: "team".into(),
            name: "app".into(),
            number: 1,
        };
        let started = std::time::Instant::now();
        let first = Mutex::new(None);
        let counted = Mutex::new((0usize, 0usize));
        block_on(provider.stream_diff(
            spec,
            CancelFlag::default(),
            Box::new(|file| {
                first.lock().unwrap().get_or_insert(started.elapsed());
                let mut counted = counted.lock().unwrap();
                counted.0 += 1;
                counted.1 += file
                    .hunks
                    .iter()
                    .map(|hunk| hunk.lines.len())
                    .sum::<usize>();
            }),
        ))
        .expect("the diff");
        let total = started.elapsed();
        let (files, rows) = *counted.lock().unwrap();
        eprintln!(
            "MEASURE {path}: {lines} lines ({} bytes), {files} files, {rows} rows; first file {:?}, all {:?}",
            made.stdout.len(),
            first.lock().unwrap().unwrap(),
            total
        );
    }

    /// Not a check: what starting a bundled forge costs the thread that
    /// starts it (the UI thread, at launch) and how long until it has
    /// loaded, for the forge-platform report.
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn measure_starting_a_forge_plugin() {
        struct Loaded(Mutex<std::sync::mpsc::Sender<()>>);
        impl diffui_plugins::Host for Loaded {
            fn call(&self, _plugin: &str, _call: protocol::HostCall, _reply: Reply) {}
            fn event(&self, _plugin: &str, _event: diffui_plugins::PluginEvent) {
                let _ = self.0.lock().unwrap().send(());
            }
        }
        let fixture = fixture_dir().join("testforge");
        let files: std::collections::BTreeMap<String, String> = ["plugin.toml", "main.js"]
            .into_iter()
            .map(|name| {
                (
                    name.to_owned(),
                    std::fs::read_to_string(fixture.join(name)).unwrap(),
                )
            })
            .collect();
        let mut spawns = Vec::new();
        let mut loads = Vec::new();
        let mut plugins = Vec::new();
        for _ in 0..20 {
            let (tell, heard) = std::sync::mpsc::channel();
            let started = std::time::Instant::now();
            let manifest =
                Manifest::bundled(std::path::Path::new("(bundled)/testforge"), files.clone())
                    .unwrap();
            let plugin = Plugin::spawn(
                manifest,
                diffui_plugins::Options {
                    config: toml::Table::new(),
                    data_dir: std::env::temp_dir(),
                    limits: Default::default(),
                    secrets: None,
                },
                Arc::new(Loaded(Mutex::new(tell))),
            );
            spawns.push(started.elapsed());
            heard.recv().unwrap();
            loads.push(started.elapsed());
            plugins.push(plugin);
        }
        spawns.sort();
        loads.sort();
        eprintln!(
            "MEASURE start: manifest + spawn on the caller's thread median {:?} (max {:?}); loaded after median {:?} (max {:?})",
            spawns[10], spawns[19], loads[10], loads[19]
        );
    }
}

//! Forges: the hosts a repository's remotes live on (GitHub, GitLab), behind
//! one [`ForgeProvider`] trait.
//!
//! A provider is a data adapter. It answers "which pull requests do these
//! branches have", "how are the checks on this commit", "open or update this
//! pull request", and serves the pull-request tab's metadata, commits and
//! diff. The app draws the badges and dialogs and drives stacked pull
//! requests once, for every forge.
//!
//! Everything a provider takes and returns is owned data, and every call is a
//! boxed future, so a provider written in another language (the script
//! runtime's adapter) implements the same trait without reaching into app
//! state. Every forge is a plugin's: GitHub and GitLab are bundled ones
//! (`plugins/bundled`) that shell out to `gh` and `glab`, which keeps tokens
//! out of diffui.

mod remote;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use remote::{ForgePr, parse_remote_url, remotes};

use jj_lib::graph::GraphEdge;

use crate::diff_parse::DiffStreamParser;
use crate::graph::assign_lanes;
use crate::model::{
    CommitSummary, DiffDocument, DiffFile, DiffFileStatus, RevisionDetails, SignatureInfo,
    StreamRow,
};
use crate::repo::CancelFlag;

/// Which forge a remote or a pull request lives on: the spec of the plugin
/// that provides it. `Copy`, so the spec is interned for the life of the
/// process (one small allocation per distinct spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForgeKind(&'static ForgeSpec);

/// What a forge is called and how its references read: everything the app
/// says about a forge without asking it, from its plugin's manifest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ForgeSpec {
    /// What a when-clause's `repo.forge` reads: the plugin's id (`github`,
    /// `gitlab`).
    pub id: String,
    /// The forge's name, for messages.
    pub name: String,
    /// It says "merge request" rather than "pull request".
    pub merge_requests: bool,
    /// What prefixes a number in the forge's own references: `#12`, `!12`.
    pub sigil: char,
    /// The abbreviation: `PR`, `MR`.
    pub short: String,
    /// The program its errors tell the user to install and sign in with
    /// (`gh`); empty when it has none.
    pub cli: String,
    /// The host a reference without one means; empty when it has none.
    pub default_host: String,
    /// The environment variable that names another host for that.
    pub host_env: Option<String>,
    /// A pull request page's path after the host, with `{owner}`, `{name}`
    /// and `{number}`; `None` when the forge's links aren't known.
    pub pull_request_path: Option<String>,
    /// An owner may be several segments (GitLab's groups).
    pub subgroups: bool,
}

impl std::ops::Deref for ForgeKind {
    type Target = ForgeSpec;
    fn deref(&self) -> &ForgeSpec {
        self.0
    }
}

/// `spec` for the life of the process, the same allocation for the same spec.
fn intern(spec: ForgeSpec) -> &'static ForgeSpec {
    static INTERNED: std::sync::Mutex<Vec<&'static ForgeSpec>> = std::sync::Mutex::new(Vec::new());
    let mut interned = INTERNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(known) = interned.iter().find(|known| ***known == spec) {
        return known;
    }
    let leaked: &'static ForgeSpec = Box::leak(Box::new(spec));
    interned.push(leaked);
    leaked
}

impl ForgeKind {
    pub fn new(spec: ForgeSpec) -> Self {
        Self(intern(spec))
    }

    /// The forge's name, for messages.
    pub fn name(self) -> &'static str {
        &self.0.name
    }

    /// What a when-clause's `repo.forge` reads: the plugin's id.
    pub fn id(self) -> &'static str {
        &self.0.id
    }

    /// Whether the forge says "merge request" for a pull request.
    pub fn merge_requests(self) -> bool {
        self.0.merge_requests
    }

    /// What the forge calls a pull request, lowercase.
    pub fn noun(self) -> &'static str {
        if self.merge_requests() {
            "merge request"
        } else {
            "pull request"
        }
    }

    /// [`Self::noun`], sentence case.
    pub fn title_noun(self) -> &'static str {
        if self.merge_requests() {
            "Merge request"
        } else {
            "Pull request"
        }
    }

    /// The abbreviation: `PR`, `MR`.
    pub fn short(self) -> &'static str {
        &self.0.short
    }

    /// What prefixes a number in the forge's own references: `#12`, `!12`.
    pub fn sigil(self) -> char {
        self.0.sigil
    }

    /// The program the forge's errors name; empty when it has none.
    pub fn cli(self) -> &'static str {
        &self.0.cli
    }

    /// The host a reference without one means; empty when it has none.
    pub fn default_host(self) -> &'static str {
        &self.0.default_host
    }

    /// The environment variable that points the CLI at a host.
    fn host_env(self) -> Option<&'static str> {
        self.0.host_env.as_deref()
    }

    /// The host a reference without one resolves to: the CLI's environment
    /// override, else the forge's public host.
    pub fn implied_host(self) -> String {
        self.host_env()
            .and_then(|var| std::env::var(var).ok())
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| self.default_host().to_owned())
    }
}

/// A remote a provider claimed: where its project lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForgeRemote {
    pub kind: ForgeKind,
    /// Lowercase host name, without a port.
    pub host: String,
    /// The owner or namespace. A GitLab project in a subgroup has slashes
    /// here (`group/sub`).
    pub owner: String,
    pub name: String,
    /// The project's page: `https://host/owner/name`.
    pub web_url: String,
}

impl ForgeRemote {
    pub fn new(kind: ForgeKind, host: &str, owner: &str, name: &str) -> Self {
        let host = host.to_ascii_lowercase();
        Self {
            kind,
            web_url: format!("https://{host}/{owner}/{name}"),
            host,
            owner: owner.to_owned(),
            name: name.to_owned(),
        }
    }

    /// `owner/name`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The pull request `number` of this project, as a tab opens it.
    pub fn pull_request(&self, number: u64) -> ForgePr {
        ForgePr {
            kind: self.kind,
            host: Some(self.host.clone()).filter(|host| host != self.kind.default_host()),
            owner: self.owner.clone(),
            name: self.name.clone(),
            number,
        }
    }
}

/// Where a pull request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrState {
    Open,
    Draft,
    Closed,
    Merged,
}

impl PrState {
    /// Whether it still takes changes: open or draft.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Open | Self::Draft)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Draft => "draft",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }
}

/// The review verdict, where the forge has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// One pull (or merge) request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub title: String,
    /// The branch it merges, as named on the forge.
    pub head: String,
    /// The branch it merges into.
    pub base: String,
    /// The head commit's hex id, when the forge says.
    pub head_sha: String,
    pub state: PrState,
    pub review: Option<ReviewDecision>,
}

/// The overall state of a commit's checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckState {
    Pending,
    Success,
    Failure,
    /// No checks ran.
    None,
}

impl CheckState {
    pub fn phrase(self) -> &'static str {
        match self {
            Self::Pending => "checks running",
            Self::Success => "checks passing",
            Self::Failure => "checks failing",
            Self::None => "no checks",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checks {
    pub state: CheckState,
    /// Where the forge shows them.
    pub url: Option<String>,
}

/// What a new pull request says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePr {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
}

/// The fields an update changes; `None` leaves one alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdatePr {
    pub base: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
}

impl UpdatePr {
    pub fn is_empty(&self) -> bool {
        self.base.is_none() && self.title.is_none() && self.body.is_none()
    }
}

/// A pull request's header, for its tab.
#[derive(Debug, Clone, Default)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub author: String,
    /// The state, lowercase (`open`, `merged`, `closed`, `draft`).
    pub state: String,
    pub base_ref: String,
    pub head_ref: String,
    /// Lines added and removed, when the forge's header says.
    pub totals: Option<(usize, usize)>,
    pub changed_files: usize,
    pub url: String,
}

/// One commit of a pull request.
#[derive(Debug, Clone)]
pub struct PrCommit {
    pub oid: String,
    pub author: String,
    pub headline: String,
    /// The authored date, in epoch millis, when it parses.
    pub authored_millis: Option<i64>,
}

/// Why a forge call failed, in terms the UI can explain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeError {
    /// The provider's CLI isn't installed, or not on the PATH diffui sees.
    MissingCli {
        kind: ForgeKind,
    },
    /// The CLI isn't signed in to this host.
    NotAuthenticated {
        kind: ForgeKind,
        host: String,
    },
    /// The project or pull request isn't there, or the account can't see it.
    NotFound {
        kind: ForgeKind,
        host: String,
        what: String,
    },
    RateLimited {
        kind: ForgeKind,
        host: String,
    },
    Unreachable {
        host: String,
    },
    TimedOut {
        host: String,
        seconds: u64,
    },
    Cancelled,
    /// The provider doesn't do this.
    Unsupported,
    /// Anything else, with the forge's own words.
    Failed(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCli { kind } if kind.cli().is_empty() => {
                write!(f, "{}'s command-line tool wasn't found.", kind.name())
            }
            Self::MissingCli { kind } => {
                let cli = kind.cli();
                write!(
                    f,
                    "The {} CLI (`{cli}`) wasn't found. Install it (`brew install {cli}`) and \
                     run `{cli} auth login`. If it is installed: an app opened from Finder or \
                     the Dock only sees the system PATH (/usr/bin:/bin:/usr/sbin:/sbin), so \
                     {cli} in /opt/homebrew/bin or ~/.local/bin is invisible to it. Launch \
                     diffui from a terminal, or make {cli}'s directory part of the launchd PATH.",
                    kind.name()
                )
            }
            Self::NotAuthenticated { kind, host } if kind.cli().is_empty() => write!(
                f,
                "diffui isn't signed in to {} on {host}. Sign in, then try again.",
                kind.name()
            ),
            Self::NotAuthenticated { kind, host } => write!(
                f,
                "{} isn't signed in to {host}. Run `{} auth login --hostname {host}` in a \
                 terminal, then try again.",
                kind.cli(),
                kind.cli()
            ),
            Self::NotFound { kind, host, what } if kind.cli().is_empty() => write!(
                f,
                "{what} wasn't found on {host}, or the account diffui uses there can't see it."
            ),
            Self::NotFound { kind, host, what } => write!(
                f,
                "{what} wasn't found on {host}, or the account {} is signed in with can't see it.",
                kind.cli()
            ),
            Self::RateLimited { kind, host } => write!(
                f,
                "{}'s API rate limit on {host} was hit. Wait a few minutes and try again.",
                kind.name()
            ),
            Self::Unreachable { host } => write!(
                f,
                "Couldn't reach {host}. Check the network (and the VPN, for a company host)."
            ),
            Self::TimedOut { host, seconds } => write!(
                f,
                "{host} didn't answer within {seconds} seconds. Check the network (and the \
                 VPN, for a company host), then try again."
            ),
            Self::Cancelled => f.write_str("Cancelled."),
            Self::Unsupported => f.write_str("This forge doesn't support that."),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ForgeError {}

pub type ForgeResult<T> = Result<T, ForgeError>;

/// What every provider call returns: a boxed future, so the trait stays
/// object-safe.
pub type ForgeFuture<'a, T> = Pin<Box<dyn Future<Output = ForgeResult<T>> + Send + 'a>>;

/// Called once per file as a pull request's diff streams in.
pub type OnFile<'a> = Box<dyn FnMut(DiffFile) + Send + 'a>;

/// Something a provider may not be able to do: a plugin's forge does what its
/// script implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeAbility {
    /// [`ForgeProvider::create_pull_request`].
    Create,
    /// [`ForgeProvider::update_pull_request`].
    Update,
    /// A pull request in a tab of its own: `view`, `commits` and the diffs.
    Tab,
}

/// A forge, as diffui talks to it. See the module docs.
pub trait ForgeProvider: Send + Sync {
    fn kind(&self) -> ForgeKind;

    /// Whether this provider can do `ability`, so the app offers only what
    /// would work.
    fn can(&self, _ability: ForgeAbility) -> bool {
        true
    }

    /// The project `remote_url` names, if this forge hosts it.
    fn claims(&self, remote_url: &str) -> Option<ForgeRemote>;

    /// The pull requests whose head is one of `branches`, in `remote`'s own
    /// project (not forks): the open one for a branch when there is one, else
    /// its latest.
    fn pull_requests(
        &self,
        remote: ForgeRemote,
        branches: Vec<String>,
    ) -> ForgeFuture<'_, Vec<PullRequest>>;

    /// The overall state of the checks on commit `sha`.
    fn checks(&self, remote: ForgeRemote, sha: String) -> ForgeFuture<'_, Checks>;

    fn create_pull_request(
        &self,
        remote: ForgeRemote,
        request: CreatePr,
    ) -> ForgeFuture<'_, PullRequest>;

    fn update_pull_request(
        &self,
        remote: ForgeRemote,
        number: u64,
        patch: UpdatePr,
    ) -> ForgeFuture<'_, PullRequest>;

    /// The pull-request tab: its header.
    fn view(&self, _pr: ForgePr) -> ForgeFuture<'_, PrInfo> {
        Box::pin(async { Err(ForgeError::Unsupported) })
    }

    /// The pull-request tab: its commits, oldest first.
    fn commits(&self, _pr: ForgePr) -> ForgeFuture<'_, Vec<PrCommit>> {
        Box::pin(async { Err(ForgeError::Unsupported) })
    }

    /// The pull-request tab: one commit's diff and header.
    fn commit_diff(
        &self,
        _pr: ForgePr,
        _oid: String,
        _cancel: CancelFlag,
    ) -> ForgeFuture<'_, (DiffDocument, Option<RevisionDetails>)> {
        Box::pin(async { Err(ForgeError::Unsupported) })
    }

    /// The pull-request tab: the whole diff, file by file as it arrives.
    fn stream_diff<'a>(
        &'a self,
        _pr: ForgePr,
        _cancel: CancelFlag,
        _on_file: OnFile<'a>,
    ) -> ForgeFuture<'a, ()> {
        Box::pin(async { Err(ForgeError::Unsupported) })
    }
}

/// The forges the plugins provide, in the order they're asked to claim a
/// remote.
#[derive(Clone, Default)]
pub struct Forges {
    providers: Vec<Arc<dyn ForgeProvider>>,
}

impl fmt::Debug for Forges {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.providers.iter().map(|provider| provider.kind()))
            .finish()
    }
}

impl Forges {
    pub fn new(providers: Vec<Arc<dyn ForgeProvider>>) -> Self {
        Self { providers }
    }

    pub fn provider(&self, kind: ForgeKind) -> Option<Arc<dyn ForgeProvider>> {
        self.providers
            .iter()
            .find(|provider| provider.kind() == kind)
            .cloned()
    }

    /// The forges, in the order they're asked to claim a remote.
    pub fn kinds(&self) -> Vec<ForgeKind> {
        self.providers
            .iter()
            .map(|provider| provider.kind())
            .collect()
    }

    /// The pull request a pasted link or reference names, on one of these
    /// forges (see [`ForgePr::parse_with`]).
    pub fn parse_pr(&self, input: &str) -> Option<ForgePr> {
        let pr = ForgePr::parse_with(input, &self.kinds())?;
        // A link on a host another forge claims is that forge's, when its
        // links read the same way (a GitHub-like forge on a company host).
        let Some(host) = pr.host.as_deref() else {
            return Some(pr);
        };
        let claimed = self.claim(&format!("https://{host}/{}/{}", pr.owner, pr.name));
        match claimed {
            Some(remote) if remote.kind != pr.kind => {
                ForgePr::parse_with(input, &[remote.kind]).or(Some(pr))
            }
            _ => Some(pr),
        }
    }

    /// The forges' names for a sentence: "GitHub, GitLab or Forgejo".
    pub fn names(&self) -> String {
        let names: Vec<&str> = self.kinds().iter().map(|kind| kind.name()).collect();
        match names.split_last() {
            None => "a forge".to_owned(),
            Some((last, [])) => (*last).to_owned(),
            Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        }
    }

    /// Whether `kind`'s provider can do `ability`.
    pub fn can(&self, kind: ForgeKind, ability: ForgeAbility) -> bool {
        self.provider(kind)
            .is_some_and(|provider| provider.can(ability))
    }

    /// The first provider that claims `url`, and what it made of it.
    pub fn claim(&self, url: &str) -> Option<ForgeRemote> {
        self.providers
            .iter()
            .find_map(|provider| provider.claims(url))
    }

    /// The remote a bookmark's pull requests live on, from the repository's
    /// `(name, url)` remotes: the one it tracks (`preferred`), else `origin`,
    /// else the first remote a provider claims. Returns the remote's name and
    /// project.
    pub fn pick_remote(
        &self,
        remotes: &[(String, String)],
        preferred: Option<&str>,
    ) -> Option<(String, ForgeRemote)> {
        let claimed = |name: &str| {
            remotes
                .iter()
                .find(|(remote, _)| remote == name)
                .and_then(|(_, url)| self.claim(url))
        };
        preferred
            .and_then(|name| claimed(name).map(|forge| (name.to_owned(), forge)))
            .or_else(|| claimed("origin").map(|forge| ("origin".to_owned(), forge)))
            .or_else(|| {
                remotes
                    .iter()
                    .find_map(|(name, url)| self.claim(url).map(|forge| (name.clone(), forge)))
            })
    }
}

/// For each of `branches`, its pull request among `prs`: the open one when
/// there is one, else the latest. In `branches`' order.
pub fn latest_per_branch(
    prs: impl IntoIterator<Item = PullRequest>,
    branches: &[String],
) -> Vec<PullRequest> {
    let mut best: Vec<Option<PullRequest>> = vec![None; branches.len()];
    for pr in prs {
        let Some(slot) = branches.iter().position(|branch| *branch == pr.head) else {
            continue;
        };
        let better = match &best[slot] {
            None => true,
            Some(held) => (pr.state.is_open(), pr.number) > (held.state.is_open(), held.number),
        };
        if better {
            best[slot] = Some(pr);
        }
    }
    best.into_iter().flatten().collect()
}

/// One file of a forge's per-file diff listing: its `patch` (bare hunks, no
/// `diff --git` header) fed through the streaming parser, so row numbering
/// and highlighting match a streamed unified diff. A file without one
/// (binary, oversized) keeps its row.
pub fn patch_to_file(
    path: String,
    old_path: Option<String>,
    status: DiffFileStatus,
    patch: Option<&str>,
) -> DiffFile {
    let mut parser = DiffStreamParser::default();
    parser.begin_file(DiffFile {
        path,
        old_path,
        status,
        hunks: Vec::new(),
        additions: 0,
        deletions: 0,
        collapsed: None,
        blob: None,
        mode_change: None,
    });
    match patch.filter(|patch| !patch.is_empty()) {
        Some(patch) => {
            for line in patch.lines() {
                parser.push_line(line);
            }
        }
        None => {
            parser.push_line("@@ diff unavailable (binary or oversized file) @@");
        }
    }
    parser
        .finish()
        .expect("begin_file seeded a file, so finish returns it")
}

/// A diff that arrives in parts, as a plugin's forge sends one: unified diff
/// text cut anywhere, or one file at a time with its bare hunks. Each file
/// is handed on once it is whole.
#[derive(Default)]
pub struct DiffParts {
    parser: DiffStreamParser,
    /// The end of the last text part, up to its first unfinished line.
    carry: String,
}

impl DiffParts {
    /// A piece of `git diff` text. Lines may be split across pieces; `\r\n`
    /// reads as `\n`.
    pub fn push_text(&mut self, text: &str, on_file: &mut dyn FnMut(DiffFile)) {
        let mut text = text;
        if !self.carry.is_empty() {
            match text.find('\n') {
                Some(end) => {
                    let mut line = std::mem::take(&mut self.carry);
                    line.push_str(&text[..end]);
                    self.line(&line, on_file);
                    text = &text[end + 1..];
                }
                None => {
                    self.carry.push_str(text);
                    return;
                }
            }
        }
        let (whole, rest) = match text.rfind('\n') {
            Some(end) => (&text[..end], &text[end + 1..]),
            None => ("", text),
        };
        if !whole.is_empty() || text.starts_with('\n') {
            for line in whole.split('\n') {
                self.line(line, on_file);
            }
        }
        self.carry.push_str(rest);
    }

    fn line(&mut self, line: &str, on_file: &mut dyn FnMut(DiffFile)) {
        if let Some(file) = self
            .parser
            .push_line(line.strip_suffix('\r').unwrap_or(line))
        {
            on_file(file);
        }
    }

    /// One file, as a forge's API lists it: `patch` holds its `@@` hunks
    /// (absent for a binary or oversized file), `counts` the forge's own
    /// line counts, which win over the patch's.
    pub fn push_file(
        &mut self,
        path: String,
        old_path: Option<String>,
        status: DiffFileStatus,
        patch: Option<&str>,
        counts: Option<(usize, usize)>,
        on_file: &mut dyn FnMut(DiffFile),
    ) {
        let mut file = patch_to_file(path, old_path, status, patch);
        if let Some((additions, deletions)) = counts {
            file.additions = additions;
            file.deletions = deletions;
        }
        on_file(file);
    }

    /// The text ended: hand on the file it was in.
    pub fn finish(mut self, on_file: &mut dyn FnMut(DiffFile)) {
        if !self.carry.is_empty() {
            let line = std::mem::take(&mut self.carry);
            self.line(&line, on_file);
        }
        if let Some(file) = self.parser.finish() {
            on_file(file);
        }
    }
}

/// Lower a pull request's commits into the tab's graph, newest first
/// (matching `jj log`), with a synthetic "All changes" row pinned on top.
/// That row is flagged as the working copy, so the existing
/// `RevisionSelection::WorkingCopy` selection (the tab's default) means "the
/// whole diff", and clicking it brings the full diff back after viewing one
/// commit.
pub fn pr_commit_rows(commits: &[PrCommit]) -> Vec<StreamRow> {
    let row = |id: &str, description: String, author: String, millis, working_copy| CommitSummary {
        change_id: id.to_owned(),
        commit_id: id.to_owned(),
        shortest_change_id_len: None,
        has_description: !description.is_empty(),
        description: if description.is_empty() {
            "(no description set)".to_owned()
        } else {
            description
        },
        author,
        author_timestamp: millis,
        is_empty: Some(false),
        has_conflict: false,
        is_divergent: false,
        is_hidden: false,
        change_offset: None,
        is_working_copy: working_copy,
        is_immutable: false,
        on_trunk: false,
        bookmarks: Vec::new(),
        parent_ids: Vec::new(),
    };
    let mut rows = Vec::with_capacity(commits.len() + 1);
    rows.push(row(
        "pr",
        "All changes".to_owned(),
        String::new(),
        None,
        true,
    ));
    for commit in commits.iter().rev() {
        rows.push(row(
            &commit.oid,
            commit.headline.clone(),
            commit.author.clone(),
            commit.authored_millis,
            false,
        ));
    }

    // A straight chain: each row's parent is the next one down; the last has
    // no listed parent (its real parent is the base, outside this view).
    let lane_inputs = rows.iter().enumerate().map(|(index, row)| {
        let edges: Vec<GraphEdge<String>> = match rows.get(index + 1) {
            Some(parent) => vec![GraphEdge::direct(parent.commit_id.clone())],
            None => Vec::new(),
        };
        (row.commit_id.clone(), edges)
    });
    let frames = assign_lanes(lane_inputs);
    rows.into_iter()
        .zip(frames)
        .map(|(summary, frame)| StreamRow { summary, frame })
        .collect()
}

/// Map a pull request's header onto the [`RevisionDetails`] the diff pane
/// already renders: the number (`#12`, `!12`) as the id, head→base and state
/// as "bookmark" chips, the title and URL as the description.
pub fn pr_revision_details(kind: ForgeKind, info: &PrInfo) -> RevisionDetails {
    RevisionDetails {
        commit_id: format!("{}{}", kind.sigil(), info.number),
        change_id: None,
        bookmarks: vec![
            format!("{} \u{2192} {}", info.head_ref, info.base_ref),
            info.state.clone(),
        ],
        author: SignatureInfo {
            name: info.author.clone(),
            email: String::new(),
            timestamp: None,
        },
        committer: None,
        signature: None,
        description: format!("{}\n\n{}", info.title, info.url),
        parents: Vec::new(),
    }
}

/// GitHub's and GitLab's kinds as their bundled manifests
/// (`plugins/bundled/*/plugin.toml`) give them, for the core's tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::{ForgeKind, ForgeSpec};

    pub(crate) fn github() -> ForgeKind {
        ForgeKind::new(ForgeSpec {
            id: "github".into(),
            name: "GitHub".into(),
            merge_requests: false,
            sigil: '#',
            short: "PR".into(),
            cli: "gh".into(),
            default_host: "github.com".into(),
            host_env: Some("GH_HOST".into()),
            pull_request_path: Some("{owner}/{name}/pull/{number}".into()),
            subgroups: false,
        })
    }

    pub(crate) fn gitlab() -> ForgeKind {
        ForgeKind::new(ForgeSpec {
            id: "gitlab".into(),
            name: "GitLab".into(),
            merge_requests: true,
            sigil: '!',
            short: "MR".into(),
            cli: "glab".into(),
            default_host: "gitlab.com".into(),
            host_env: Some("GITLAB_HOST".into()),
            pull_request_path: Some("{owner}/{name}/-/merge_requests/{number}".into()),
            subgroups: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A forge that hosts everything on `gitlab.com`.
    struct Claims;

    impl ForgeProvider for Claims {
        fn kind(&self) -> ForgeKind {
            ForgeKind::new(ForgeSpec {
                id: "test".into(),
                ..ForgeSpec::default()
            })
        }

        fn claims(&self, remote_url: &str) -> Option<ForgeRemote> {
            let (host, owner, name) = parse_remote_url(remote_url)?;
            (host == "gitlab.com").then(|| ForgeRemote::new(self.kind(), &host, &owner, &name))
        }

        fn pull_requests(
            &self,
            _remote: ForgeRemote,
            _branches: Vec<String>,
        ) -> ForgeFuture<'_, Vec<PullRequest>> {
            Box::pin(async { Err(ForgeError::Unsupported) })
        }

        fn checks(&self, _remote: ForgeRemote, _sha: String) -> ForgeFuture<'_, Checks> {
            Box::pin(async { Err(ForgeError::Unsupported) })
        }

        fn create_pull_request(
            &self,
            _remote: ForgeRemote,
            _create: CreatePr,
        ) -> ForgeFuture<'_, PullRequest> {
            Box::pin(async { Err(ForgeError::Unsupported) })
        }

        fn update_pull_request(
            &self,
            _remote: ForgeRemote,
            _number: u64,
            _update: UpdatePr,
        ) -> ForgeFuture<'_, PullRequest> {
            Box::pin(async { Err(ForgeError::Unsupported) })
        }
    }

    fn forges() -> Forges {
        Forges::new(vec![Arc::new(Claims)])
    }

    #[test]
    fn the_tracked_remote_wins_then_origin_then_the_first_claimed() {
        let forges = forges();
        let remotes = vec![
            ("backup".to_owned(), "/srv/backup.git".to_owned()),
            ("fork".to_owned(), "git@gitlab.com:me/app.git".to_owned()),
            (
                "origin".to_owned(),
                "git@gitlab.com:team/app.git".to_owned(),
            ),
        ];
        let pick = |preferred| {
            forges
                .pick_remote(&remotes, preferred)
                .map(|(name, _)| name)
        };
        assert_eq!(pick(Some("fork")).as_deref(), Some("fork"));
        assert_eq!(pick(None).as_deref(), Some("origin"));
        // A tracked remote no forge hosts falls back like none was named.
        assert_eq!(pick(Some("backup")).as_deref(), Some("origin"));
        let no_origin = &remotes[..2];
        assert_eq!(
            forges
                .pick_remote(no_origin, None)
                .map(|(name, _)| name)
                .as_deref(),
            Some("fork")
        );
        assert!(forges.pick_remote(&remotes[..1], None).is_none());
    }

    #[test]
    fn a_diff_cut_anywhere_parses_like_a_whole_one() {
        let text = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\r\ndiff --git a/b.rs b/b.rs\nnew file mode 100644\n--- /dev/null\n+++ b/b.rs\n@@ -0,0 +1 @@\n+x\n";
        let whole = crate::diff_parse::parse_unified_diff(&text.replace('\r', ""));
        for cut in [1, 3, 7, 40] {
            let mut parts = DiffParts::default();
            let mut files = Vec::new();
            let chars: Vec<char> = text.chars().collect();
            for piece in chars.chunks(cut) {
                parts.push_text(&piece.iter().collect::<String>(), &mut |file| {
                    files.push(file)
                });
            }
            parts.finish(&mut |file| files.push(file));
            let paths: Vec<_> = files
                .iter()
                .map(|file| (file.path.clone(), file.additions, file.deletions))
                .collect();
            let expected: Vec<_> = whole
                .files
                .iter()
                .map(|file| (file.path.clone(), file.additions, file.deletions))
                .collect();
            assert_eq!(paths, expected, "pieces of {cut}");
            assert_eq!(
                files[0].hunks[0].lines[2].content,
                whole.files[0].hunks[0].lines[2].content
            );
        }
        let mut parts = DiffParts::default();
        let mut files = Vec::new();
        parts.push_file(
            "big.bin".into(),
            None,
            DiffFileStatus::Added,
            None,
            Some((0, 0)),
            &mut |file| files.push(file),
        );
        assert!(files[0].hunks[0].header.contains("diff unavailable"));
    }

    #[test]
    fn errors_say_what_to_do() {
        let missing = ForgeError::MissingCli {
            kind: fixtures::gitlab(),
        }
        .to_string();
        assert!(missing.contains("glab auth login"), "{missing}");
        assert!(missing.contains("PATH"), "{missing}");
        let auth = ForgeError::NotAuthenticated {
            kind: fixtures::github(),
            host: "github.acme.com".to_owned(),
        }
        .to_string();
        assert!(
            auth.contains("gh auth login --hostname github.acme.com"),
            "{auth}"
        );
    }
}

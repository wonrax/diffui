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
//! state. The built-in providers shell out to each forge's own CLI (`gh`,
//! `glab`), which keeps tokens out of diffui.

mod cli;
pub mod github;
pub mod gitlab;
mod remote;

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
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

/// Which forge a remote or a pull request lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForgeKind {
    GitHub,
    GitLab,
}

impl ForgeKind {
    /// The forge's name, for messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
        }
    }

    /// What the forge calls a pull request, lowercase.
    pub fn noun(self) -> &'static str {
        match self {
            Self::GitHub => "pull request",
            Self::GitLab => "merge request",
        }
    }

    /// [`Self::noun`], sentence case.
    pub fn title_noun(self) -> &'static str {
        match self {
            Self::GitHub => "Pull request",
            Self::GitLab => "Merge request",
        }
    }

    /// The abbreviation: `PR`, `MR`.
    pub fn short(self) -> &'static str {
        match self {
            Self::GitHub => "PR",
            Self::GitLab => "MR",
        }
    }

    /// What prefixes a number in the forge's own references: `#12`, `!12`.
    pub fn sigil(self) -> char {
        match self {
            Self::GitHub => '#',
            Self::GitLab => '!',
        }
    }

    /// The CLI the built-in provider drives.
    pub fn cli(self) -> &'static str {
        match self {
            Self::GitHub => "gh",
            Self::GitLab => "glab",
        }
    }

    /// The host a reference without one means.
    pub fn default_host(self) -> &'static str {
        match self {
            Self::GitHub => "github.com",
            Self::GitLab => "gitlab.com",
        }
    }

    /// The environment variable that points the CLI at a host.
    fn host_env(self) -> &'static str {
        match self {
            Self::GitHub => "GH_HOST",
            Self::GitLab => "GITLAB_HOST",
        }
    }

    /// The host a reference without one resolves to: the CLI's environment
    /// override, else the forge's public host.
    pub fn implied_host(self) -> String {
        std::env::var(self.host_env())
            .ok()
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
            Self::NotAuthenticated { kind, host } => write!(
                f,
                "{} isn't signed in to {host}. Run `{} auth login --hostname {host}` in a \
                 terminal, then try again.",
                kind.cli(),
                kind.cli()
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

/// A forge, as diffui talks to it. See the module docs.
pub trait ForgeProvider: Send + Sync {
    fn kind(&self) -> ForgeKind;

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

/// The providers diffui knows, in the order they're asked to claim a remote.
#[derive(Clone)]
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

impl Default for Forges {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Forges {
    /// GitHub through `gh` and GitLab through `glab`, as found on the PATH.
    pub fn builtin() -> Self {
        Self::with_programs("gh".into(), "glab".into())
    }

    /// The built-in providers driving these programs: a test points them at
    /// fakes.
    pub fn with_programs(gh: PathBuf, glab: PathBuf) -> Self {
        Self::new(vec![
            Arc::new(github::GitHub::new(gh)),
            Arc::new(gitlab::GitLab::new(glab)),
        ])
    }

    pub fn new(providers: Vec<Arc<dyn ForgeProvider>>) -> Self {
        Self { providers }
    }

    pub fn provider(&self, kind: ForgeKind) -> Option<Arc<dyn ForgeProvider>> {
        self.providers
            .iter()
            .find(|provider| provider.kind() == kind)
            .cloned()
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
pub(crate) fn latest_per_branch(
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
pub(crate) fn patch_to_file(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn forges() -> Forges {
        Forges::with_programs("gh-absent".into(), "glab-absent".into())
    }

    #[test]
    fn providers_claim_their_hosts() {
        let forges = forges();
        let github = forges
            .claim("git@github.com:team/app.git")
            .expect("claimed");
        assert_eq!(github.kind, ForgeKind::GitHub);
        assert_eq!(github.web_url, "https://github.com/team/app");
        let gitlab = forges
            .claim("https://gitlab.com/group/sub/app.git")
            .expect("claimed");
        assert_eq!(gitlab.kind, ForgeKind::GitLab);
        assert_eq!(
            (gitlab.owner.as_str(), gitlab.name.as_str()),
            ("group/sub", "app")
        );
        let enterprise = forges
            .claim("ssh://git@github.acme.com:2222/team/app")
            .expect("claimed");
        assert_eq!(enterprise.host, "github.acme.com");
        let company = forges
            .claim("git@gitlab.acme.org:infra/tools.git")
            .expect("claimed");
        assert_eq!(company.kind, ForgeKind::GitLab);
        assert!(forges.claim("git@example.org:team/app.git").is_none());
        assert!(forges.claim("/srv/git/app.git").is_none());
    }

    #[test]
    fn the_tracked_remote_wins_then_origin_then_the_first_claimed() {
        let forges = forges();
        let remotes = vec![
            ("backup".to_owned(), "/srv/backup.git".to_owned()),
            ("fork".to_owned(), "git@github.com:me/app.git".to_owned()),
            (
                "origin".to_owned(),
                "git@github.com:team/app.git".to_owned(),
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
    fn errors_say_what_to_do() {
        let missing = ForgeError::MissingCli {
            kind: ForgeKind::GitLab,
        }
        .to_string();
        assert!(missing.contains("glab auth login"), "{missing}");
        assert!(missing.contains("PATH"), "{missing}");
        let auth = ForgeError::NotAuthenticated {
            kind: ForgeKind::GitHub,
            host: "github.acme.com".to_owned(),
        }
        .to_string();
        assert!(
            auth.contains("gh auth login --hostname github.acme.com"),
            "{auth}"
        );
    }
}

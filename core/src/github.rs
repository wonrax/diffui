//! GitHub pull-request diff source, shelling out to the `gh` CLI (must be on
//! PATH and authenticated for the PR's host — github.com or a GitHub
//! Enterprise Server, see [`PrSpec::host`]). The diff **streams**: `gh pr diff` stdout is
//! parsed incrementally and each completed file is handed back as soon as its
//! last hunk is read, so a huge PR paints progressively while the download is
//! still running. Header metadata (`gh pr view`) is field-separated through
//! `--jq`, so this module needs no JSON dependency.

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use jj_lib::graph::GraphEdge;

use crate::diff_parse::DiffStreamParser;
use crate::graph::assign_lanes;

use crate::model::{
    CommitSummary, DiffDocument, DiffFile, DiffFileStatus, RevisionDetails, SignatureInfo,
    StreamRow,
};

/// GitHub's REST API caps `per_page` at 100; we always request the max and
/// page until a short page signals the end. Used both in the request URL and
/// the `len() < PAGE_SIZE` end-of-pagination check.
const GITHUB_API_PAGE_SIZE: usize = 100;

/// A pull-request reference: `[host/]owner/repo#number`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrSpec {
    /// The GitHub host for a GitHub Enterprise Server PR (`github.acme.com`),
    /// read off the URL or the reference. `None` leaves the choice to `gh`:
    /// `GH_HOST` when set, github.com otherwise.
    pub host: Option<String>,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrSpec {
    /// Parse the forms people paste: a PR URL
    /// (`https://github.com/o/r/pull/123`, trailing `/files`-style segments and
    /// queries tolerated), `o/r#123`, and `o/r/pull/123`. Returns `None` for
    /// anything that doesn't unambiguously name a PR, so a caller (the open
    /// dialog) can fall back to treating the input as a local path.
    ///
    /// Any host works, so a GitHub Enterprise URL
    /// (`https://github.acme.com/o/r/pull/7`) or reference
    /// (`github.acme.com/o/r#7`) carries its host along. A host without a
    /// scheme must look like one (contain a dot) so a relative path can't be
    /// mistaken for it.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim().trim_end_matches('/');

        if let Some(rest) = input
            .strip_prefix("https://")
            .or_else(|| input.strip_prefix("http://"))
        {
            let (host, path) = rest.split_once('/')?;
            return Self::parse_path(path).map(|spec| spec.on_host(host));
        }

        // `o/r#123` or `host/o/r#123`. Counting the slugs' slashes keeps
        // plain paths (`a/b/c`) and shell fragments out.
        if let Some((slug, number)) = input.split_once('#') {
            let parts: Vec<&str> = slug.split('/').collect();
            let (host, owner, repo) = match parts.as_slice() {
                [owner, repo] => (None, *owner, *repo),
                [host, owner, repo] if looks_like_host(host) => (Some(*host), *owner, *repo),
                _ => return None,
            };
            if owner.is_empty() || repo.is_empty() {
                return None;
            }
            let spec = Self {
                host: None,
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                number: number.parse().ok()?,
            };
            return Some(match host {
                Some(host) => spec.on_host(host),
                None => spec,
            });
        }

        // `host/o/r/pull/123` (URL form without the scheme).
        if let Some((host, path)) = input.split_once('/')
            && looks_like_host(host)
            && let Some(spec) = Self::parse_path(path)
        {
            return Some(spec.on_host(host));
        }

        // Bare `o/r/pull/123` (URL form without the host).
        if input.contains("/pull/") {
            return Self::parse_path(input);
        }
        None
    }

    /// `self` on `host`, with github.com folded back to `None` so the two
    /// spellings of a github.com PR are one tab.
    fn on_host(mut self, host: &str) -> Self {
        let host = host.to_ascii_lowercase();
        self.host = (host != "github.com" && host != "www.github.com").then_some(host);
        self
    }

    /// The host `gh` is pointed at, for messages: the explicit one, else
    /// whatever `gh` defaults to.
    pub fn host_label(&self) -> String {
        match &self.host {
            Some(host) => host.clone(),
            None => std::env::var("GH_HOST")
                .ok()
                .filter(|host| !host.is_empty())
                .unwrap_or_else(|| "github.com".to_owned()),
        }
    }

    /// Fill in the host from a local repository's remote: a bare `o/r#123`
    /// typed while a repo whose remote is `git@github.acme.com:o/r.git` is
    /// open means the Enterprise PR, not a github.com one.
    pub fn with_host_from_remotes<'a>(
        self,
        remote_urls: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        if self.host.is_some() {
            return self;
        }
        for url in remote_urls {
            if let Some((host, owner, repo)) = parse_remote_url(url)
                && owner.eq_ignore_ascii_case(&self.owner)
                && repo.eq_ignore_ascii_case(&self.repo)
            {
                return self.on_host(&host);
            }
        }
        self
    }

    /// Parse `owner/repo/pull/number[...]`.
    fn parse_path(path: &str) -> Option<Self> {
        let mut parts = path.split('/');
        let owner = parts.next().filter(|s| !s.is_empty())?;
        let repo = parts.next().filter(|s| !s.is_empty())?;
        if parts.next()? != "pull" {
            return None;
        }
        // The number segment may drag a query or fragment along
        // (`123?diff=split`); the leading digit run is the number.
        let segment = parts.next()?;
        let digits = segment.split(|c: char| !c.is_ascii_digit()).next()?;
        Some(Self {
            host: None,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number: digits.parse().ok()?,
        })
    }

    /// `owner/repo`, the form the REST paths take.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// `[host/]owner/repo`, the form `gh --repo` takes.
    fn repo_arg(&self) -> String {
        match &self.host {
            Some(host) => format!("{host}/{}/{}", self.owner, self.repo),
            None => self.slug(),
        }
    }

    /// `gh` with the host applied: `GH_HOST` for the commands that take
    /// `--repo` (which carries the host too), so `gh api` resolves against
    /// the same server.
    fn gh(&self) -> Command {
        let mut command = Command::new("gh");
        if let Some(host) = &self.host {
            command.env("GH_HOST", host);
        }
        command.stdin(Stdio::null());
        command
    }

    /// Short display label: `repo#number`.
    pub fn label(&self) -> String {
        format!("{}#{}", self.repo, self.number)
    }
}

/// The remote URLs of the repository at `root` (jj or git), read straight
/// from its git config — best effort, empty when there is none to read.
pub fn remote_urls(root: &std::path::Path) -> Vec<String> {
    let git_dir = if root.join(".jj").is_dir() {
        crate::jj::settings::backing_git_dir(root)
    } else {
        crate::repository::resolve_git_dirs(root)
            .ok()
            .map(|dirs| dirs.common_dir)
    };
    let Some(config) = git_dir.and_then(|dir| std::fs::read_to_string(dir.join("config")).ok())
    else {
        return Vec::new();
    };
    remote_urls_in_config(&config)
}

/// `url` values under `[remote "…"]` sections of a git config file.
fn remote_urls_in_config(config: &str) -> Vec<String> {
    let mut in_remote = false;
    let mut urls = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_remote = line.starts_with("[remote ");
            continue;
        }
        if !in_remote {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
        {
            urls.push(value.trim().trim_matches('"').to_owned());
        }
    }
    urls
}

fn looks_like_host(segment: &str) -> bool {
    segment.contains('.') && !segment.starts_with('.') && !segment.contains(['~', ' '])
}

/// `(host, owner, repo)` of a git remote URL in any of the forms git takes:
/// `https://host/o/r(.git)`, `ssh://git@host[:port]/o/r`, `git@host:o/r.git`.
pub fn parse_remote_url(url: &str) -> Option<(String, String, String)> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (host, path)
    } else {
        // scp-like `[user@]host:path`.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (authority.rsplit('@').next()?, path)
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    if host.is_empty() || owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((host.to_ascii_lowercase(), owner.to_owned(), repo.to_owned()))
}

/// What to tell the user when `gh` itself couldn't be started.
fn gh_spawn_error(error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!(
            "The GitHub CLI (`gh`) wasn't found. Install it (e.g. `brew install gh`) and run \
             `gh auth login`. If it is installed: an app opened from Finder or the Dock only \
             sees the system PATH (/usr/bin:/bin:/usr/sbin:/sbin), so gh in /opt/homebrew/bin \
             or ~/.local/bin is invisible to it. Launch diffui from a terminal, or make gh's \
             directory part of the launchd PATH."
        )
    } else {
        anyhow::Error::new(error).context("failed to run `gh`")
    }
}

/// Turn a failed `gh` invocation's stderr into something actionable, or
/// `None` to report it as is. gh relays the API's own messages, so this keys
/// on the phrases gh and GitHub use for the cases that have a fix.
fn explain_gh_failure(spec: &PrSpec, stderr: &str) -> Option<String> {
    let host = spec.host_label();
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("gh auth login")
        || lower.contains("not logged in")
        || lower.contains("authentication required")
        || lower.contains("bad credentials")
        || lower.contains("http 401")
    {
        return Some(format!(
            "gh isn't signed in to {host}. Run `gh auth login --hostname {host}` in a terminal, \
             then reopen the tab."
        ));
    }
    if lower.contains("rate limit") {
        return Some(format!(
            "GitHub's API rate limit on {host} was hit. Wait a few minutes and reopen the tab \
             (`gh api rate_limit` shows when it resets)."
        ));
    }
    if lower.contains("could not resolve to a pullrequest")
        || lower.contains("could not resolve to a repository")
        || lower.contains("http 404")
    {
        return Some(format!(
            "{}#{} wasn't found on {host}, or the account gh is signed in with can't see it. \
             For a GitHub Enterprise PR, open it by its full URL.",
            spec.slug(),
            spec.number
        ));
    }
    if lower.contains("error connecting to")
        || lower.contains("no such host")
        || lower.contains("could not resolve host")
    {
        return Some(format!(
            "Couldn't reach {host}. Check the network (and the VPN, for an Enterprise host)."
        ));
    }
    None
}

/// The error for a `gh` run that exited non-zero.
fn gh_failure(
    spec: &PrSpec,
    what: &str,
    status: std::process::ExitStatus,
    stderr: &str,
) -> anyhow::Error {
    match explain_gh_failure(spec, stderr) {
        Some(explained) => anyhow::anyhow!("{explained}\n\n{what}: {}", stderr.trim()),
        None => anyhow::anyhow!("{what} exited with {status}: {}", stderr.trim()),
    }
}

/// Run `gh` against `spec`'s host with `args` and return its stdout. `what`
/// names the invocation in the error (e.g. "gh pr view").
async fn run_gh(spec: &PrSpec, args: &[&str], what: &str) -> Result<Vec<u8>> {
    let output = spec
        .gh()
        .args(args)
        .output()
        .await
        .map_err(gh_spawn_error)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(gh_failure(spec, what, output.status, &stderr));
    }
    Ok(output.stdout)
}

/// PR header metadata from `gh pr view`.
#[derive(Debug, Clone, Default)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub author: String,
    /// `OPEN` / `MERGED` / `CLOSED` (GitHub's wire values).
    pub state: String,
    pub base_ref: String,
    pub head_ref: String,
    pub additions: usize,
    pub deletions: usize,
    pub changed_files: usize,
    pub url: String,
}

/// Fetch the PR's header metadata. The `--jq` join uses `\u{1f}` (the same
/// unit-separator trick as the git backend) so titles containing tabs or
/// quotes can't break the framing; PR titles cannot contain newlines.
pub async fn fetch_pr_info(spec: &PrSpec) -> Result<PrInfo> {
    const JQ: &str = r#"[(.number|tostring), .title, .author.login, .state, .baseRefName, .headRefName, (.additions|tostring), (.deletions|tostring), (.changedFiles|tostring), .url] | join("\u001f")"#;
    let stdout = run_gh(
        spec,
        &[
            "pr",
            "view",
            &spec.number.to_string(),
            "--repo",
            &spec.repo_arg(),
            "--json",
            "number,title,author,state,baseRefName,headRefName,additions,deletions,changedFiles,url",
            "--jq",
            JQ,
        ],
        "gh pr view",
    )
    .await?;

    let stdout = String::from_utf8_lossy(&stdout);
    let mut parts = stdout.trim_end_matches('\n').split('\u{1f}');
    let mut next = || parts.next().unwrap_or("").to_owned();
    Ok(PrInfo {
        number: next().parse().unwrap_or(spec.number),
        title: next(),
        author: next(),
        state: next(),
        base_ref: next(),
        head_ref: next(),
        additions: next().parse().unwrap_or(0),
        deletions: next().parse().unwrap_or(0),
        changed_files: next().parse().unwrap_or(0),
        url: next(),
    })
}

/// One commit of a pull request, from `gh pr view --json commits`.
#[derive(Debug, Clone)]
pub struct PrCommit {
    pub oid: String,
    pub author: String,
    pub headline: String,
}

/// Fetch the PR's commits, oldest first (GitHub's order). Same `\u{1f}` field
/// framing as [`fetch_pr_info`]; headlines are single-line by construction.
pub async fn fetch_pr_commits(spec: &PrSpec) -> Result<Vec<PrCommit>> {
    const JQ: &str =
        r#".commits[] | [.oid, (.authors[0].login // ""), .messageHeadline] | join("\u001f")"#;
    let stdout = run_gh(
        spec,
        &[
            "pr",
            "view",
            &spec.number.to_string(),
            "--repo",
            &spec.repo_arg(),
            "--json",
            "commits",
            "--jq",
            JQ,
        ],
        "gh pr view --json commits",
    )
    .await?;

    let stdout = String::from_utf8_lossy(&stdout);
    Ok(stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut parts = line.split('\u{1f}');
            let mut next = || parts.next().unwrap_or("").to_owned();
            PrCommit {
                oid: next(),
                author: next(),
                headline: next(),
            }
        })
        .collect())
}

/// Lower the PR's commits into the sidebar's commit store + a single-lane
/// graph, newest first (matching `jj log`), with a synthetic "All changes"
/// row pinned on top. That row is flagged as the working copy, so the
/// existing `RevisionSelection::WorkingCopy` selection — the tab's default —
/// means "the whole PR diff" and clicking it brings the full diff back after
/// viewing an individual commit.
pub fn pr_commit_rows(commits: &[PrCommit]) -> Vec<StreamRow> {
    let mut rows = Vec::with_capacity(commits.len() + 1);
    rows.push(CommitSummary {
        change_id: "pr".to_owned(),
        commit_id: "pr".to_owned(),
        shortest_change_id_len: None,
        description: "All changes".to_owned(),
        author: String::new(),
        has_description: true,
        is_empty: Some(false),
        has_conflict: false,
        is_divergent: false,
        is_hidden: false,
        change_offset: None,
        is_working_copy: true,
        is_immutable: false,
        bookmarks: Vec::new(),
        parent_ids: Vec::new(),
    });
    for commit in commits.iter().rev() {
        rows.push(CommitSummary {
            change_id: commit.oid.clone(),
            commit_id: commit.oid.clone(),
            shortest_change_id_len: None,
            description: if commit.headline.is_empty() {
                "(no description set)".to_owned()
            } else {
                commit.headline.clone()
            },
            author: commit.author.clone(),
            has_description: !commit.headline.is_empty(),
            is_empty: Some(false),
            has_conflict: false,
            is_divergent: false,
            is_hidden: false,
            change_offset: None,
            is_working_copy: false,
            is_immutable: false,
            bookmarks: Vec::new(),
            parent_ids: Vec::new(),
        });
    }

    // A straight chain: each row's parent is the next one down; the last has
    // no listed parent (its real parent is the PR's base, outside this view).
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

/// Wire shape of `GET /repos/{owner}/{repo}/commits/{sha}` — the same
/// per-file entries as the PR files endpoint, wrapped in a commit object.
#[derive(serde::Deserialize)]
struct ApiCommit {
    #[serde(default)]
    files: Vec<ApiPrFile>,
    #[serde(default)]
    stats: Option<ApiCommitStats>,
    #[serde(default)]
    commit: Option<ApiCommitMeta>,
}

#[derive(serde::Deserialize)]
struct ApiCommitStats {
    #[serde(default)]
    additions: usize,
    #[serde(default)]
    deletions: usize,
}

#[derive(serde::Deserialize)]
struct ApiCommitMeta {
    #[serde(default)]
    message: String,
    #[serde(default)]
    author: Option<ApiCommitSignature>,
}

#[derive(serde::Deserialize)]
struct ApiCommitSignature {
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    date: String,
}

/// Load one PR commit's diff through the commits REST endpoint (paginated —
/// the per-file entries match the files-API shape, so the same lowering
/// applies). Returns the document plus a `jj show`-style header built from
/// the commit's message and author.
pub async fn load_pr_commit_diff(
    spec: &PrSpec,
    oid: &str,
) -> Result<(DiffDocument, Option<RevisionDetails>), String> {
    fetch_pr_commit_diff(spec, oid)
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn fetch_pr_commit_diff(
    spec: &PrSpec,
    oid: &str,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    let mut files = Vec::new();
    let mut details: Option<RevisionDetails> = None;
    let mut totals: Option<(usize, usize)> = None;
    let mut page = 1usize;
    loop {
        let path = format!(
            "repos/{}/commits/{oid}?per_page={GITHUB_API_PAGE_SIZE}&page={page}",
            spec.slug()
        );
        let stdout = run_gh(spec, &["api", &path], "gh api commit diff").await?;
        let commit: ApiCommit =
            serde_json::from_slice(&stdout).context("failed to parse the commit API response")?;
        if details.is_none() {
            details = commit.commit.map(|meta| {
                let author = meta.author.unwrap_or(ApiCommitSignature {
                    name: String::new(),
                    email: String::new(),
                    date: String::new(),
                });
                RevisionDetails {
                    commit_id: oid.to_owned(),
                    change_id: None,
                    bookmarks: Vec::new(),
                    author: SignatureInfo {
                        name: author.name,
                        email: author.email,
                        timestamp: Some(author.date).filter(|date| !date.is_empty()),
                    },
                    committer: None,
                    signature: None,
                    description: meta.message,
                    parents: Vec::new(),
                }
            });
        }
        if totals.is_none() {
            totals = commit.stats.map(|stats| (stats.additions, stats.deletions));
        }
        let last = commit.files.len() < GITHUB_API_PAGE_SIZE;
        files.extend(commit.files.into_iter().map(api_file_to_diff_file));
        if last {
            break;
        }
        page += 1;
    }

    // Prefer the commit's own stats over summed files — same oversized-blob
    // zeroing as the PR files endpoint.
    let (total_additions, total_deletions) = totals.unwrap_or_else(|| {
        (
            files.iter().map(|file| file.additions).sum(),
            files.iter().map(|file| file.deletions).sum(),
        )
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
}

/// Map PR metadata onto the [`RevisionDetails`] header the diff pane already
/// renders: the PR number as the id, head→base and state as "bookmark" chips,
/// the title + URL as the description.
pub fn pr_revision_details(info: &PrInfo) -> RevisionDetails {
    RevisionDetails {
        commit_id: format!("#{}", info.number),
        change_id: None,
        bookmarks: vec![
            format!("{} → {}", info.head_ref, info.base_ref),
            info.state.to_lowercase(),
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

/// Stream the PR's diff, invoking `on_file` for each completed
/// (already-highlighted) file. Prefers `gh pr diff` (one request, true
/// streaming); when GitHub refuses to serve the unified diff outright
/// (HTTP 406 — more than 300 changed files), falls back to paging through the
/// REST files endpoint, which serves per-file patches up to GitHub's
/// 3,000-file listing cap (the app surfaces any shortfall against
/// `changedFiles` in the activity log).
pub async fn stream_pr_diff(spec: &PrSpec, mut on_file: impl FnMut(DiffFile)) -> Result<()> {
    let mut emitted = 0usize;
    let result = stream_pr_diff_gh(spec, |file| {
        emitted += 1;
        on_file(file);
    })
    .await;
    match result {
        // Only the too-large refusal reroutes — an auth/network failure would
        // fail the files API identically, and a second error just confuses.
        // `emitted == 0` because the 406 arrives before any diff bytes; if
        // files already streamed, rerouting would duplicate them.
        Err(error) if emitted == 0 && is_diff_too_large(&error) => {
            stream_pr_files_api(spec, on_file).await
        }
        other => other,
    }
}

/// GitHub's "Sorry, the diff exceeded the maximum number of files" refusal
/// (HTTP 406 / `PullRequest.diff too_large`), as relayed through gh's stderr.
fn is_diff_too_large(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("HTTP 406") || text.contains("too_large") || text.contains("exceeded the maximum")
}

/// One file from the REST "List pull request files" endpoint. `patch` is
/// absent for binary or oversized files.
#[derive(serde::Deserialize)]
struct ApiPrFile {
    filename: String,
    #[serde(default)]
    previous_filename: Option<String>,
    status: String,
    #[serde(default)]
    additions: usize,
    #[serde(default)]
    deletions: usize,
    #[serde(default)]
    patch: Option<String>,
}

/// Page through `GET /repos/{owner}/{repo}/pulls/{n}/files`, lowering each
/// entry to a [`DiffFile`]. Streams page-by-page (100 files per request).
async fn stream_pr_files_api(spec: &PrSpec, mut on_file: impl FnMut(DiffFile)) -> Result<()> {
    let mut page = 1usize;
    loop {
        let path = format!(
            "repos/{}/pulls/{}/files?per_page={GITHUB_API_PAGE_SIZE}&page={page}",
            spec.slug(),
            spec.number
        );
        let stdout = run_gh(spec, &["api", &path], "gh api pull-request files").await?;
        let files: Vec<ApiPrFile> = serde_json::from_slice(&stdout)
            .context("failed to parse the pull-request files API response")?;
        // A short (or empty) page is the last one; GitHub also simply stops
        // listing past its 3,000-file cap.
        let last = files.len() < GITHUB_API_PAGE_SIZE;
        for file in files {
            on_file(api_file_to_diff_file(file));
        }
        if last {
            return Ok(());
        }
        page += 1;
    }
}

/// Lower one files-API entry to a [`DiffFile`], feeding its `patch` (bare
/// hunks, no `diff --git` header) through the streaming parser so row
/// numbering and highlighting match the `gh pr diff` path.
fn api_file_to_diff_file(file: ApiPrFile) -> DiffFile {
    let status = match file.status.as_str() {
        "added" => DiffFileStatus::Added,
        "removed" => DiffFileStatus::Deleted,
        // The REST vocabulary also has "copied"; rendering it as a rename
        // shows the source path, which is the closest fit we have.
        "renamed" | "copied" => DiffFileStatus::Renamed,
        _ => DiffFileStatus::Modified,
    };
    let mut parser = DiffStreamParser::default();
    parser.begin_file(DiffFile {
        path: file.filename,
        old_path: file.previous_filename,
        status,
        hunks: Vec::new(),
        additions: 0,
        deletions: 0,
        collapsed: None,
    });
    match &file.patch {
        Some(patch) => {
            for line in patch.lines() {
                parser.push_line(line);
            }
        }
        None => {
            // Keep the row visible with its counts so the file list stays
            // complete even though there's nothing to render.
            parser.push_line("@@ diff unavailable (binary or oversized file) @@");
        }
    }
    let mut out = parser
        .finish()
        .expect("begin_file seeded a file, so finish returns it");
    // The API's counts cover the whole file even when the patch was omitted;
    // trust them over the parsed rows.
    out.additions = file.additions;
    out.deletions = file.deletions;
    out
}

/// The `gh pr diff` path: one process, unified diff streamed off its stdout.
/// Returns once the stream ends; a non-zero exit fails with captured stderr.
async fn stream_pr_diff_gh(spec: &PrSpec, mut on_file: impl FnMut(DiffFile)) -> Result<()> {
    let mut child = spec
        .gh()
        .args([
            "pr",
            "diff",
            &spec.number.to_string(),
            "--repo",
            &spec.repo_arg(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(gh_spawn_error)?;

    let stdout = child.stdout.take().context("gh stdout unavailable")?;
    let mut stderr = child.stderr.take().context("gh stderr unavailable")?;
    // Drain stderr concurrently so a chatty gh can't fill its pipe and
    // deadlock while we're blocked reading stdout.
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    });

    let mut parser = DiffStreamParser::default();
    // Split on bytes and decode each line lossily: `lines()` validates UTF-8
    // and would abort the stream halfway through the first PR touching a
    // latin-1 file. The trailing `\r` is dropped the way `str::lines` does it,
    // so a CRLF diff parses like any other.
    let mut lines = BufReader::with_capacity(256 * 1024, stdout).split(b'\n');
    while let Some(raw) = lines
        .next_segment()
        .await
        .context("failed to read gh pr diff output")?
    {
        let raw = raw.strip_suffix(b"\r").unwrap_or(&raw);
        if let Some(file) = parser.push_line(&String::from_utf8_lossy(raw)) {
            on_file(file);
        }
    }
    if let Some(file) = parser.finish() {
        on_file(file);
    }

    let status = child.wait().await.context("failed to await gh")?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if !status.success() {
        return Err(gh_failure(spec, "gh pr diff", status, &stderr_text));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_urls() {
        let expected = Some(PrSpec {
            host: None,
            owner: "anthropics".to_owned(),
            repo: "claude-code".to_owned(),
            number: 123,
        });
        assert_eq!(
            PrSpec::parse("https://github.com/anthropics/claude-code/pull/123"),
            expected
        );
        assert_eq!(
            PrSpec::parse("https://github.com/anthropics/claude-code/pull/123/files?diff=split"),
            expected
        );
        assert_eq!(
            PrSpec::parse("github.com/anthropics/claude-code/pull/123"),
            expected
        );
        assert_eq!(PrSpec::parse("anthropics/claude-code/pull/123"), expected);
        assert_eq!(PrSpec::parse("anthropics/claude-code#123"), expected);
    }

    #[test]
    fn api_file_lowering_parses_patch_and_statuses() {
        let file = api_file_to_diff_file(ApiPrFile {
            filename: "src/new name.rs".to_owned(),
            previous_filename: Some("src/old name.rs".to_owned()),
            status: "renamed".to_owned(),
            additions: 1,
            deletions: 1,
            patch: Some("@@ -10,2 +10,2 @@\n context\n-old line\n+new line".to_owned()),
        });
        // Paths with spaces survive (no `diff --git` header round-trip).
        assert_eq!(file.path, "src/new name.rs");
        assert_eq!(file.old_path.as_deref(), Some("src/old name.rs"));
        assert_eq!(file.status, DiffFileStatus::Renamed);
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines[0].old_line, Some(10));
        assert_eq!(file.hunks[0].lines[0].new_line, Some(10));
        assert_eq!((file.additions, file.deletions), (1, 1));

        // No patch (binary / oversized): the row survives with its counts.
        let binary = api_file_to_diff_file(ApiPrFile {
            filename: "big.bin".to_owned(),
            previous_filename: None,
            status: "added".to_owned(),
            additions: 0,
            deletions: 0,
            patch: None,
        });
        assert_eq!(binary.status, DiffFileStatus::Added);
        assert_eq!(binary.hunks.len(), 1);
        assert!(binary.hunks[0].header.contains("diff unavailable"));
    }

    fn on(host: &str, owner: &str, repo: &str, number: u64) -> Option<PrSpec> {
        Some(PrSpec {
            host: Some(host.to_owned()),
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number,
        })
    }

    #[test]
    fn parses_enterprise_hosts() {
        let expected = on("github.acme.com", "team", "app", 42);
        assert_eq!(
            PrSpec::parse("https://github.acme.com/team/app/pull/42/files"),
            expected
        );
        assert_eq!(PrSpec::parse("github.acme.com/team/app/pull/42"), expected);
        assert_eq!(PrSpec::parse("github.acme.com/team/app#42"), expected);
        assert_eq!(PrSpec::parse("GitHub.Acme.com/team/app#42"), expected);
        // github.com spelled out is the same PR as the shorthand.
        assert_eq!(
            PrSpec::parse("https://www.github.com/team/app/pull/42"),
            PrSpec::parse("team/app#42")
        );
        // A relative path with three segments isn't a host reference.
        assert_eq!(PrSpec::parse("code/team/app#42"), None);
    }

    #[test]
    fn parses_remote_urls() {
        let expected = Some((
            "github.acme.com".to_owned(),
            "team".to_owned(),
            "app".to_owned(),
        ));
        assert_eq!(
            parse_remote_url("git@github.acme.com:team/app.git"),
            expected
        );
        assert_eq!(
            parse_remote_url("https://github.acme.com/team/app"),
            expected
        );
        assert_eq!(
            parse_remote_url("ssh://git@github.acme.com:2222/team/app.git"),
            expected
        );
        assert_eq!(parse_remote_url("/srv/git/app.git"), None);
        assert_eq!(parse_remote_url("file:///srv/git/team/app.git"), None);
    }

    #[test]
    fn shorthand_takes_the_host_of_a_matching_remote() {
        let config = r#"
[core]
	bare = false
[remote "origin"]
	url = git@github.acme.com:team/app.git
	fetch = +refs/heads/*:refs/remotes/origin/*
[remote "fork"]
	url = https://github.com/me/app.git
"#;
        let urls = remote_urls_in_config(config);
        assert_eq!(urls.len(), 2);
        let spec = PrSpec::parse("team/app#42")
            .expect("a reference")
            .with_host_from_remotes(urls.iter().map(String::as_str));
        assert_eq!(Some(spec), on("github.acme.com", "team", "app", 42));
        let spec = PrSpec::parse("me/app#1")
            .expect("a reference")
            .with_host_from_remotes(urls.iter().map(String::as_str));
        assert_eq!(spec.host, None, "github.com stays the default");
    }

    #[test]
    fn gh_failures_get_actionable_messages() {
        let spec = on("github.acme.com", "team", "app", 42).expect("spec");
        let auth = explain_gh_failure(
            &spec,
            "To get started with GitHub CLI, please run:  gh auth login",
        )
        .expect("an auth hint");
        assert!(auth.contains("gh auth login --hostname github.acme.com"));
        let limited = explain_gh_failure(&spec, "HTTP 403: API rate limit exceeded for user ID 1.")
            .expect("a rate-limit hint");
        assert!(limited.contains("rate limit"));
        assert!(explain_gh_failure(&spec, "HTTP 404: Not Found").is_some());
        let offline = explain_gh_failure(
            &spec,
            "error connecting to github.acme.com\ncheck your internet connection",
        )
        .expect("a network hint");
        assert!(offline.contains("Couldn't reach github.acme.com"));
        assert!(explain_gh_failure(&spec, "something else").is_none());
        let missing = gh_spawn_error(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(format!("{missing}").contains("PATH"));
    }

    #[test]
    fn rejects_non_pr_inputs() {
        // Local paths must fall through to the repository-open flow.
        assert_eq!(PrSpec::parse("~/code/diffui"), None);
        assert_eq!(PrSpec::parse("/abs/path/repo"), None);
        assert_eq!(PrSpec::parse("relative/repo"), None);
        assert_eq!(PrSpec::parse("a/b/c#1"), None);
        assert_eq!(PrSpec::parse("owner/repo#notanumber"), None);
        assert_eq!(
            PrSpec::parse("https://github.com/anthropics/claude-code/issues/123"),
            None
        );
        assert_eq!(PrSpec::parse(""), None);
    }
}

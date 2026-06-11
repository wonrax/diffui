//! GitHub pull-request diff source, shelling out to the `gh` CLI (must be on
//! PATH and authenticated). The diff **streams**: `gh pr diff` stdout is
//! parsed incrementally and each completed file is handed back as soon as its
//! last hunk is read, so a huge PR paints progressively while the download is
//! still running. Header metadata (`gh pr view`) is field-separated through
//! `--jq`, so this module needs no JSON dependency.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use jj_lib::graph::GraphEdge;

use crate::diff_parse::DiffStreamParser;
use crate::graph::assign_lanes;
use crate::graph_layout::{GraphLayout, GraphLayoutBuilder};
use crate::model::{
    CommitStore, CommitStoreBuilder, CommitSummary, DiffDocument, DiffFile, DiffFileStatus,
    RevisionDetails, SignatureInfo,
};

/// A pull-request reference: `owner/repo#number`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSpec {
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
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim().trim_end_matches('/');

        let rest = input
            .strip_prefix("https://github.com/")
            .or_else(|| input.strip_prefix("http://github.com/"))
            .or_else(|| input.strip_prefix("github.com/"));
        if let Some(rest) = rest {
            return Self::parse_path(rest);
        }

        // `o/r#123` — exactly one `/` in the slug keeps plain paths (`a/b/c`)
        // and shell fragments out.
        if let Some((slug, number)) = input.split_once('#') {
            let (owner, repo) = slug.split_once('/')?;
            if owner.is_empty() || repo.is_empty() || repo.contains('/') {
                return None;
            }
            return Some(Self {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                number: number.parse().ok()?,
            });
        }

        // Bare `o/r/pull/123` (URL form without the host).
        if input.contains("/pull/") {
            return Self::parse_path(input);
        }
        None
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
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number: digits.parse().ok()?,
        })
    }

    /// `owner/repo`, the form `gh --repo` takes.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Short display label: `repo#number`.
    pub fn label(&self) -> String {
        format!("{}#{}", self.repo, self.number)
    }
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
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &spec.number.to_string(),
            "--repo",
            &spec.slug(),
            "--json",
            "number,title,author,state,baseRefName,headRefName,additions,deletions,changedFiles,url",
            "--jq",
            JQ,
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .context("failed to run `gh` — is the GitHub CLI installed and on PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh pr view exited with {}: {}", output.status, stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
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
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &spec.number.to_string(),
            "--repo",
            &spec.slug(),
            "--json",
            "commits",
            "--jq",
            JQ,
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .context("failed to run `gh` — is the GitHub CLI installed and on PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "gh pr view --json commits exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
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
pub fn pr_commit_store(commits: &[PrCommit]) -> (CommitStore, GraphLayout) {
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
        is_working_copy: true,
        bookmarks: Vec::new(),
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
            is_working_copy: false,
            bookmarks: Vec::new(),
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
    let mut graph = GraphLayoutBuilder::new();
    for frame in &frames {
        graph.push(frame, &[]);
    }

    let mut builder = CommitStoreBuilder::with_capacity(rows.len());
    for row in rows {
        builder.push(row);
    }
    (builder.finish(), graph.finish())
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
            "repos/{}/commits/{oid}?per_page=100&page={page}",
            spec.slug()
        );
        let output = Command::new("gh")
            .args(["api", &path])
            .stdin(Stdio::null())
            .output()
            .await
            .context("failed to run `gh` — is the GitHub CLI installed and on PATH?")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "gh api commit diff exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }
        let commit: ApiCommit = serde_json::from_slice(&output.stdout)
            .context("failed to parse the commit API response")?;
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
                }
            });
        }
        if totals.is_none() {
            totals = commit.stats.map(|stats| (stats.additions, stats.deletions));
        }
        let last = commit.files.len() < 100;
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
            "repos/{}/pulls/{}/files?per_page=100&page={page}",
            spec.slug(),
            spec.number
        );
        let output = Command::new("gh")
            .args(["api", &path])
            .stdin(Stdio::null())
            .output()
            .await
            .context("failed to run `gh` — is the GitHub CLI installed and on PATH?")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "gh api pull-request files exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }
        let files: Vec<ApiPrFile> = serde_json::from_slice(&output.stdout)
            .context("failed to parse the pull-request files API response")?;
        // A short (or empty) page is the last one; GitHub also simply stops
        // listing past its 3,000-file cap.
        let last = files.len() < 100;
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
    let mut child = Command::new("gh")
        .args([
            "pr",
            "diff",
            &spec.number.to_string(),
            "--repo",
            &spec.slug(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run `gh` — is the GitHub CLI installed and on PATH?")?;

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
    let mut lines = BufReader::with_capacity(256 * 1024, stdout).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("failed to read gh pr diff output")?
    {
        if let Some(file) = parser.push_line(&line) {
            on_file(file);
        }
    }
    if let Some(file) = parser.finish() {
        on_file(file);
    }

    let status = child.wait().await.context("failed to await gh")?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    if !status.success() {
        bail!(
            "gh pr diff exited with {}: {}",
            status,
            stderr_text.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_urls() {
        let expected = Some(PrSpec {
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
        assert_eq!(
            PrSpec::parse("anthropics/claude-code/pull/123"),
            expected
        );
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

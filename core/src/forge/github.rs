//! GitHub, through the `gh` CLI (on the PATH and signed in to the host:
//! github.com or a GitHub Enterprise Server).
//!
//! A pull request's diff **streams**: `gh pr diff` stdout is parsed
//! incrementally and each completed file is handed back as soon as its last
//! hunk is read, so a huge PR paints progressively while the download is
//! still running. Lists and writes go through `gh pr list --json` and
//! `gh api`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::cli::{Cli, configured_hosts};
use super::{
    CheckState, Checks, CreatePr, ForgeError, ForgeFuture, ForgeKind, ForgePr, ForgeProvider,
    ForgeRemote, ForgeResult, OnFile, PrCommit, PrInfo, PrState, PullRequest, ReviewDecision,
    UpdatePr, latest_per_branch, patch_to_file,
};
use crate::diff_parse::DiffStreamParser;
use crate::model::{DiffDocument, DiffFile, DiffFileStatus, RevisionDetails, SignatureInfo};
use crate::process::NETWORK_TIMEOUT;
use crate::repo::CancelFlag;

/// GitHub's REST API caps `per_page` at 100; we always request the max and
/// page until a short page signals the end.
const PAGE_SIZE: usize = 100;

/// How many of a project's most recent pull requests a refresh looks through
/// for its branches.
const LIST_LIMIT: &str = "200";

pub struct GitHub {
    cli: Cli,
    hosts: OnceLock<Vec<String>>,
}

impl GitHub {
    /// GitHub through `program` (`gh`, or a test's fake).
    pub fn new(program: PathBuf) -> Self {
        Self {
            cli: Cli {
                kind: ForgeKind::GitHub,
                program,
            },
            hosts: OnceLock::new(),
        }
    }

    /// Whether `host` is GitHub's: github.com, a host named like one
    /// (`github.acme.com`), or one `gh` is signed in to.
    fn hosts_github(&self, host: &str) -> bool {
        host == "github.com"
            || host.split('.').any(|label| label == "github")
            || self
                .hosts
                .get_or_init(|| configured_hosts(ForgeKind::GitHub))
                .iter()
                .any(|known| known == host)
    }

    async fn list(&self, remote: &ForgeRemote) -> ForgeResult<Vec<ListedPr>> {
        let repo = repo_arg(&remote.host, &remote.slug());
        self.cli
            .json(
                &remote.host,
                &[
                    "pr",
                    "list",
                    "--repo",
                    &repo,
                    "--state",
                    "all",
                    "--limit",
                    LIST_LIMIT,
                    "--json",
                    "number,url,title,headRefName,baseRefName,headRefOid,state,isDraft,\
                     reviewDecision,isCrossRepository",
                ],
                None,
            )
            .await
    }

    async fn rollup(&self, remote: &ForgeRemote, sha: &str) -> ForgeResult<Checks> {
        const QUERY: &str = "query($owner:String!,$name:String!,$sha:GitObjectID!){\
            repository(owner:$owner,name:$name){object(oid:$sha){\
            ... on Commit{statusCheckRollup{state}}}}}";
        let stdout = self
            .cli
            .run(
                &remote.host,
                &[
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={QUERY}"),
                    "-f",
                    &format!("owner={}", remote.owner),
                    "-f",
                    &format!("name={}", remote.name),
                    "-f",
                    &format!("sha={sha}"),
                    "--jq",
                    ".data.repository.object.statusCheckRollup.state",
                ],
                None,
            )
            .await?;
        let state = match String::from_utf8_lossy(&stdout).trim() {
            "SUCCESS" => CheckState::Success,
            "FAILURE" | "ERROR" => CheckState::Failure,
            "PENDING" | "EXPECTED" => CheckState::Pending,
            _ => CheckState::None,
        };
        Ok(Checks {
            state,
            url: (state != CheckState::None).then(|| format!("{}/commit/{sha}", remote.web_url)),
        })
    }

    /// `gh api` with `fields` as `-f key=value` (strings) and `typed` as
    /// `-F key=value` (booleans), answered with a pull request.
    async fn write_pr(
        &self,
        remote: &ForgeRemote,
        method: &str,
        path: &str,
        fields: &[(&str, &str)],
        typed: &[(&str, &str)],
    ) -> ForgeResult<PullRequest> {
        let mut args: Vec<String> = vec!["api".into(), "-X".into(), method.into(), path.into()];
        for (flag, pairs) in [("-f", fields), ("-F", typed)] {
            for (key, value) in pairs {
                args.push(flag.into());
                args.push(format!("{key}={value}"));
            }
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let pr: RestPr = self.cli.json(&remote.host, &args, None).await?;
        Ok(pr.into())
    }

    /// Run `gh` for `pr`, with a not-found naming the pull request.
    async fn run_for(
        &self,
        pr: &ForgePr,
        args: &[&str],
        cancel: Option<&CancelFlag>,
    ) -> ForgeResult<Vec<u8>> {
        self.cli
            .run(&pr.host_label(), args, cancel)
            .await
            .map_err(|error| about(error, pr))
    }

    async fn info(&self, pr: &ForgePr) -> ForgeResult<PrInfo> {
        // `\u{1f}` (the unit separator) frames the fields, so titles with
        // tabs or quotes can't break them; titles can't hold newlines.
        const JQ: &str = r#"[(.number|tostring), .title, .author.login, (if .isDraft and .state == "OPEN" then "DRAFT" else .state end), .baseRefName, .headRefName, (.additions|tostring), (.deletions|tostring), (.changedFiles|tostring), .url] | join("\u001f")"#;
        let stdout = self
            .run_for(
                pr,
                &[
                    "pr",
                    "view",
                    &pr.number.to_string(),
                    "--repo",
                    &repo_arg(&pr.host_label(), &pr.slug()),
                    "--json",
                    "number,title,author,state,isDraft,baseRefName,headRefName,additions,\
                     deletions,changedFiles,url",
                    "--jq",
                    JQ,
                ],
                None,
            )
            .await?;
        let stdout = String::from_utf8_lossy(&stdout);
        let mut parts = stdout.trim_end_matches('\n').split('\u{1f}');
        let mut next = || parts.next().unwrap_or("").to_owned();
        Ok(PrInfo {
            number: next().parse().unwrap_or(pr.number),
            title: next(),
            author: next(),
            state: next().to_lowercase(),
            base_ref: next(),
            head_ref: next(),
            totals: next().parse().ok().zip(next().parse().ok()),
            changed_files: next().parse().unwrap_or(0),
            url: next(),
        })
    }

    async fn pr_commits(&self, pr: &ForgePr) -> ForgeResult<Vec<PrCommit>> {
        const JQ: &str = r#".commits[] | [.oid, (.authors[0].login // ""), .messageHeadline, (.authoredDate // "")] | join("\u001f")"#;
        let stdout = self
            .run_for(
                pr,
                &[
                    "pr",
                    "view",
                    &pr.number.to_string(),
                    "--repo",
                    &repo_arg(&pr.host_label(), &pr.slug()),
                    "--json",
                    "commits",
                    "--jq",
                    JQ,
                ],
                None,
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
                    authored_millis: crate::history::parse_timestamp_millis(&next()),
                }
            })
            .collect())
    }

    /// One commit's diff through the commits REST endpoint, paged; the
    /// per-file entries match the files API's, so the same lowering applies.
    async fn one_commit_diff(
        &self,
        pr: &ForgePr,
        oid: &str,
        cancel: &CancelFlag,
    ) -> ForgeResult<(DiffDocument, Option<RevisionDetails>)> {
        let mut files = Vec::new();
        let mut details: Option<RevisionDetails> = None;
        let mut totals: Option<(usize, usize)> = None;
        let mut page = 1usize;
        loop {
            let path = format!(
                "repos/{}/commits/{oid}?per_page={PAGE_SIZE}&page={page}",
                pr.slug()
            );
            let stdout = self.run_for(pr, &["api", &path], Some(cancel)).await?;
            let commit: ApiCommit = serde_json::from_slice(&stdout).map_err(|error| {
                ForgeError::Failed(format!("couldn't read the commit API response: {error}"))
            })?;
            if details.is_none() {
                details = commit.commit.map(|meta| {
                    let author = meta.author.unwrap_or_default();
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
            let last = commit.files.len() < PAGE_SIZE;
            files.extend(commit.files.into_iter().map(ApiPrFile::into_file));
            if last {
                break;
            }
            page += 1;
        }
        // Prefer the commit's own stats over summed files: the API zeroes
        // an oversized blob's per-file counts.
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

    /// Page through `GET /repos/{owner}/{repo}/pulls/{n}/files`, 100 files a
    /// request.
    async fn stream_files_api(
        &self,
        pr: &ForgePr,
        cancel: &CancelFlag,
        on_file: &mut OnFile<'_>,
    ) -> ForgeResult<()> {
        let mut page = 1usize;
        loop {
            let path = format!(
                "repos/{}/pulls/{}/files?per_page={PAGE_SIZE}&page={page}",
                pr.slug(),
                pr.number
            );
            let stdout = self.run_for(pr, &["api", &path], Some(cancel)).await?;
            let files: Vec<ApiPrFile> = serde_json::from_slice(&stdout).map_err(|error| {
                ForgeError::Failed(format!(
                    "couldn't read the pull-request files API response: {error}"
                ))
            })?;
            // A short (or empty) page is the last one; GitHub also simply
            // stops listing past its 3,000-file cap.
            let last = files.len() < PAGE_SIZE;
            for file in files {
                on_file(file.into_file());
            }
            if last {
                return Ok(());
            }
            page += 1;
        }
    }

    /// The `gh pr diff` path: one process, unified diff streamed off its
    /// stdout. A stream that sends nothing for [`NETWORK_TIMEOUT`], or whose
    /// `cancel` is raised, fails, and dropping the child kills it.
    async fn stream_pr_diff(
        &self,
        pr: &ForgePr,
        cancel: &CancelFlag,
        on_file: &mut OnFile<'_>,
    ) -> ForgeResult<()> {
        let host = pr.host_label();
        let mut child = self
            .cli
            .command(&host)
            .args([
                "pr",
                "diff",
                &pr.number.to_string(),
                "--repo",
                &repo_arg(&host, &pr.slug()),
            ])
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| self.cli.wait_error(&host, error))?;
        let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(ForgeError::Failed("gh's output was unavailable".to_owned()));
        };
        // Drain stderr concurrently so a chatty gh can't fill its pipe and
        // deadlock while we're blocked reading stdout.
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf).await;
            buf
        });

        let mut parser = DiffStreamParser::default();
        // Split on bytes and decode each line lossily: `lines()` validates
        // UTF-8 and would abort the stream halfway through the first PR
        // touching a latin-1 file. The trailing `\r` is dropped the way
        // `str::lines` does it, so a CRLF diff parses like any other.
        let mut lines = BufReader::with_capacity(256 * 1024, stdout).split(b'\n');
        loop {
            let next =
                crate::process::bounded(lines.next_segment(), Some(NETWORK_TIMEOUT), Some(cancel))
                    .await
                    .map_err(|error| self.cli.wait_error(&host, error))?
                    .map_err(|error| {
                        ForgeError::Failed(format!("couldn't read gh pr diff's output: {error}"))
                    })?;
            let Some(raw) = next else { break };
            let raw = raw.strip_suffix(b"\r").unwrap_or(&raw);
            if let Some(file) = parser.push_line(&String::from_utf8_lossy(raw)) {
                on_file(file);
            }
        }
        if let Some(file) = parser.finish() {
            on_file(file);
        }

        let status = child
            .wait()
            .await
            .map_err(|error| ForgeError::Failed(format!("couldn't wait for gh: {error}")))?;
        let stderr_text = stderr_task.await.unwrap_or_default();
        if !status.success() {
            if is_diff_too_large(&stderr_text) {
                return Err(ForgeError::Failed(TOO_LARGE.to_owned()));
            }
            return Err(about(self.cli.failure(&host, &stderr_text, ""), pr));
        }
        Ok(())
    }
}

/// What [`GitHub::stream_pr_diff`] fails with when GitHub refuses to serve
/// the unified diff, so the caller reroutes.
const TOO_LARGE: &str = "diff too large";

/// `host/owner/name`, the form `gh --repo` takes.
fn repo_arg(host: &str, slug: &str) -> String {
    format!("{host}/{slug}")
}

/// A not-found from a pull request's own call names it.
fn about(error: ForgeError, pr: &ForgePr) -> ForgeError {
    match error {
        ForgeError::NotFound { kind, host, .. } => ForgeError::NotFound {
            kind,
            host,
            what: pr.reference(),
        },
        other => other,
    }
}

/// GitHub's "Sorry, the diff exceeded the maximum number of files" refusal
/// (HTTP 406 / `PullRequest.diff too_large`), as relayed through gh's stderr.
fn is_diff_too_large(text: &str) -> bool {
    text.contains("HTTP 406") || text.contains("too_large") || text.contains("exceeded the maximum")
}

impl ForgeProvider for GitHub {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn claims(&self, remote_url: &str) -> Option<ForgeRemote> {
        let (host, owner, name) = super::parse_remote_url(remote_url)?;
        (!owner.contains('/') && self.hosts_github(&host))
            .then(|| ForgeRemote::new(ForgeKind::GitHub, &host, &owner, &name))
    }

    fn pull_requests(
        &self,
        remote: ForgeRemote,
        branches: Vec<String>,
    ) -> ForgeFuture<'_, Vec<PullRequest>> {
        Box::pin(async move {
            let listed = self.list(&remote).await?;
            let prs = listed
                .into_iter()
                .filter(|pr| !pr.is_cross_repository)
                .map(PullRequest::from);
            Ok(latest_per_branch(prs, &branches))
        })
    }

    fn checks(&self, remote: ForgeRemote, sha: String) -> ForgeFuture<'_, Checks> {
        Box::pin(async move { self.rollup(&remote, &sha).await })
    }

    fn create_pull_request(
        &self,
        remote: ForgeRemote,
        request: CreatePr,
    ) -> ForgeFuture<'_, PullRequest> {
        Box::pin(async move {
            let path = format!("repos/{}/pulls", remote.slug());
            self.write_pr(
                &remote,
                "POST",
                &path,
                &[
                    ("head", &request.head),
                    ("base", &request.base),
                    ("title", &request.title),
                    ("body", &request.body),
                ],
                &[("draft", if request.draft { "true" } else { "false" })],
            )
            .await
        })
    }

    fn update_pull_request(
        &self,
        remote: ForgeRemote,
        number: u64,
        patch: UpdatePr,
    ) -> ForgeFuture<'_, PullRequest> {
        Box::pin(async move {
            let path = format!("repos/{}/pulls/{number}", remote.slug());
            let fields: Vec<(&str, &str)> = [
                ("base", &patch.base),
                ("title", &patch.title),
                ("body", &patch.body),
            ]
            .into_iter()
            .filter_map(|(key, value)| value.as_deref().map(|value| (key, value)))
            .collect();
            self.write_pr(&remote, "PATCH", &path, &fields, &[]).await
        })
    }

    fn view(&self, pr: ForgePr) -> ForgeFuture<'_, PrInfo> {
        Box::pin(async move { self.info(&pr).await })
    }

    fn commits(&self, pr: ForgePr) -> ForgeFuture<'_, Vec<PrCommit>> {
        Box::pin(async move { self.pr_commits(&pr).await })
    }

    fn commit_diff(
        &self,
        pr: ForgePr,
        oid: String,
        cancel: CancelFlag,
    ) -> ForgeFuture<'_, (DiffDocument, Option<RevisionDetails>)> {
        Box::pin(async move { self.one_commit_diff(&pr, &oid, &cancel).await })
    }

    /// Prefers `gh pr diff` (one request, true streaming); when GitHub
    /// refuses to serve the unified diff outright (HTTP 406, more than 300
    /// changed files), pages through the REST files endpoint, which serves
    /// per-file patches up to GitHub's 3,000-file listing cap.
    fn stream_diff<'a>(
        &'a self,
        pr: ForgePr,
        cancel: CancelFlag,
        mut on_file: OnFile<'a>,
    ) -> ForgeFuture<'a, ()> {
        Box::pin(async move {
            match self.stream_pr_diff(&pr, &cancel, &mut on_file).await {
                // Only the too-large refusal reroutes: an auth or network
                // failure would fail the files API identically. It arrives
                // before any diff bytes, so nothing is emitted twice.
                Err(ForgeError::Failed(message)) if message == TOO_LARGE => {
                    self.stream_files_api(&pr, &cancel, &mut on_file).await
                }
                other => other,
            }
        })
    }
}

/// One row of `gh pr list --json`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedPr {
    number: u64,
    url: String,
    title: String,
    head_ref_name: String,
    base_ref_name: String,
    #[serde(default)]
    head_ref_oid: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    is_cross_repository: bool,
}

impl From<ListedPr> for PullRequest {
    fn from(pr: ListedPr) -> Self {
        let state = match pr.state.as_str() {
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ if pr.is_draft => PrState::Draft,
            _ => PrState::Open,
        };
        let review = match pr.review_decision.as_deref() {
            Some("APPROVED") => Some(ReviewDecision::Approved),
            Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
            Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
            _ => None,
        };
        Self {
            number: pr.number,
            url: pr.url,
            title: pr.title,
            head: pr.head_ref_name,
            base: pr.base_ref_name,
            head_sha: pr.head_ref_oid,
            state,
            review,
        }
    }
}

/// A pull request as the REST API answers a create or an update.
#[derive(serde::Deserialize)]
struct RestPr {
    number: u64,
    html_url: String,
    title: String,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    merged_at: Option<String>,
    head: RestRef,
    base: RestRef,
}

#[derive(serde::Deserialize)]
struct RestRef {
    #[serde(rename = "ref")]
    name: String,
    #[serde(default)]
    sha: String,
}

impl From<RestPr> for PullRequest {
    fn from(pr: RestPr) -> Self {
        let state = match (pr.state.as_str(), pr.merged_at.is_some(), pr.draft) {
            (_, true, _) => PrState::Merged,
            ("closed", ..) => PrState::Closed,
            (_, _, true) => PrState::Draft,
            _ => PrState::Open,
        };
        Self {
            number: pr.number,
            url: pr.html_url,
            title: pr.title,
            head: pr.head.name,
            base: pr.base.name,
            head_sha: pr.head.sha,
            state,
            review: None,
        }
    }
}

/// Wire shape of `GET /repos/{owner}/{repo}/commits/{sha}`: the same
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

#[derive(serde::Deserialize, Default)]
struct ApiCommitSignature {
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    date: String,
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

impl ApiPrFile {
    fn into_file(self) -> DiffFile {
        let status = match self.status.as_str() {
            "added" => DiffFileStatus::Added,
            "removed" => DiffFileStatus::Deleted,
            // The REST vocabulary also has "copied"; rendering it as a rename
            // shows the source path, which is the closest fit we have.
            "renamed" | "copied" => DiffFileStatus::Renamed,
            _ => DiffFileStatus::Modified,
        };
        let mut file = patch_to_file(
            self.filename,
            self.previous_filename,
            status,
            self.patch.as_deref(),
        );
        // The API's counts cover the whole file even when the patch was
        // omitted; trust them over the parsed rows.
        file.additions = self.additions;
        file.deletions = self.deletions;
        file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_file_lowering_parses_patch_and_statuses() {
        let file = ApiPrFile {
            filename: "src/new name.rs".to_owned(),
            previous_filename: Some("src/old name.rs".to_owned()),
            status: "renamed".to_owned(),
            additions: 1,
            deletions: 1,
            patch: Some("@@ -10,2 +10,2 @@\n context\n-old line\n+new line".to_owned()),
        }
        .into_file();
        // Paths with spaces survive (no `diff --git` header round-trip).
        assert_eq!(file.path, "src/new name.rs");
        assert_eq!(file.old_path.as_deref(), Some("src/old name.rs"));
        assert_eq!(file.status, DiffFileStatus::Renamed);
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines[0].old_line, Some(10));
        assert_eq!(file.hunks[0].lines[0].new_line, Some(10));
        assert_eq!((file.additions, file.deletions), (1, 1));

        // No patch (binary / oversized): the row survives with its counts.
        let binary = ApiPrFile {
            filename: "big.bin".to_owned(),
            previous_filename: None,
            status: "added".to_owned(),
            additions: 0,
            deletions: 0,
            patch: None,
        }
        .into_file();
        assert_eq!(binary.status, DiffFileStatus::Added);
        assert_eq!(binary.hunks.len(), 1);
        assert!(binary.hunks[0].header.contains("diff unavailable"));
    }

    #[test]
    fn claims_github_hosts_only() {
        let github = GitHub::new("gh-absent".into());
        assert!(github.claims("git@github.com:team/app.git").is_some());
        assert!(github.claims("https://github.acme.com/team/app").is_some());
        assert!(github.claims("git@gitlab.com:team/app.git").is_none());
        // GitHub projects have no subgroups.
        assert!(github.claims("git@github.com:team/sub/app.git").is_none());
    }

    #[test]
    fn too_large_is_recognised() {
        assert!(is_diff_too_large(
            "HTTP 406: Sorry, the diff exceeded the maximum number of files (300)."
        ));
        assert!(!is_diff_too_large("HTTP 404: Not Found"));
    }
}

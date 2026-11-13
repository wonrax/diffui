//! GitLab, through the `glab` CLI's `glab api` (on the PATH and signed in to
//! the host: gitlab.com, or a self-hosted GitLab glab is configured for).
//! Projects are addressed by their URL-encoded path, so a project in a
//! subgroup works like any other.

use std::path::PathBuf;
use std::sync::OnceLock;

use super::cli::{Cli, configured_hosts};
use super::{
    CheckState, Checks, CreatePr, ForgeError, ForgeFuture, ForgeKind, ForgePr, ForgeProvider,
    ForgeRemote, ForgeResult, OnFile, PrCommit, PrInfo, PrState, PullRequest, UpdatePr,
    latest_per_branch, patch_to_file,
};
use crate::model::{DiffDocument, DiffFile, DiffFileStatus, RevisionDetails, SignatureInfo};
use crate::repo::CancelFlag;

/// GitLab's largest page.
const PAGE_SIZE: usize = 100;

pub struct GitLab {
    cli: Cli,
    hosts: OnceLock<Vec<String>>,
}

impl GitLab {
    /// GitLab through `program` (`glab`, or a test's fake).
    pub fn new(program: PathBuf) -> Self {
        Self {
            cli: Cli {
                kind: ForgeKind::GitLab,
                program,
            },
            hosts: OnceLock::new(),
        }
    }

    /// Whether `host` is a GitLab: gitlab.com, a host named like one
    /// (`gitlab.acme.org`), or one `glab` is configured for.
    fn hosts_gitlab(&self, host: &str) -> bool {
        host == "gitlab.com"
            || host.split('.').any(|label| label == "gitlab")
            || self
                .hosts
                .get_or_init(|| configured_hosts(ForgeKind::GitLab))
                .iter()
                .any(|known| known == host)
    }

    /// `glab api` on `host`, its JSON answer parsed.
    async fn api<T: serde::de::DeserializeOwned>(
        &self,
        host: &str,
        args: &[&str],
        cancel: Option<&CancelFlag>,
    ) -> ForgeResult<T> {
        let mut full = vec!["api", "--hostname", host];
        full.extend_from_slice(args);
        self.cli.json(host, &full, cancel).await
    }

    /// `glab api` for one merge request's project, a not-found naming it.
    async fn api_for<T: serde::de::DeserializeOwned>(
        &self,
        pr: &ForgePr,
        path: &str,
        cancel: Option<&CancelFlag>,
    ) -> ForgeResult<T> {
        self.api(&pr.host_label(), &[path], cancel)
            .await
            .map_err(|error| match error {
                ForgeError::NotFound { kind, host, .. } => ForgeError::NotFound {
                    kind,
                    host,
                    what: pr.reference(),
                },
                other => other,
            })
    }

    /// Write a merge request: `fields` as `-f key=value`.
    async fn write_mr(
        &self,
        remote: &ForgeRemote,
        method: &str,
        path: &str,
        fields: &[(&str, &str)],
    ) -> ForgeResult<PullRequest> {
        let mut args: Vec<String> = vec![path.into(), "-X".into(), method.into()];
        for (key, value) in fields {
            args.push("-f".into());
            args.push(format!("{key}={value}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mr: Mr = self.api(&remote.host, &args, None).await?;
        Ok(mr.into())
    }

    /// Every page of a per-file diff listing at `path` (which takes
    /// `per_page` / `page`), handed over a file at a time.
    async fn stream_diffs(
        &self,
        pr: &ForgePr,
        path: &str,
        cancel: &CancelFlag,
        on_file: &mut (dyn FnMut(DiffFile) + Send),
    ) -> ForgeResult<()> {
        let mut page = 1usize;
        loop {
            let paged = format!("{path}?per_page={PAGE_SIZE}&page={page}");
            let files: Vec<MrDiff> = self.api_for(pr, &paged, Some(cancel)).await?;
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
}

/// `projects/<path>`, the project's path URL-encoded as the API wants it.
fn project(owner: &str, name: &str) -> String {
    let path = format!("{owner}/{name}");
    let mut encoded = String::with_capacity(path.len() + 8);
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("projects/{encoded}")
}

fn pr_project(pr: &ForgePr) -> String {
    project(&pr.owner, &pr.name)
}

impl ForgeProvider for GitLab {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitLab
    }

    fn claims(&self, remote_url: &str) -> Option<ForgeRemote> {
        let (host, owner, name) = super::parse_remote_url(remote_url)?;
        self.hosts_gitlab(&host)
            .then(|| ForgeRemote::new(ForgeKind::GitLab, &host, &owner, &name))
    }

    fn pull_requests(
        &self,
        remote: ForgeRemote,
        branches: Vec<String>,
    ) -> ForgeFuture<'_, Vec<PullRequest>> {
        Box::pin(async move {
            let path = format!(
                "{}/merge_requests?state=all&order_by=updated_at&per_page={PAGE_SIZE}",
                project(&remote.owner, &remote.name)
            );
            let listed: Vec<Mr> = self.api(&remote.host, &[&path], None).await?;
            let prs = listed
                .into_iter()
                // A fork's merge request names the fork's branch.
                .filter(|mr| mr.source_project_id == mr.target_project_id)
                .map(PullRequest::from);
            Ok(latest_per_branch(prs, &branches))
        })
    }

    fn checks(&self, remote: ForgeRemote, sha: String) -> ForgeFuture<'_, Checks> {
        Box::pin(async move {
            let path = format!(
                "{}/repository/commits/{sha}",
                project(&remote.owner, &remote.name)
            );
            let commit: Commit = self.api(&remote.host, &[&path], None).await?;
            let Some(pipeline) = commit.last_pipeline else {
                return Ok(Checks {
                    state: CheckState::None,
                    url: None,
                });
            };
            let state = match pipeline.status.as_str() {
                "success" => CheckState::Success,
                "failed" | "canceled" => CheckState::Failure,
                "created"
                | "waiting_for_resource"
                | "preparing"
                | "pending"
                | "running"
                | "scheduled" => CheckState::Pending,
                _ => CheckState::None,
            };
            Ok(Checks {
                state,
                url: pipeline.web_url,
            })
        })
    }

    fn create_pull_request(
        &self,
        remote: ForgeRemote,
        request: CreatePr,
    ) -> ForgeFuture<'_, PullRequest> {
        Box::pin(async move {
            let path = format!("{}/merge_requests", project(&remote.owner, &remote.name));
            // GitLab marks a draft by its title.
            let title = if request.draft {
                format!("Draft: {}", request.title)
            } else {
                request.title
            };
            self.write_mr(
                &remote,
                "POST",
                &path,
                &[
                    ("source_branch", &request.head),
                    ("target_branch", &request.base),
                    ("title", &title),
                    ("description", &request.body),
                ],
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
            let path = format!(
                "{}/merge_requests/{number}",
                project(&remote.owner, &remote.name)
            );
            let fields: Vec<(&str, &str)> = [
                ("target_branch", &patch.base),
                ("title", &patch.title),
                ("description", &patch.body),
            ]
            .into_iter()
            .filter_map(|(key, value)| value.as_deref().map(|value| (key, value)))
            .collect();
            self.write_mr(&remote, "PUT", &path, &fields).await
        })
    }

    fn view(&self, pr: ForgePr) -> ForgeFuture<'_, PrInfo> {
        Box::pin(async move {
            let path = format!("{}/merge_requests/{}", pr_project(&pr), pr.number);
            let mr: Mr = self.api_for(&pr, &path, None).await?;
            let state = PullRequest::from(mr.clone()).state;
            Ok(PrInfo {
                number: mr.iid,
                author: mr.author.map(|author| author.username).unwrap_or_default(),
                state: state.label().to_owned(),
                base_ref: mr.target_branch,
                head_ref: mr.source_branch,
                totals: None,
                changed_files: mr
                    .changes_count
                    .as_deref()
                    .and_then(|count| count.trim_end_matches('+').parse().ok())
                    .unwrap_or(0),
                url: mr.web_url,
                title: mr.title,
            })
        })
    }

    fn commits(&self, pr: ForgePr) -> ForgeFuture<'_, Vec<PrCommit>> {
        Box::pin(async move {
            let mut commits = Vec::new();
            let mut page = 1usize;
            loop {
                let path = format!(
                    "{}/merge_requests/{}/commits?per_page={PAGE_SIZE}&page={page}",
                    pr_project(&pr),
                    pr.number
                );
                let listed: Vec<Commit> = self.api_for(&pr, &path, None).await?;
                let last = listed.len() < PAGE_SIZE;
                commits.extend(listed.into_iter().map(|commit| {
                    PrCommit {
                        authored_millis: commit
                            .authored_date
                            .as_deref()
                            .and_then(crate::history::parse_timestamp_millis),
                        oid: commit.id,
                        author: commit.author_name,
                        headline: commit.title,
                    }
                }));
                if last {
                    break;
                }
                page += 1;
            }
            // GitLab lists newest first; the tab wants the forge's oldest
            // first, like GitHub's.
            commits.reverse();
            Ok(commits)
        })
    }

    fn commit_diff(
        &self,
        pr: ForgePr,
        oid: String,
        cancel: CancelFlag,
    ) -> ForgeFuture<'_, (DiffDocument, Option<RevisionDetails>)> {
        Box::pin(async move {
            let base = format!("{}/repository/commits/{oid}", pr_project(&pr));
            let commit: Commit = self.api_for(&pr, &base, Some(&cancel)).await?;
            let mut files = Vec::new();
            self.stream_diffs(&pr, &format!("{base}/diff"), &cancel, &mut |file| {
                files.push(file)
            })
            .await?;
            let (total_additions, total_deletions) = commit
                .stats
                .map(|stats| (stats.additions, stats.deletions))
                .unwrap_or_else(|| {
                    (
                        files.iter().map(|file| file.additions).sum(),
                        files.iter().map(|file| file.deletions).sum(),
                    )
                });
            let details = RevisionDetails {
                commit_id: oid,
                change_id: None,
                bookmarks: Vec::new(),
                author: SignatureInfo {
                    name: commit.author_name,
                    email: commit.author_email,
                    timestamp: commit.authored_date,
                },
                committer: None,
                signature: None,
                description: commit.message.unwrap_or(commit.title),
                parents: Vec::new(),
            };
            Ok((
                DiffDocument {
                    files,
                    total_additions,
                    total_deletions,
                    range: None,
                },
                Some(details),
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
            let path = format!("{}/merge_requests/{}/diffs", pr_project(&pr), pr.number);
            self.stream_diffs(&pr, &path, &cancel, &mut on_file).await
        })
    }
}

/// A merge request, as the API returns one.
#[derive(serde::Deserialize, Clone)]
struct Mr {
    iid: u64,
    web_url: String,
    title: String,
    source_branch: String,
    target_branch: String,
    #[serde(default)]
    sha: Option<String>,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    work_in_progress: bool,
    #[serde(default)]
    source_project_id: Option<u64>,
    #[serde(default)]
    target_project_id: Option<u64>,
    #[serde(default)]
    author: Option<User>,
    #[serde(default)]
    changes_count: Option<String>,
}

#[derive(serde::Deserialize, Clone)]
struct User {
    username: String,
}

impl From<Mr> for PullRequest {
    fn from(mr: Mr) -> Self {
        let state = match mr.state.as_str() {
            "merged" => PrState::Merged,
            "closed" | "locked" => PrState::Closed,
            _ if mr.draft || mr.work_in_progress => PrState::Draft,
            _ => PrState::Open,
        };
        Self {
            number: mr.iid,
            url: mr.web_url,
            title: mr.title,
            head: mr.source_branch,
            base: mr.target_branch,
            head_sha: mr.sha.unwrap_or_default(),
            state,
            review: None,
        }
    }
}

/// A commit, from the commits endpoints.
#[derive(serde::Deserialize)]
struct Commit {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    author_name: String,
    #[serde(default)]
    author_email: String,
    #[serde(default)]
    authored_date: Option<String>,
    #[serde(default)]
    stats: Option<Stats>,
    #[serde(default)]
    last_pipeline: Option<Pipeline>,
}

#[derive(serde::Deserialize)]
struct Stats {
    additions: usize,
    deletions: usize,
}

#[derive(serde::Deserialize)]
struct Pipeline {
    status: String,
    #[serde(default)]
    web_url: Option<String>,
}

/// One file of a diff listing.
#[derive(serde::Deserialize)]
struct MrDiff {
    old_path: String,
    new_path: String,
    #[serde(default)]
    new_file: bool,
    #[serde(default)]
    renamed_file: bool,
    #[serde(default)]
    deleted_file: bool,
    #[serde(default)]
    diff: String,
}

impl MrDiff {
    fn into_file(self) -> DiffFile {
        let status = if self.new_file {
            DiffFileStatus::Added
        } else if self.deleted_file {
            DiffFileStatus::Deleted
        } else if self.renamed_file {
            DiffFileStatus::Renamed
        } else {
            DiffFileStatus::Modified
        };
        let old_path = self.renamed_file.then_some(self.old_path);
        patch_to_file(self.new_path, old_path, status, Some(&self.diff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_are_addressed_by_their_encoded_path() {
        assert_eq!(
            project("group/sub", "my.app"),
            "projects/group%2Fsub%2Fmy.app"
        );
    }

    #[test]
    fn claims_gitlab_hosts_with_subgroups() {
        let gitlab = GitLab::new("glab-absent".into());
        let remote = gitlab
            .claims("git@gitlab.com:group/sub/app.git")
            .expect("claimed");
        assert_eq!(remote.slug(), "group/sub/app");
        assert_eq!(remote.web_url, "https://gitlab.com/group/sub/app");
        assert!(gitlab.claims("git@github.com:team/app.git").is_none());
    }

    #[test]
    fn diff_entries_lower_with_their_counts() {
        let file = MrDiff {
            old_path: "a.rs".to_owned(),
            new_path: "b.rs".to_owned(),
            new_file: false,
            renamed_file: true,
            deleted_file: false,
            diff: "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n+more\n".to_owned(),
        }
        .into_file();
        assert_eq!(file.status, DiffFileStatus::Renamed);
        assert_eq!(file.old_path.as_deref(), Some("a.rs"));
        assert_eq!((file.additions, file.deletions), (2, 1));
    }
}

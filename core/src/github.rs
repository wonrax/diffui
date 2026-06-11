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

use crate::diff_parse::DiffStreamParser;
use crate::model::{DiffFile, RevisionDetails, SignatureInfo};

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

/// Stream the PR's unified diff, invoking `on_file` for each completed
/// (already-highlighted) file as it parses off the pipe. Returns once the
/// stream ends; a non-zero `gh` exit fails with its captured stderr.
pub async fn stream_pr_diff(spec: &PrSpec, mut on_file: impl FnMut(DiffFile)) -> Result<()> {
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

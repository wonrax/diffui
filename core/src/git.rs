use std::{
    collections::HashMap,
    ffi::OsString,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use jj_lib::graph::GraphEdge;
use tokio::process::Command;

use crate::FetchTarget;
use crate::diff_parse::parse_unified_diff;
use crate::graph::assign_lanes;
use crate::model::{
    CommitSummary, DiffDocument, ParentBrief, RevisionDetails, RevisionSelection, SignatureInfo,
    StreamRow,
};
use crate::repository::{Repository, RepositorySnapshot};
use crate::source_browse::{SourceEntry, SourceEntryStatus};

/// Sentinel commit_id used for the synthetic git working-copy row.
/// Selection short-circuits on `is_working_copy` before this value is ever
/// passed to git, so it just needs to be visually distinct and not collide
/// with a real hex hash.
pub const GIT_WORKING_COPY_ID: &str = "wc";

/// How many commits a graph load lists unless the revision field asks for a
/// count of its own (`-n N`, `--max-count=N`). `git log` of a whole
/// million-commit history is seconds of git plus a graph nobody scrolls to
/// the bottom of; the cut-off rows show their parents as stubs.
pub const DEFAULT_LOG_LIMIT: usize = 10_000;

pub async fn load_git_diff(
    repository: &Repository,
    revision: &RevisionSelection,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    let base = match revision {
        RevisionSelection::WorkingCopy => working_copy_base(&repository.root).await?,
        RevisionSelection::Commit(_) => String::new(),
    };
    let args = git_backend_command(repository, revision, &base);
    let output = run_command(&repository.root, "git", args).await?;
    let document = parse_unified_diff(&output);
    let details = load_git_revision_details(repository, revision).await.ok();
    Ok((document, details))
}

pub async fn load_git_commits(
    repository: &Repository,
    revision_range: &str,
) -> Result<Vec<StreamRow>> {
    let worktree_output = run_command_bytes(
        &repository.root,
        "git",
        git_args(&["worktree", "list", "--porcelain", "-z"]),
    )
    .await?;
    let worktrees = parse_git_worktrees(&worktree_output);
    // Explicit log arguments filter as supplied. The default includes other
    // worktree tips, including detached HEADs outside the current ancestry.
    let mut args = git_args(&["log", "--topo-order", "--no-color"]);
    args.push(OsString::from(
        "--pretty=format:%h%x09%H%x09%P%x09%an%x09%x09%s",
    ));
    // `None` on an unborn branch (a repository with no commits yet), where
    // `git log HEAD` fails outright.
    let head = git_head(&repository.root).await;
    let user_args = log_range_args(revision_range)?;
    if !user_args
        .iter()
        .any(|arg| arg.to_string_lossy().starts_with("--max-count="))
    {
        args.push(OsString::from(format!("--max-count={DEFAULT_LOG_LIMIT}")));
    }
    let default_view = user_args.is_empty();
    if default_view {
        args.extend(head.iter().map(OsString::from));
        args.extend(worktrees.iter().map(|tree| OsString::from(&tree.head)));
    } else {
        args.extend(user_args);
    }
    let output = if default_view && head.is_none() && worktrees.is_empty() {
        // Nothing to walk: with no revisions `git log` would fall back to the
        // unborn HEAD and fail.
        String::new()
    } else {
        // Everything above is revisions; never let a typo be read as a path.
        args.push(OsString::from("--"));
        run_command(&repository.root, "git", args).await?
    };

    let mut rows = parse_commit_log_rows(&output);
    // Git has no native @ commit, so synthesize a working-copy row at
    // the top whenever the tree differs from HEAD. The row's parent
    // is HEAD so the graph keeps a continuous lane; on an unborn branch it
    // is a root, and the only row.
    if (!rows.is_empty() || head.is_none()) && git_has_uncommitted_changes(&repository.root).await?
    {
        rows.insert(
            0,
            ParsedCommitRow {
                change_id: GIT_WORKING_COPY_ID.to_owned(),
                commit_id: GIT_WORKING_COPY_ID.to_owned(),
                parents: head.into_iter().collect(),
                author: String::new(),
                is_empty: None,
                description: "Working copy".to_owned(),
                has_description: true,
                is_working_copy: true,
            },
        );
    }
    let root = repository.root.canonicalize()?;
    let mut labels: HashMap<String, Vec<String>> = HashMap::new();
    for tree in &worktrees {
        if tree
            .path
            .canonicalize()
            .unwrap_or_else(|_| tree.path.clone())
            == root
        {
            continue;
        }
        let name = tree.path.file_name().unwrap_or(tree.path.as_os_str());
        let duplicate = worktrees
            .iter()
            .filter(|other| other.path.file_name() == Some(name))
            .count()
            > 1;
        let label = if duplicate {
            tree.path.to_string_lossy().into_owned()
        } else {
            name.to_string_lossy().into_owned()
        };
        labels
            .entry(tree.head.clone())
            .or_default()
            .push(format!("{label}@"));
    }
    let mut summaries = build_commit_summaries(rows);
    for row in &mut summaries {
        row.summary.bookmarks = labels.remove(&row.summary.commit_id).unwrap_or_default();
    }
    Ok(summaries)
}

/// `git log` options the revision field may carry, as `--name` or
/// `--name=value`: revision selection and filtering only. Anything else —
/// `--output=<file>` writes a file, `-p`/`--format` reshape what the parser
/// reads — is refused rather than handed to git.
const LOG_OPTIONS: &[&str] = &[
    "--all",
    "--branches",
    "--tags",
    "--remotes",
    "--glob",
    "--exclude",
    "--not",
    "--first-parent",
    "--merges",
    "--no-merges",
    "--min-parents",
    "--max-parents",
    "--ancestry-path",
    "--simplify-by-decoration",
    "--author",
    "--committer",
    "--grep",
    "--since",
    "--after",
    "--until",
    "--before",
    "--max-count",
    "--skip",
];

/// Split the revision field into `git log` arguments, refusing any option not
/// in [`LOG_OPTIONS`]. Order is kept: `--not` and the `--all`/`--branches`
/// pseudo-revisions apply to what follows them, so options and revisions
/// can't be separated by `--end-of-options`. A ref can never begin with `-`,
/// so a leading dash always means an option. `-n N`, `-nN` and `-N` become
/// `--max-count=N`.
fn log_range_args(input: &str) -> Result<Vec<OsString>> {
    let mut args = Vec::new();
    let mut tokens = input.split_whitespace();
    while let Some(token) = tokens.next() {
        if !token.starts_with('-') {
            args.push(OsString::from(token));
            continue;
        }
        let count = match token {
            "-n" => Some(tokens.next().unwrap_or_default()),
            _ => token
                .strip_prefix("-n")
                .or_else(|| token.strip_prefix('-'))
                .filter(|count| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit())),
        };
        if let Some(count) = count {
            if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
                bail!("`-n` needs a number");
            }
            args.push(OsString::from(format!("--max-count={count}")));
            continue;
        }
        let name = token.split_once('=').map_or(token, |(name, _)| name);
        if !LOG_OPTIONS.contains(&name) {
            bail!(
                "unsupported git log option `{token}`; the revision field takes revisions \
                 and {}",
                LOG_OPTIONS.join(", ")
            );
        }
        args.push(OsString::from(token));
    }
    Ok(args)
}

#[derive(Debug)]
struct GitWorktree {
    path: PathBuf,
    head: String,
}

fn parse_git_worktrees(output: &[u8]) -> Vec<GitWorktree> {
    let mut trees = Vec::new();
    let mut path = None;
    let mut head = String::new();
    let mut skip = false;
    for field in output.split(|&byte| byte == 0) {
        if field.is_empty() {
            if let Some(path) = path.take()
                && !skip
                && !head.is_empty()
                && !head.bytes().all(|b| b == b'0')
            {
                trees.push(GitWorktree {
                    path,
                    head: std::mem::take(&mut head),
                });
            }
            head.clear();
            skip = false;
        } else if let Some(value) = field.strip_prefix(b"worktree ") {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt as _;
                path = Some(PathBuf::from(std::ffi::OsStr::from_bytes(value)));
            }
            #[cfg(not(unix))]
            {
                path = Some(PathBuf::from(String::from_utf8_lossy(value).into_owned()));
            }
        } else if let Some(value) = field.strip_prefix(b"HEAD ") {
            head = String::from_utf8_lossy(value).into_owned();
        } else if field == b"bare" || field.starts_with(b"prunable") {
            skip = true;
        }
    }
    trees
}

pub async fn load_git_repository_snapshot(repository_root: &Path) -> Result<RepositorySnapshot> {
    let output = run_command_bytes(
        repository_root,
        "git",
        git_args(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--branch",
            "--untracked-files=normal",
        ]),
    )
    .await?;
    let head = git_head(repository_root).await.unwrap_or_default();
    let refs = run_command(
        repository_root,
        "git",
        git_args(&["for-each-ref", "--format=%(refname) %(objectname)"]),
    )
    .await?;
    let worktrees = run_command_bytes(
        repository_root,
        "git",
        git_args(&["worktree", "list", "--porcelain", "-z"]),
    )
    .await?;
    // Status stays "M" across consecutive edits, so the second save of a file
    // needs something else to move the fingerprint. That used to be a full
    // `git diff HEAD --binary` on every watcher tick — every byte of every
    // change, re-diffed per save. The size and mtime of each path status
    // lists move on a save just the same, for a stat per dirty path.
    let mut fingerprint = std::collections::hash_map::DefaultHasher::new();
    for path in status_paths(&output) {
        path.hash(&mut fingerprint);
        path_stamp(&repository_root.join(path)).hash(&mut fingerprint);
    }
    (output, head, refs, worktrees).hash(&mut fingerprint);
    Ok(RepositorySnapshot {
        fingerprint: format!("{:016x}", fingerprint.finish()),
        working_copy_empty: None,
        // Git has no op log — the external-op escalation never applies.
        parent_fingerprint: None,
        ..RepositorySnapshot::default()
    })
}

/// The paths in `git status --porcelain=v1 -z` output: each entry is `XY
/// path`, a rename or copy is followed by its source path as a field of its
/// own, and `--branch` adds a leading `## ...` field.
fn status_paths(output: &[u8]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut fields = output.split(|&byte| byte == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 || field.starts_with(b"## ") {
            continue;
        }
        let (status, path) = field.split_at(3);
        paths.push(path_from_bytes(path));
        if matches!(status[0], b'R' | b'C')
            && let Some(source) = fields.next()
        {
            paths.push(path_from_bytes(source));
        }
    }
    paths
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Size and mtime of `path`, or `None` once it's gone — what a save changes.
fn path_stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

/// Global git flags prepended to every invocation we read the output of.
///
/// The user's own config otherwise reshapes that output under the parser:
/// `core.quotePath` decides whether a non-ASCII path arrives C-quoted,
/// `diff.noprefix` and `diff.mnemonicPrefix` replace the `a/`/`b/` the header
/// parser strips, `diff.suppressBlankEmpty` drops the leading space from an
/// empty context line, and `color.ui=always` wraps every line in escape
/// codes. `--no-optional-locks` keeps the watcher's polling off the user's
/// `index.lock` — git only takes that lock to write back a refreshed index,
/// so it is inert for the commands that don't.
const PINNED_GIT_ARGS: &[&str] = &[
    "-c",
    "core.quotePath=false",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicPrefix=false",
    "-c",
    "diff.suppressBlankEmpty=false",
    "-c",
    "color.ui=never",
    "--no-optional-locks",
];

fn git_args(args: &[&str]) -> Vec<OsString> {
    PINNED_GIT_ARGS
        .iter()
        .chain(args)
        .map(OsString::from)
        .collect()
}

/// The commit `HEAD` names, or `None` on an unborn branch.
async fn git_head(repository_root: &Path) -> Option<String> {
    run_command(
        repository_root,
        "git",
        git_args(&["rev-parse", "--verify", "--quiet", "HEAD"]),
    )
    .await
    .ok()
    .map(|head| head.trim().to_owned())
    .filter(|head| !head.is_empty())
}

/// What the working copy diffs against: `HEAD`, or on an unborn branch the
/// empty tree, so a fresh repository's staged files show as additions. Asked
/// of git rather than hardcoded because a SHA-256 repository's empty tree has
/// a different id.
async fn working_copy_base(repository_root: &Path) -> Result<String> {
    if git_head(repository_root).await.is_some() {
        return Ok("HEAD".to_owned());
    }
    // stdin is /dev/null, so this hashes an empty tree.
    let tree = run_command(
        repository_root,
        "git",
        git_args(&["hash-object", "-t", "tree", "--stdin"]),
    )
    .await?;
    Ok(tree.trim().to_owned())
}

/// The diff command for `revision`. `base` is what the working copy diffs
/// against (see [`working_copy_base`]); commits ignore it.
fn git_backend_command(
    repository: &Repository,
    revision: &RevisionSelection,
    base: &str,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = match revision {
        RevisionSelection::WorkingCopy => {
            // `git diff HEAD` covers both staged and unstaged changes
            // against the last committed state — the closest analog to
            // jj's @ working-copy diff. Untracked files are not included
            // (git diff only walks tracked paths).
            git_args(&["diff", base, "--no-ext-diff", "--no-color"])
        }
        RevisionSelection::Commit(revision) => {
            // `-m --first-parent`: `git show` defaults to a `--cc` combined
            // diff on a merge, which carries no `diff --git` headers, so the
            // parser sees nothing and the commit renders empty. The diff we
            // mean is the one against the first parent.
            let mut args = git_args(&[
                "show",
                "--format=",
                "--no-ext-diff",
                "--no-color",
                "-m",
                "--first-parent",
            ]);
            args.push(OsString::from(revision));
            args
        }
    };
    args.push(OsString::from("--"));

    if !repository.scope.as_os_str().is_empty() {
        args.push(repository.scope.as_os_str().to_owned());
    }

    args
}

/// `git fetch` (git's default remote, all remotes, or a single
/// remote/branch). Captures both stdout
/// and stderr as lines for the activity log — git writes progress and remote
/// messages to stderr even on success. No output means the fetch was a no-op
/// (up to date); the caller words the summary.
pub async fn fetch_git(repository: &Repository, target: &FetchTarget) -> Result<Vec<String>> {
    let args: Vec<OsString> = match target {
        FetchTarget::Default => vec![OsString::from("fetch")],
        FetchTarget::AllRemotes => {
            vec![OsString::from("fetch"), OsString::from("--all")]
        }
        FetchTarget::RemoteBranch { remote, branch } => vec![
            OsString::from("fetch"),
            OsString::from(remote),
            OsString::from(branch),
        ],
    };
    let askpass = crate::askpass::Scope::new();
    run_command_lines(&repository.root, "git", args, askpass.environment())
        .await
        .map_err(|error| askpass.explain(error))
}

/// Run a command and return its combined stdout+stderr split into non-empty
/// lines, regardless of exit status — used where the interesting output (git's
/// progress / remote sideband) lands on stderr even on success. Still errors on
/// a non-zero exit, surfacing the captured lines.
async fn run_command_lines(
    current_dir: &Path,
    program: &str,
    args: Vec<OsString>,
    env: std::collections::HashMap<OsString, OsString>,
) -> Result<Vec<String>> {
    let output = Command::new(program)
        .args(args)
        .envs(env)
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to execute {program}"))?;

    let mut lines = Vec::new();
    for chunk in [&output.stdout, &output.stderr] {
        for line in String::from_utf8_lossy(chunk).split(['\n', '\r']) {
            let line = line.trim_end();
            if !line.is_empty() {
                lines.push(line.to_owned());
            }
        }
    }

    if !output.status.success() {
        bail!(
            "{program} exited with {}: {}",
            output.status,
            lines.join("; ")
        );
    }
    Ok(lines)
}

async fn run_command_bytes(
    current_dir: &Path,
    program: &str,
    args: Vec<OsString>,
) -> Result<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to execute {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} exited with {}: {}", output.status, stderr.trim());
    }

    Ok(output.stdout)
}

/// Run a command and decode its stdout as text. git speaks bytes: a single
/// latin-1 line in a diff, or one non-UTF-8 filename in a listing, used to
/// fail the whole load, so the decode is lossy. Output that sniffs binary is
/// still refused rather than mangled into text — the same NUL sniff
/// [`crate::source_browse::classify_disk_bytes`] uses — so `git show` of a
/// binary blob doesn't reach a parser expecting lines.
async fn run_command(current_dir: &Path, program: &str, args: Vec<OsString>) -> Result<String> {
    let stdout = run_command_bytes(current_dir, program, args).await?;
    if stdout[..stdout.len().min(8 * 1024)].contains(&0) {
        bail!("{program} emitted binary output");
    }
    Ok(decode_lossy(stdout))
}

/// Run a git command whose output is NUL-separated (`-z`) and return its
/// non-empty fields. Separate from [`run_command`] because that one's binary
/// sniff would reject this shape outright; `-z` also means git never quotes
/// the paths, so they need no unquoting here.
async fn run_command_fields(current_dir: &Path, args: Vec<OsString>) -> Result<Vec<String>> {
    let stdout = run_command_bytes(current_dir, "git", args).await?;
    Ok(stdout
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect())
}

fn decode_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// Read the full old/new contents of one file at `revision`, for full-context
/// syntax highlighting. Best-effort: a side that doesn't resolve (added or
/// deleted file, root commit's parent, binary content, oversized) comes back
/// `None` and the caller falls back to the diff-only reconstruction.
pub async fn read_git_file_pair(
    repository: &Repository,
    revision: &RevisionSelection,
    path: &str,
    old_path: Option<&str>,
) -> (Option<String>, Option<String>) {
    const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
    let cap = |content: String| (content.len() <= MAX_SOURCE_BYTES).then_some(content);
    let old_path = old_path.unwrap_or(path);

    match revision {
        RevisionSelection::WorkingCopy => {
            // The new side is the working tree itself; the old side is what
            // the diff compared against (`git diff HEAD`).
            let new = tokio::fs::read(repository.root.join(path))
                .await
                .ok()
                .and_then(|bytes| {
                    (bytes.len() <= MAX_SOURCE_BYTES)
                        .then(|| String::from_utf8_lossy(&bytes).into_owned())
                });
            let old = git_show_file(&repository.root, &format!("HEAD:{old_path}"))
                .await
                .and_then(cap);
            (old, new)
        }
        RevisionSelection::Commit(id) => {
            let new = git_show_file(&repository.root, &format!("{id}:{path}"))
                .await
                .and_then(cap);
            let old = git_show_file(&repository.root, &format!("{id}^:{old_path}"))
                .await
                .and_then(cap);
            (old, new)
        }
    }
}

/// List every path at `revision` for the source browser. Commits list their
/// tree; the working copy combines `git ls-files` passes so untracked and
/// ignored paths appear too (ignored directories arrive collapsed via
/// `--directory`). See [`crate::source_browse::list_source_tree`].
pub async fn list_git_source_tree(
    repository: &Repository,
    revision: &RevisionSelection,
) -> Result<Vec<SourceEntry>> {
    // NUL-separated so paths with spaces/newlines split cleanly.
    let list = |args: &[&str]| run_command_fields(&repository.root, git_args(args));

    let mut entries = Vec::new();
    match revision {
        RevisionSelection::Commit(id) => {
            for path in list(&["ls-tree", "-r", "-z", "--name-only", id.as_str()]).await? {
                entries.push(SourceEntry::new(path, false, SourceEntryStatus::Tracked));
            }
        }
        RevisionSelection::WorkingCopy => {
            let tracked = list(&["ls-files", "-z"]).await?;
            let untracked = list(&["ls-files", "-z", "--others", "--exclude-standard"]).await?;
            let ignored = list(&[
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
            ])
            .await?;
            let changes = git_change_statuses(repository).await.unwrap_or_default();
            // Tracked entries reflect the index; a file deleted on disk but
            // still in the index is skipped so the browser mirrors the
            // directory like the jj walk does.
            for path in tracked {
                if repository.root.join(&path).symlink_metadata().is_ok() {
                    let mut entry = SourceEntry::new(path, false, SourceEntryStatus::Tracked);
                    entry.change = changes.get(&entry.path).copied();
                    entries.push(entry);
                }
            }
            for path in untracked {
                entries.push(SourceEntry::new(path, false, SourceEntryStatus::Untracked));
            }
            for path in ignored {
                let (path, is_dir) = match path.strip_suffix('/') {
                    Some(dir) => (dir.to_owned(), true),
                    None => (path, false),
                };
                entries.push(SourceEntry::new(path, is_dir, SourceEntryStatus::Ignored));
            }
        }
    }
    Ok(entries)
}

/// Per-path status of the working tree vs `HEAD` (staged or not) for the
/// browser's chips: `git diff --name-status` letters mapped to the shared
/// [`DiffFileStatus`]. Deletions are skipped (nothing on disk to chip);
/// renames chip the *new* path. Best-effort — an unborn HEAD yields nothing.
async fn git_change_statuses(
    repository: &Repository,
) -> Result<std::collections::HashMap<String, crate::DiffFileStatus>> {
    use crate::DiffFileStatus;
    let fields = run_command_fields(
        &repository.root,
        git_args(&["diff", "--name-status", "-z", "HEAD"]),
    )
    .await?;
    let mut fields = fields.into_iter();
    let mut changes = std::collections::HashMap::new();
    while let Some(status) = fields.next() {
        let Some(kind) = status.chars().next() else {
            continue;
        };
        let Some(path) = fields.next() else { break };
        match kind {
            'A' => {
                changes.insert(path, DiffFileStatus::Added);
            }
            'M' | 'T' => {
                changes.insert(path, DiffFileStatus::Modified);
            }
            'U' => {
                changes.insert(path, DiffFileStatus::Conflicted);
            }
            // Renames/copies carry a second (destination) path field.
            'R' | 'C' => {
                let Some(new_path) = fields.next() else { break };
                changes.insert(new_path, DiffFileStatus::Renamed);
            }
            _ => {}
        }
    }
    Ok(changes)
}

/// Read one file for the source browser: the working copy from disk, a commit
/// via `git show` (raw bytes, so binary detection happens on our side).
pub async fn read_git_source_file(
    repository: &Repository,
    revision: &RevisionSelection,
    path: &str,
) -> Result<crate::source_browse::SourceFileData> {
    match revision {
        RevisionSelection::WorkingCopy => {
            if Path::new(path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                bail!("invalid path {path}");
            }
            let bytes = tokio::fs::read(repository.root.join(path))
                .await
                .with_context(|| format!("failed to read {path}"))?;
            Ok(crate::source_browse::classify_disk_bytes(bytes))
        }
        RevisionSelection::Commit(id) => {
            let bytes = run_command_bytes(
                &repository.root,
                "git",
                vec![
                    OsString::from("show"),
                    OsString::from(format!("{id}:{path}")),
                ],
            )
            .await?;
            Ok(crate::source_browse::classify_disk_bytes(bytes))
        }
    }
}

/// `git show <rev>:<path>`, `None` on any failure (missing path, bad rev,
/// binary content — `run_command`'s sniff rejects that). Non-UTF-8 text
/// decodes lossily, matching the diff the spans are mapped onto.
async fn git_show_file(repository_root: &Path, spec: &str) -> Option<String> {
    run_command(
        repository_root,
        "git",
        vec![OsString::from("show"), OsString::from(spec)],
    )
    .await
    .ok()
}

async fn git_has_uncommitted_changes(repository_root: &Path) -> Result<bool> {
    // `git status --porcelain` prints one line per change (staged, unstaged,
    // or untracked) and nothing on a clean tree.
    let output = run_command(repository_root, "git", git_args(&["status", "--porcelain"])).await?;
    Ok(!output.trim().is_empty())
}

async fn load_git_revision_details(
    repository: &Repository,
    revision: &RevisionSelection,
) -> Result<RevisionDetails> {
    // %x1f is a unit-separator byte chosen to be unlikely in commit metadata,
    // so we can split the fields cleanly even when names or descriptions
    // contain tabs/newlines.
    const SEP: &str = "\x1f";
    let target = match revision {
        RevisionSelection::WorkingCopy => "HEAD".to_owned(),
        RevisionSelection::Commit(id) => id.clone(),
    };
    let format = format!("%H{SEP}%P{SEP}%an{SEP}%ae{SEP}%ad{SEP}%cn{SEP}%ce{SEP}%cd{SEP}%D{SEP}%B");
    // The same shape jj's details carry, so a timestamp reads the same in
    // either kind of repository.
    let mut args = git_args(&["show", "--no-patch", "--date=format:%Y-%m-%d %H:%M:%S %z"]);
    args.push(OsString::from(format!("--format={format}")));
    args.push(OsString::from(target));
    let output = run_command(&repository.root, "git", args).await?;

    let mut parts = output.splitn(10, '\x1f');
    let commit_id = parts.next().unwrap_or("").trim().to_owned();
    let parents = parts
        .next()
        .unwrap_or("")
        .split_whitespace()
        .map(|commit_id| ParentBrief {
            commit_id: commit_id.to_owned(),
            ..ParentBrief::default()
        })
        .collect();
    let author_name = parts.next().unwrap_or("").to_owned();
    let author_email = parts.next().unwrap_or("").to_owned();
    let author_date = parts.next().unwrap_or("").to_owned();
    let committer_name = parts.next().unwrap_or("").to_owned();
    let committer_email = parts.next().unwrap_or("").to_owned();
    let committer_date = parts.next().unwrap_or("").to_owned();
    let refs = parts.next().unwrap_or("").to_owned();
    let description = parts.next().unwrap_or("").trim_end().to_owned();

    let bookmarks: Vec<String> = refs
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(RevisionDetails {
        commit_id,
        change_id: None,
        bookmarks,
        author: SignatureInfo {
            name: author_name,
            email: author_email,
            timestamp: Some(author_date).filter(|s| !s.is_empty()),
        },
        committer: Some(SignatureInfo {
            name: committer_name,
            email: committer_email,
            timestamp: Some(committer_date).filter(|s| !s.is_empty()),
        }),
        signature: None,
        description,
        parents,
    })
}

struct ParsedCommitRow {
    change_id: String,
    commit_id: String,
    parents: Vec<String>,
    author: String,
    is_empty: Option<bool>,
    description: String,
    has_description: bool,
    is_working_copy: bool,
}

fn parse_commit_log_rows(output: &str) -> Vec<ParsedCommitRow> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(5, '\t');
            let change_id = parts.next()?.trim();
            let commit_id = parts.next()?.trim();
            let parents_field = parts.next().unwrap_or("");
            let author = parts.next()?.trim();
            let remainder = parts.next().unwrap_or("");
            let (empty, description) =
                if let Some((empty, description)) = remainder.split_once('\t') {
                    (parse_optional_bool(empty.trim()), description.trim())
                } else {
                    (None, remainder.trim())
                };

            if change_id.is_empty() || commit_id.is_empty() {
                return None;
            }

            let parents: Vec<String> = parents_field
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let has_description = !description.is_empty();
            Some(ParsedCommitRow {
                change_id: change_id.to_owned(),
                commit_id: commit_id.to_owned(),
                parents,
                author: author.to_owned(),
                is_empty: empty,
                description: if description.is_empty() {
                    "(no description set)".to_owned()
                } else {
                    description.to_owned()
                },
                has_description,
                is_working_copy: false,
            })
        })
        .collect()
}

fn build_commit_summaries(rows: Vec<ParsedCommitRow>) -> Vec<StreamRow> {
    // Walk the rows in their existing topo order and assign lanes from
    // parent edges. Parents not present in the listing (shallow clone, etc.)
    // become Missing edges so the renderer can draw a stub.
    let known: std::collections::HashSet<&str> =
        rows.iter().map(|row| row.commit_id.as_str()).collect();
    let lane_inputs = rows.iter().map(|row| {
        let edges: Vec<GraphEdge<String>> = row
            .parents
            .iter()
            .map(|parent| {
                if known.contains(parent.as_str()) {
                    GraphEdge::direct(parent.clone())
                } else {
                    GraphEdge::missing(parent.clone())
                }
            })
            .collect();
        (row.commit_id.clone(), edges)
    });
    let lane_frames = assign_lanes(lane_inputs);

    rows.into_iter()
        .zip(lane_frames)
        .map(|(row, frame)| StreamRow {
            frame,
            summary: CommitSummary {
                change_id: row.change_id,
                commit_id: row.commit_id,
                shortest_change_id_len: None,
                description: row.description,
                author: row.author,
                has_description: row.has_description,
                is_empty: row.is_empty,
                has_conflict: false,
                is_divergent: false,
                is_hidden: false,
                change_offset: None,
                is_working_copy: row.is_working_copy,
                is_immutable: false,
                on_trunk: false,
                bookmarks: Vec::new(),
                parent_ids: row.parents,
            },
        })
        .collect()
}

fn parse_optional_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::Vcs;
    use std::path::PathBuf;

    #[test]
    fn worktree_porcelain_preserves_paths_and_skips_non_checkouts() {
        let trees = parse_git_worktrees(b"worktree /repos/bare\0bare\0\0worktree /repos/with spaces\0HEAD abc123\0branch refs/heads/main\0\0worktree /repos/with\nnewline\0HEAD def456\0detached\0locked a reason\0\0worktree /repos/missing\0HEAD abc123\0prunable gone\0\0worktree /repos/unborn\0HEAD 000000\0branch refs/heads/new\0\0");
        assert_eq!(trees.len(), 2);
        assert_eq!(trees[0].path, PathBuf::from("/repos/with spaces"));
        assert_eq!(trees[0].head, "abc123");
        assert_eq!(trees[1].path, PathBuf::from("/repos/with\nnewline"));
        assert_eq!(trees[1].head, "def456");
    }

    fn parse_commit_log(output: &str) -> Vec<CommitSummary> {
        build_commit_summaries(parse_commit_log_rows(output))
            .into_iter()
            .map(|row| row.summary)
            .collect()
    }

    fn parse_commit_log_graph(output: &str) -> crate::graph_layout::GraphLayout {
        let mut graph = crate::graph_layout::GraphLayoutBuilder::new();
        for row in build_commit_summaries(parse_commit_log_rows(output)) {
            graph.push(&row.frame, &[]);
        }
        graph.finish()
    }

    #[test]
    fn git_commit_diff_uses_selected_revision() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Git,
            scope: PathBuf::new(),
        };

        let args = git_backend_command(
            &repository,
            &RevisionSelection::Commit("abc123".to_owned()),
            "",
        );

        assert!(args.contains(&OsString::from("show")));
        assert!(args.contains(&OsString::from("abc123")));
        assert_eq!(args.last(), Some(&OsString::from("--")));
        // `git show` on a merge otherwise emits a `--cc` combined diff, which
        // the parser drops whole.
        assert!(args.contains(&OsString::from("--first-parent")));
        assert!(args.contains(&OsString::from("-m")));
    }

    #[test]
    fn git_diff_commands_pin_the_parser_visible_config() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Git,
            scope: PathBuf::new(),
        };

        for revision in [
            RevisionSelection::WorkingCopy,
            RevisionSelection::Commit("abc123".to_owned()),
        ] {
            let args = git_backend_command(&repository, &revision, "HEAD");
            let rendered: Vec<&str> = args.iter().filter_map(|arg| arg.to_str()).collect();

            for pinned in [
                "core.quotePath=false",
                "diff.noprefix=false",
                "diff.mnemonicPrefix=false",
                "diff.suppressBlankEmpty=false",
                "color.ui=never",
                "--no-optional-locks",
                "--no-color",
            ] {
                assert!(
                    rendered.contains(&pinned),
                    "{pinned} missing from {rendered:?}"
                );
            }
            // git only accepts `-c` and friends before the subcommand.
            let subcommand = rendered
                .iter()
                .position(|arg| ["diff", "show"].contains(arg))
                .expect("a subcommand");
            let last_global = rendered
                .iter()
                .rposition(|arg| *arg == "--no-optional-locks")
                .expect("the lock flag");
            assert!(last_global < subcommand, "globals after {rendered:?}");
        }
    }

    #[test]
    fn the_revision_field_cannot_smuggle_options_into_git_log() {
        let args = |input: &str| {
            log_range_args(input).map(|args| {
                args.into_iter()
                    .map(|arg| arg.into_string().unwrap())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            args("--all --not origin/main").unwrap(),
            ["--all", "--not", "origin/main"]
        );
        assert_eq!(
            args("main --author=me -n 20").unwrap(),
            ["main", "--author=me", "--max-count=20"]
        );
        assert_eq!(args("-5 -n7").unwrap(), ["--max-count=5", "--max-count=7"]);
        assert!(args("").unwrap().is_empty());
        for hostile in [
            "--output=/tmp/x",
            "-p",
            "--format=%H",
            "-c",
            "--exec=sh",
            "-n x",
        ] {
            assert!(args(hostile).is_err(), "{hostile} must be refused");
        }
    }

    #[test]
    fn status_paths_skip_the_branch_line_and_keep_rename_sources() {
        let output = b"## main...origin/main\0 M src/a.rs\0R  new name.rs\0old name.rs\0?? dir/\0";
        assert_eq!(
            status_paths(output),
            ["src/a.rs", "new name.rs", "old name.rs", "dir/"].map(PathBuf::from)
        );
    }

    #[test]
    fn parses_commit_log_rows_basic() {
        let commits = parse_commit_log("abc\tdef\t\tme@example.com\tfalse\tadd commit sidebar\n");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].change_id, "abc");
        assert_eq!(commits[0].commit_id, "def");
        assert_eq!(commits[0].author, "me@example.com");
        assert_eq!(commits[0].description, "add commit sidebar");
        assert!(commits[0].has_description);
        assert_eq!(commits[0].is_empty, Some(false));
    }

    #[test]
    fn parses_commit_log_rows_without_description() {
        let commits = parse_commit_log("abc\tdef\t\tme@example.com\ttrue\t\n");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].description, "(no description set)");
        assert!(!commits[0].has_description);
        assert_eq!(commits[0].is_empty, Some(true));
    }

    #[test]
    fn commit_log_rows_select_full_revision_id() {
        let commits = parse_commit_log(
            "abc\tdef123456789abcdef\t\tme@example.com\tfalse\tadd commit sidebar\n",
        );

        assert_eq!(commits[0].change_id, "abc");
        assert_eq!(commits[0].commit_id, "def123456789abcdef");
    }

    #[test]
    fn git_log_merge_assigns_distinct_lanes_for_second_parent() {
        // Topo order, descendants first:
        //   M (merge of T and W) - parents: T W
        //   T - parent: A
        //   W - parent: A
        //   A - root, no parents
        let graph = parse_commit_log_graph(
            "M\tM\tT W\tme@example.com\tfalse\tmerge\n\
             T\tT\tA\tme@example.com\tfalse\ttrunk\n\
             W\tW\tA\tme@example.com\tfalse\tside\n\
             A\tA\t\tme@example.com\tfalse\troot\n",
        );

        assert_eq!(graph.len(), 4);
        assert_eq!(graph.frame(0, usize::MAX).node_lane, 0);
        assert_eq!(graph.frame(1, usize::MAX).node_lane, 0);
        // Second parent of the merge spawns a new lane to the right.
        assert_eq!(graph.frame(2, usize::MAX).node_lane, 1);
        // Both lanes converge back at A.
        assert_eq!(graph.frame(3, usize::MAX).node_lane, 0);
        assert_eq!(graph.frame(3, usize::MAX).merging_lanes, vec![0, 1]);
    }

    #[test]
    fn git_log_marks_unknown_parents_as_missing() {
        // Single commit whose parent isn't in the listing — e.g. a shallow clone.
        let graph = parse_commit_log_graph("abc\tabc\tdeadbeef\tme@example.com\tfalse\thead\n");

        assert_eq!(graph.len(), 1);
        assert_eq!(graph.frame(0, usize::MAX).missing_parents, 1);
        assert!(graph.frame(0, usize::MAX).after.is_empty());
    }
}

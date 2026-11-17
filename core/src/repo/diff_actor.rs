//! The actor of a diff with no repository behind it: a patch (a series of
//! them, each a step) or two files or folders compared.
//!
//! Like a pull request's, its outline stands where a graph would, and the
//! whole (or one patch) is its diff, streamed in parts under the size caps
//! other diffs have. A compare of folders is watched: an edit in either
//! reloads it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

use super::cancel::CancelFlag;
use super::protocol::{Command, Event, JobId, Payload, RepoError, RepoId};
use super::{Envelope, Jobs, route_commands, run_job};
use crate::diff_parse::DiffStreamParser;
use crate::jj::large_diff::{self, DiffLimits, RowBudget};
use crate::model::{
    DiffBase, DiffDocument, DiffFile, DiffFileStatus, DiffHunkView, Outline, RevisionSelection,
    Step, Subject,
};
use crate::patch::{DiffInput, PatchOrigin, Patches};

/// Flush the pending files once they hold this many rows, as a pull
/// request's diff does.
const BATCH_LINE_LIMIT: usize = 4_096;

/// What the actor keeps between commands: a patch's text, read once (stdin
/// can't be read twice), and how it splits.
#[derive(Default)]
struct Loaded {
    text: Option<Arc<str>>,
    patches: Patches,
}

pub(super) fn spawn(
    id: RepoId,
    input: DiffInput,
    commands: mpsc::UnboundedReceiver<Envelope>,
    poke: mpsc::UnboundedSender<Envelope>,
    events: mpsc::UnboundedSender<Event>,
) {
    if let DiffInput::Compare { left, right } = &input
        && left.is_dir()
        && right.is_dir()
    {
        super::watch(left.clone(), poke.clone());
        super::watch(right.clone(), poke);
    }
    let jobs: Jobs = Jobs::default();
    let (work_tx, mut work) = mpsc::unbounded_channel();
    tokio::spawn(route_commands(commands, work_tx, jobs.clone()));
    tokio::spawn(async move {
        let mut loaded = Loaded::default();
        while let Some(envelope) = work.recv().await {
            match envelope {
                Envelope::Shutdown => return,
                // An edit in a compared folder: the whole diff again.
                Envelope::Watch(batch) => {
                    if batch.worktree {
                        let _ = events.send(Event::new(id.clone(), Payload::RepositoryChanged));
                    }
                }
                Envelope::Run { command, .. } => {
                    let Some(job) = command.job() else { continue };
                    let span = tracing::info_span!("diff_command", repo = %id, job = job.0);
                    let emit = |payload| {
                        let _ = events.send(Event::new(id.clone(), payload));
                    };
                    let (input, loaded) = (&input, &mut loaded);
                    run_job(&jobs, job, span, emit, |ticket| async move {
                        run(input, loaded, *command, ticket.flag(), &emit).await
                    })
                    .await;
                }
            }
            if events.is_closed() {
                return;
            }
        }
    });
}

async fn run(
    input: &DiffInput,
    loaded: &mut Loaded,
    command: Command,
    cancel: &CancelFlag,
    emit: &impl Fn(Payload),
) -> Result<(), RepoError> {
    match (input, command) {
        (DiffInput::Patch(origin), Command::LoadGraph { job, .. }) => {
            read_patch(origin, loaded).await?;
            let text = loaded.text.clone().unwrap_or_default();
            let patches = &loaded.patches;
            let steps = if patches.parts.len() < 2 {
                Vec::new()
            } else {
                steps(&text, patches)
            };
            let mail = match patches.parts.as_slice() {
                [only] => only.mail.clone(),
                _ => patches.cover.clone(),
            };
            emit(Payload::Outline {
                job,
                outline: Outline {
                    subject: Subject::Patch {
                        origin: origin.name(),
                        mail,
                    },
                    steps,
                },
            });
        }
        (DiffInput::Compare { left, right }, Command::LoadGraph { job, .. }) => {
            emit(Payload::Outline {
                job,
                outline: Outline {
                    subject: Subject::Compare {
                        left: left.clone(),
                        right: right.clone(),
                    },
                    steps: Vec::new(),
                },
            });
        }
        (
            _,
            Command::LoadDiff {
                job,
                revision,
                base: DiffBase::Parents,
                expand,
            },
        ) => {
            let mut budget = RowBudget::new(DiffLimits::default());
            let mut stream = Stream::new(job, emit);
            match input {
                DiffInput::Patch(origin) => {
                    read_patch(origin, loaded).await?;
                    let text = loaded.text.clone().unwrap_or_default();
                    let parts = scope_parts(&loaded.patches, &revision);
                    let series_whole = loaded.patches.parts.len() > 1 && parts.len() > 1;
                    let mut parsed = Vec::new();
                    for part in parts {
                        if cancel.is_cancelled() {
                            emit(Payload::Cancelled { job });
                            return Ok(());
                        }
                        parse(&text[part], &mut |mut file| {
                            if series_whole {
                                parsed.push(file);
                            } else {
                                budget.cap(&mut file, &expand);
                                stream.push(file);
                            }
                        });
                    }
                    for mut file in merge_by_path(parsed) {
                        budget.cap(&mut file, &expand);
                        stream.push(file);
                    }
                }
                DiffInput::Compare { left, right } => {
                    let (left, right) = (left.clone(), right.clone());
                    let expand = expand.clone();
                    let flag = cancel.clone();
                    let files = tokio::task::spawn_blocking(move || {
                        compare(&left, &right, None, &expand, &mut budget, &flag)
                    })
                    .await
                    .map_err(|error| RepoError::Other(error.to_string()))??;
                    for file in files {
                        stream.push(file);
                    }
                }
            }
            if cancel.is_cancelled() {
                emit(Payload::Cancelled { job });
                return Ok(());
            }
            stream.finish(revision);
        }
        (
            _,
            Command::ExpandFile {
                job,
                revision,
                path,
                ..
            },
        ) => {
            let file = match input {
                DiffInput::Patch(origin) => {
                    read_patch(origin, loaded).await?;
                    let text = loaded.text.clone().unwrap_or_default();
                    let mut found = Vec::new();
                    for part in scope_parts(&loaded.patches, &revision) {
                        parse(&text[part], &mut |file| {
                            if file.path == path {
                                found.push(file);
                            }
                        });
                    }
                    merge_by_path(found).pop()
                }
                DiffInput::Compare { left, right } => {
                    let (left, right, only) = (left.clone(), right.clone(), path.clone());
                    let flag = cancel.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut budget = RowBudget::new(DiffLimits::unlimited());
                        compare(&left, &right, Some(&only), &[], &mut budget, &flag)
                    })
                    .await
                    .map_err(|error| RepoError::Other(error.to_string()))??
                    .pop()
                }
            };
            emit(Payload::FileExpanded { job, path, file });
        }
        (_, Command::Cancel { .. } | Command::SetRefreshMode { .. }) => {
            unreachable!("jobless commands are handled before dispatch")
        }
        // No other side to compare with, no history, nothing to change:
        // `Capabilities` says so.
        (_, command) => {
            if let Some(job) = command.job() {
                emit(Payload::Failed {
                    job,
                    error: RepoError::Unsupported,
                });
            }
        }
    }
    Ok(())
}

/// Read the patch once, and split it. A file is read again on each graph
/// load (a refresh); stdin and handed-over text never change.
async fn read_patch(origin: &PatchOrigin, loaded: &mut Loaded) -> Result<(), RepoError> {
    let text: Arc<str> = match origin {
        PatchOrigin::Text { text, .. } => text.clone(),
        _ if loaded.text.is_some() && *origin == PatchOrigin::Stdin => return Ok(()),
        PatchOrigin::Stdin => {
            let bytes = tokio::task::spawn_blocking(|| {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes).map(|_| bytes)
            })
            .await
            .map_err(|error| RepoError::Other(error.to_string()))?
            .map_err(|error| RepoError::Io(format!("couldn't read stdin: {error}")))?;
            String::from_utf8_lossy(&bytes).into()
        }
        PatchOrigin::File(path) => {
            let bytes = tokio::fs::read(path).await.map_err(|error| {
                RepoError::Io(format!("couldn't read {}: {error}", path.display()))
            })?;
            String::from_utf8_lossy(&bytes).into()
        }
    };
    if loaded.text.as_deref() != Some(&*text) {
        loaded.patches = crate::patch::split(&text);
        loaded.text = Some(text);
    }
    Ok(())
}

/// What a step is selected by: the commit the email names, unless two
/// share one, else its number.
fn step_ids(patches: &Patches) -> Vec<String> {
    let commits: Vec<Option<&str>> = patches
        .parts
        .iter()
        .map(|part| part.mail.as_ref().and_then(|mail| mail.commit.as_deref()))
        .collect();
    let unique = commits.iter().all(Option::is_some)
        && (1..commits.len()).all(|index| !commits[..index].contains(&commits[index]));
    commits
        .iter()
        .enumerate()
        .map(|(index, commit)| match commit {
            Some(commit) if unique => (*commit).to_owned(),
            _ => (index + 1).to_string(),
        })
        .collect()
}

/// The parts `revision` shows: one patch, or all of them.
fn scope_parts(patches: &Patches, revision: &RevisionSelection) -> Vec<std::ops::Range<usize>> {
    let ranges = patches.parts.iter().map(|part| part.range.clone());
    match revision {
        RevisionSelection::WorkingCopy => ranges.collect(),
        RevisionSelection::Commit(id) => step_ids(patches)
            .iter()
            .position(|step| step == id)
            .and_then(|index| patches.parts.get(index))
            .map(|part| vec![part.range.clone()])
            .unwrap_or_default(),
    }
}

/// A series' patches as steps, each with its lines added and removed.
fn steps(text: &str, patches: &Patches) -> Vec<Step> {
    patches
        .parts
        .iter()
        .zip(step_ids(patches))
        .enumerate()
        .map(|(index, (part, id))| {
            let mut counts = (0, 0);
            parse(&text[part.range.clone()], &mut |file| {
                counts.0 += file.additions;
                counts.1 += file.deletions;
            });
            let mail = part.mail.clone().unwrap_or_default();
            let title = if mail.subject.is_empty() {
                format!("Patch {}", index + 1)
            } else {
                mail.subject.clone()
            };
            let message = if mail.message.is_empty() {
                title.clone()
            } else {
                format!("{title}\n\n{}", mail.message)
            };
            Step {
                id,
                title,
                message,
                author: mail.author,
                authored_millis: mail.millis,
                counts: Some(counts),
            }
        })
        .collect()
}

fn parse(text: &str, on_file: &mut dyn FnMut(DiffFile)) {
    let mut parser = DiffStreamParser::default();
    for line in text.lines() {
        if let Some(file) = parser.push_line(line) {
            on_file(file);
        }
    }
    if let Some(file) = parser.finish() {
        on_file(file);
    }
}

/// A series' whole: each path once, its hunks from every patch that
/// touched it in order. They keep their own line numbers, which are each
/// patch's.
fn merge_by_path(files: Vec<DiffFile>) -> Vec<DiffFile> {
    let mut merged: Vec<DiffFile> = Vec::with_capacity(files.len());
    let mut at: HashMap<String, usize> = HashMap::new();
    for file in files {
        match at.get(&file.path) {
            Some(&index) => {
                let into = &mut merged[index];
                into.hunks.extend(file.hunks);
                into.additions += file.additions;
                into.deletions += file.deletions;
                if file.status == DiffFileStatus::Deleted {
                    into.status = DiffFileStatus::Deleted;
                }
            }
            None => {
                at.insert(file.path.clone(), merged.len());
                merged.push(file);
            }
        }
    }
    merged
}

/// The diff's files as they come, sent on in parts.
struct Stream<'a, E: Fn(Payload)> {
    job: JobId,
    emit: &'a E,
    batch: Vec<DiffFile>,
    rows: usize,
    totals: (usize, usize),
}

impl<'a, E: Fn(Payload)> Stream<'a, E> {
    fn new(job: JobId, emit: &'a E) -> Self {
        Self {
            job,
            emit,
            batch: Vec::new(),
            rows: 0,
            totals: (0, 0),
        }
    }

    fn push(&mut self, file: DiffFile) {
        self.totals.0 += file.additions;
        self.totals.1 += file.deletions;
        self.rows += file
            .hunks
            .iter()
            .map(|hunk| hunk.lines.len())
            .sum::<usize>();
        self.batch.push(file);
        if self.rows >= BATCH_LINE_LIMIT {
            (self.emit)(Payload::DiffPart {
                job: self.job,
                files: std::mem::take(&mut self.batch),
                total_additions: self.totals.0,
                total_deletions: self.totals.1,
            });
            self.rows = 0;
        }
    }

    fn finish(self, revision: RevisionSelection) {
        (self.emit)(Payload::DiffLoaded {
            job: self.job,
            revision,
            document: DiffDocument {
                files: self.batch,
                total_additions: self.totals.0,
                total_deletions: self.totals.1,
                range: None,
            },
            details: None,
        });
    }
}

/// The files that differ between `left` and `right`: two files, or two
/// folders file by file (`only`: just that path). Folders' `.git` and
/// `.jj` are skipped; a symlink is read through when it names a file.
fn compare(
    left: &Path,
    right: &Path,
    only: Option<&str>,
    expand: &[String],
    budget: &mut RowBudget,
    cancel: &CancelFlag,
) -> Result<Vec<DiffFile>, RepoError> {
    let io = |path: &Path, error: std::io::Error| {
        RepoError::Io(format!("couldn't read {}: {error}", path.display()))
    };
    let left_meta = std::fs::metadata(left).map_err(|error| io(left, error))?;
    let right_meta = std::fs::metadata(right).map_err(|error| io(right, error))?;
    let pairs: Vec<(String, Option<PathBuf>, Option<PathBuf>)> =
        match (left_meta.is_dir(), right_meta.is_dir()) {
            (false, false) => {
                let name = |path: &Path| {
                    path.file_name().map_or_else(
                        || path.display().to_string(),
                        |name| name.to_string_lossy().into_owned(),
                    )
                };
                vec![(name(right), Some(left.to_owned()), Some(right.to_owned()))]
            }
            (true, true) => {
                let mut paths = std::collections::BTreeMap::new();
                walk(left, left, &mut |relative, path| {
                    paths.entry(relative).or_insert((None, None)).0 = Some(path);
                })
                .map_err(|error| io(left, error))?;
                walk(right, right, &mut |relative, path| {
                    paths.entry(relative).or_insert((None, None)).1 = Some(path);
                })
                .map_err(|error| io(right, error))?;
                paths
                    .into_iter()
                    .map(|(relative, (old, new))| (relative, old, new))
                    .collect()
            }
            _ => {
                return Err(RepoError::Other(
                    "Compare two files or two folders, not one of each.".to_owned(),
                ));
            }
        };
    let mut files = Vec::new();
    for (path, old, new) in pairs {
        if only.is_some_and(|only| only != path) {
            continue;
        }
        if cancel.is_cancelled() {
            return Err(RepoError::Other("cancelled".to_owned()));
        }
        let read = |side: &Option<PathBuf>| match side {
            Some(path) => std::fs::read(path)
                .map(Some)
                .map_err(|error| io(path, error)),
            None => Ok(None),
        };
        let (before, after) = (read(&old)?, read(&new)?);
        if before == after {
            continue;
        }
        files.push(compare_file(path, before, after, expand, budget));
    }
    Ok(files)
}

/// One file's diff between its two sides, either of which may be absent.
fn compare_file(
    path: String,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    expand: &[String],
    budget: &mut RowBudget,
) -> DiffFile {
    let status = match (&before, &after) {
        (None, _) => DiffFileStatus::Added,
        (_, None) => DiffFileStatus::Deleted,
        _ => DiffFileStatus::Modified,
    };
    let mut file = DiffFile {
        old_path: before.is_some().then(|| path.clone()),
        path,
        status,
        hunks: Vec::new(),
        additions: 0,
        deletions: 0,
        collapsed: None,
        blob: None,
        mode_change: None,
    };
    let sizes = (
        before.as_ref().map(|bytes| bytes.len() as u64),
        after.as_ref().map(|bytes| bytes.len() as u64),
    );
    let (before, after) = (before.unwrap_or_default(), after.unwrap_or_default());
    // git's test: a NUL in the first 8000 bytes.
    let binary = |bytes: &[u8]| bytes[..bytes.len().min(8000)].contains(&0);
    if binary(&before) || binary(&after) {
        file.hunks.push(DiffHunkView {
            header: crate::media::summary_line("Binary file", sizes.0, sizes.1),
            lines: Vec::new(),
        });
        return file;
    }
    // As a tree diff budgets a file: one that surely fits gets its rows
    // and is charged for them; another is counted first, and past the caps
    // keeps only its counts.
    let expanded = expand.contains(&file.path);
    let surely_fits = budget.fits(large_diff::line_count(&before) + large_diff::line_count(&after));
    if !expanded && !surely_fits {
        let (additions, deletions) = large_diff::count_changed_lines(&before, &after);
        if let Err(reason) = budget.admit(additions + deletions) {
            file.hunks.push(DiffHunkView {
                header: large_diff::collapsed_header(reason, additions + deletions),
                lines: Vec::new(),
            });
            file.additions = additions;
            file.deletions = deletions;
            file.collapsed = Some(reason);
            return file;
        }
    }
    crate::jj::diff::text_hunks(&mut file, &before, &after, budget.limits());
    if !expanded && surely_fits {
        // Within the bound `fits` checked, so this admits.
        let _ = budget.admit(file.additions + file.deletions);
    }
    file
}

/// Every file under `dir`, by its path from `root` with `/` between names.
/// A symlink to a folder isn't followed, so a loop can't trap the walk.
fn walk(root: &Path, dir: &Path, found: &mut dyn FnMut(String, PathBuf)) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if name != ".git" && name != ".jj" {
                walk(root, &path, found)?;
            }
            continue;
        }
        if kind.is_symlink() && path.is_dir() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        found(relative, path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("diffui-compare-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        dir
    }

    fn write(path: PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn two_folders_compare_file_by_file() {
        let dir = scratch("folders");
        let (left, right) = (dir.join("left"), dir.join("right"));
        write(left.join("same.txt"), "same\n");
        write(right.join("same.txt"), "same\n");
        write(left.join("src/lib.rs"), "a\nb\nc\n");
        write(right.join("src/lib.rs"), "a\nB\nc\n");
        write(left.join("gone.txt"), "bye\n");
        write(right.join("new.txt"), "hi\n");
        write(right.join(".jj/ignored"), "x\n");
        std::fs::write(right.join("blob.bin"), b"\0\x01").unwrap();
        let mut budget = RowBudget::new(DiffLimits::default());
        let files = compare(
            &left,
            &right,
            None,
            &[],
            &mut budget,
            &CancelFlag::default(),
        )
        .expect("compares");
        let summary: Vec<_> = files
            .iter()
            .map(|file| {
                (
                    file.path.as_str(),
                    file.status,
                    file.additions,
                    file.deletions,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("blob.bin", DiffFileStatus::Added, 0, 0),
                ("gone.txt", DiffFileStatus::Deleted, 0, 1),
                ("new.txt", DiffFileStatus::Added, 1, 0),
                ("src/lib.rs", DiffFileStatus::Modified, 1, 1),
            ]
        );
        assert!(files[0].hunks[0].header.starts_with("Binary file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_past_the_caps_collapses_until_expanded() {
        let dir = scratch("caps");
        let big: String = (0..4_000).map(|n| format!("{n}\n")).collect();
        write(dir.join("left/big.txt"), "");
        write(dir.join("right/big.txt"), &big);
        let mut budget = RowBudget::new(DiffLimits::default());
        let files = compare(
            &dir.join("left"),
            &dir.join("right"),
            None,
            &[],
            &mut budget,
            &CancelFlag::default(),
        )
        .expect("compares");
        assert!(files[0].collapsed.is_some());
        assert_eq!(files[0].additions, 4_000);
        let mut budget = RowBudget::new(DiffLimits::unlimited());
        let files = compare(
            &dir.join("left"),
            &dir.join("right"),
            Some("big.txt"),
            &[],
            &mut budget,
            &CancelFlag::default(),
        )
        .expect("compares");
        assert_eq!(files[0].hunks[0].lines.len(), 4_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An edit in a compared folder reloads the comparison.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_edit_in_a_compared_folder_says_so() {
        use futures::StreamExt as _;
        let dir = scratch("watch");
        write(dir.join("left/f.txt"), "a\n");
        write(dir.join("right/f.txt"), "b\n");
        let mut events = crate::repo::open(
            crate::repo::OpenSpec::Diff(DiffInput::Compare {
                left: dir.join("left"),
                right: dir.join("right"),
            }),
            &crate::forge::Forges::default(),
            &crate::remote::Hosts::none(),
        );
        assert!(matches!(
            events.next().await.map(|event| event.payload),
            Some(Payload::Ready { .. })
        ));
        // Ready comes before the watch is up: keep saving until it is.
        let changed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for round in 0.. {
                write(dir.join("right/f.txt"), &format!("{round}\n"));
                tokio::select! {
                    event = events.next() => {
                        return matches!(event.map(|event| event.payload), Some(Payload::RepositoryChanged));
                    }
                    () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            }
            false
        })
        .await;
        assert_eq!(changed, Ok(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_series_whole_shows_each_path_once() {
        let text = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/patches/series.mbox"),
        )
        .unwrap();
        let patches = crate::patch::split(&text);
        let steps = steps(&text, &patches);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, "2bd9b96d002b47b81681b17953ff17c9f7113b06");
        assert_eq!(steps[0].counts, Some((2, 1)));
        assert_eq!(
            steps[0].message,
            "Shout beta and add delta\n\nThe list reads better with one loud entry.\nDelta was missing."
        );
        let second = scope_parts(&patches, &RevisionSelection::Commit(steps[1].id.clone()));
        assert_eq!(second, [patches.parts[1].range.clone()]);
    }
}

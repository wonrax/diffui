//! `diffui notes`: an agent's side of review notes, from its own shell.
//!
//! It works with the app closed: it opens the jj repository itself and writes
//! the store directly, under its lock (see [`super::store`]). A running app
//! watches the store and shows what changed.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use diffui_core::notes::{NotesRepo, Side};
use serde_json::{Value, json};

use super::locate::{Located, Place, locate};
use super::store::{self, Anchor, Lines, Message, Role, Store, Thread};

/// Exit code of a `wait` that timed out, as coreutils' `timeout` uses.
pub(crate) const TIMED_OUT: i32 = 124;

const ABOUT: &str = "Leave review notes on your changes and read the user's replies";

const LONG_ABOUT: &str = "\
Leave review notes on your changes and read the user's replies.

A note is on a change (a jj change id, so it follows amends and rebases),
optionally on one of its files, optionally on lines of that file. The user
reads notes inline in diffui's diff and replies where something should
change. A note nobody replies to is fine.

The loop: `add` notes on what the user should know, `wait` until the user
replies, fix the code, then `reply` (with `--resolve` when it's done).

Works whether diffui is open or not. See `diffui skill notes` for when to
leave notes.";

#[derive(Debug, clap::Parser)]
#[command(name = "diffui notes", about = ABOUT, long_about = LONG_ABOUT)]
struct NotesCli {
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    /// The repository (any path inside a jj workspace). Default: the
    /// current directory.
    #[arg(long, global = true, value_name = "PATH")]
    repo: Option<PathBuf>,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Debug, clap::Subcommand)]
enum Verb {
    /// Leave a note on a change, one of its files, or lines of a file
    ///
    /// Examples:
    ///   diffui notes add --as claude "Split the parser out so tests can reach it."
    ///   diffui notes add --as claude --file src/lib.rs --line 40-52 "Unsure about the lock order here."
    ///   diffui notes add --as claude --file src/old.rs --line 7 --old "Removed: nothing calls it."
    ///   diffui notes add --as claude --file src/lib.rs - <<'EOF'
    ///   A longer note, in Markdown.
    ///   EOF
    #[command(verbatim_doc_comment)]
    Add {
        /// The change, as a jj revset naming one revision.
        #[arg(short = 'r', long = "revision", default_value = "@")]
        revision: String,
        /// A file of the change, relative to the repository root (or to the
        /// current directory inside it).
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// A line, or an inclusive range like 40-52, of `--file` as the change
        /// has it.
        #[arg(long, value_name = "N[-M]", requires = "file")]
        line: Option<String>,
        /// The lines are on the old side: what the change removed or changed.
        #[arg(long, requires = "line")]
        old: bool,
        /// Your name, so the user can tell agents and sessions apart.
        #[arg(long = "as", value_name = "NAME", default_value = "agent")]
        author: String,
        /// The note, in Markdown; `-` reads it from stdin.
        body: Option<String>,
    },
    /// Reply to a note.
    Reply {
        /// The note's id, as `list` prints it.
        id: String,
        #[arg(long = "as", value_name = "NAME", default_value = "agent")]
        author: String,
        /// Resolve the note too: the reply says it's done.
        #[arg(long)]
        resolve: bool,
        /// The reply, in Markdown; `-` reads it from stdin.
        body: Option<String>,
    },
    /// Mark notes resolved: nothing left to do.
    Resolve {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long = "as", value_name = "NAME", default_value = "agent")]
        author: String,
    },
    /// Open resolved notes again.
    Reopen {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// List notes, grouped by change, newest activity first. Open ones only,
    /// unless --resolved.
    List {
        /// Only the changes this revset names. Default: every change.
        #[arg(short = 'r', long = "revisions", value_name = "REVSET")]
        revisions: Option<String>,
        /// Only notes the user wrote in last (a reply, or a note of theirs):
        /// your turn.
        #[arg(long)]
        waiting: bool,
        /// Resolved notes too.
        #[arg(long)]
        resolved: bool,
        /// Only notes this agent wrote in.
        #[arg(long = "by", value_name = "NAME")]
        by: Option<String>,
    },
    /// One note and all its replies.
    Show { id: String },
    /// Wait until the user replies to a note, then print the notes waiting
    /// on you. Returns at once when some already are (unless --new).
    ///
    /// Run it in the background while the user reviews. It sleeps until the
    /// store changes; it doesn't poll. Exits 0 with the notes, or 124 when
    /// --timeout passes first.
    Wait {
        /// Only notes on the changes this revset names.
        #[arg(short = 'r', long = "revisions", value_name = "REVSET")]
        revisions: Option<String>,
        /// Give up after this long: 90s, 10m, 1h; 0 waits forever.
        #[arg(long, value_name = "DURATION", default_value = "30m")]
        timeout: String,
        /// Ignore replies already waiting; wait for a new one.
        #[arg(long)]
        new: bool,
    },
    /// Notes as Markdown, to paste into a coding agent's prompt, a pull
    /// request or a message: each with where it is, the lines it's on and
    /// every message. Open notes on live changes, unless --resolved or ids.
    Export {
        /// These notes only.
        ids: Vec<String>,
        /// Only notes on the changes this revset names.
        #[arg(short = 'r', long = "revisions", value_name = "REVSET")]
        revisions: Option<String>,
        /// Resolved notes too.
        #[arg(long)]
        resolved: bool,
        #[arg(long, value_name = "FORMAT", default_value = "md", value_parser = ["md"])]
        format: String,
    },
    /// Delete notes by id.
    Delete {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Delete notes in bulk: resolved ones, archived ones (their change was
    /// abandoned), or all on some changes.
    Prune {
        #[arg(long)]
        resolved: bool,
        #[arg(long)]
        archived: bool,
        /// Only (or, alone, all) notes on the changes this revset names.
        #[arg(short = 'r', long = "revisions", value_name = "REVSET")]
        revisions: Option<String>,
        /// Say what would go, delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// The CLI's definition, for the guide's test.
#[cfg(test)]
pub(crate) fn cli() -> clap::Command {
    <NotesCli as clap::CommandFactory>::command()
}

/// `diffui notes …`: the exit code, or `None` when the command line isn't one.
pub(crate) fn client_main() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != "notes" {
        return None;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    Some(run(
        args.collect(),
        &cwd,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    ))
}

/// Run `diffui notes <args>` in `cwd`: stdin, output and errors are the
/// caller's. The exit code.
pub(crate) fn run(
    args: Vec<OsString>,
    cwd: &Path,
    stdin: &mut dyn std::io::Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    use clap::Parser as _;
    let cli =
        match NotesCli::try_parse_from(std::iter::once(OsString::from("diffui notes")).chain(args))
        {
            Ok(cli) => cli,
            Err(error) => {
                let text = error.render().to_string();
                let _ = if error.use_stderr() {
                    write!(err, "{text}")
                } else {
                    write!(out, "{text}")
                };
                return error.exit_code();
            }
        };
    let json = cli.json;
    match execute(cli, cwd, stdin, out) {
        Ok(code) => code,
        Err(error) => {
            if json {
                let _ = writeln!(out, "{}", json!({ "ok": false, "error": error }));
            } else {
                let _ = writeln!(err, "diffui notes: {error}");
            }
            1
        }
    }
}

/// The repository and its store.
struct Opened {
    repo: NotesRepo,
    store: Store,
}

fn open(cli_repo: Option<&Path>, cwd: &Path, snapshot: bool) -> Result<Opened, String> {
    let at = match cli_repo {
        Some(path) => cwd.join(path),
        None => cwd.to_owned(),
    };
    let repo = NotesRepo::open(&at, snapshot).map_err(|error| format!("{error:#}"))?;
    let store = Store::for_repo(&repo.repo_dir()).ok_or("no home directory")?;
    Ok(Opened { repo, store })
}

fn execute(
    cli: NotesCli,
    cwd: &Path,
    stdin: &mut dyn std::io::Read,
    out: &mut dyn Write,
) -> Result<i32, String> {
    let json = cli.json;
    let repo = cli.repo.as_deref();
    let io = |error: std::io::Error| format!("couldn't write the notes store: {error}");
    match cli.verb {
        Verb::Add {
            revision,
            file,
            line,
            old,
            author,
            body,
        } => {
            let body = read_body(body, stdin)?;
            let opened = open(repo, cwd, true)?;
            let thread = new_thread(&opened.repo, cwd, &revision, file, line, old, &author, body)?;
            let id = opened.store.create(thread).map_err(io)?;
            let thread = opened.store.get(&id).ok_or("the note vanished")?;
            print_threads(&opened.repo, &[thread], json, out);
            Ok(0)
        }
        Verb::Reply {
            id,
            author,
            resolve,
            body,
        } => {
            let body = read_body(body, stdin)?;
            let opened = open(repo, cwd, false)?;
            let thread = update(&opened.store, &id, |thread| {
                store::reply(thread, &author, Role::Agent, &body);
                if resolve {
                    thread.resolved = Some(store::Resolved {
                        by: author.clone(),
                        at: store::now(),
                    });
                }
                true
            })?;
            print_threads(&opened.repo, &[thread], json, out);
            Ok(0)
        }
        Verb::Resolve { ids, author } => {
            let opened = open(repo, cwd, false)?;
            let mut threads = Vec::new();
            for id in &ids {
                threads.push(update(&opened.store, id, |thread| {
                    let open = thread.resolved.is_none();
                    if open {
                        thread.resolved = Some(store::Resolved {
                            by: author.clone(),
                            at: store::now(),
                        });
                    }
                    open
                })?);
            }
            print_threads(&opened.repo, &threads, json, out);
            Ok(0)
        }
        Verb::Reopen { ids } => {
            let opened = open(repo, cwd, false)?;
            let mut threads = Vec::new();
            for id in &ids {
                threads.push(update(&opened.store, id, |thread| {
                    thread.resolved.take().is_some()
                })?);
            }
            print_threads(&opened.repo, &threads, json, out);
            Ok(0)
        }
        Verb::List {
            revisions,
            waiting,
            resolved,
            by,
        } => {
            let opened = open(repo, cwd, false)?;
            let scope = scope(&opened.repo, revisions.as_deref())?;
            let threads: Vec<Thread> = opened
                .store
                .load()
                .into_iter()
                .filter(|thread| in_scope(&scope, thread))
                .filter(|thread| resolved || thread.resolved.is_none())
                .filter(|thread| !waiting || thread.waiting_on_agent())
                .filter(|thread| {
                    by.as_deref()
                        .is_none_or(|by| thread.agents().any(|agent| agent == by))
                })
                .collect();
            if threads.is_empty() && !json {
                let _ = writeln!(out, "No notes");
            } else {
                print_threads(&opened.repo, &threads, json, out);
            }
            Ok(0)
        }
        Verb::Show { id } => {
            let opened = open(repo, cwd, false)?;
            let thread = opened.store.get(&id).ok_or_else(|| no_note(&id))?;
            print_threads(&opened.repo, &[thread], json, out);
            Ok(0)
        }
        Verb::Wait {
            revisions,
            timeout,
            new,
        } => {
            let timeout = parse_duration(&timeout)?;
            let opened = open(repo, cwd, false)?;
            let scope = scope(&opened.repo, revisions.as_deref())?;
            wait(&opened, &scope, timeout, new, json, out)
        }
        Verb::Export {
            ids,
            revisions,
            resolved,
            format: _,
        } => {
            let opened = open(repo, cwd, false)?;
            let scope = scope(&opened.repo, revisions.as_deref())?;
            let threads: Vec<Thread> = if ids.is_empty() {
                opened
                    .store
                    .load()
                    .into_iter()
                    .filter(|thread| in_scope(&scope, thread))
                    .filter(|thread| resolved || thread.resolved.is_none())
                    .collect()
            } else {
                ids.iter()
                    .map(|id| opened.store.get(id).ok_or_else(|| no_note(id)))
                    .collect::<Result<_, _>>()?
            };
            let places = locate(&opened.repo, &threads);
            let mut notes: Vec<(&Thread, &Located)> = threads
                .iter()
                .zip(&places)
                .filter(|(_, place)| !ids.is_empty() || !place.archived())
                .collect();
            notes.sort_by_key(|(thread, _)| std::cmp::Reverse(thread.activity()));
            let text = if notes.is_empty() {
                String::new()
            } else {
                super::export::markdown(&notes)
            };
            if json {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({ "ok": true, "count": notes.len(), "markdown": text })
                );
            } else if notes.is_empty() {
                let _ = writeln!(out, "No notes");
            } else {
                let _ = write!(out, "{text}");
            }
            Ok(0)
        }
        Verb::Delete { ids } => {
            let opened = open(repo, cwd, false)?;
            for id in &ids {
                opened.store.get(id).ok_or_else(|| no_note(id))?;
            }
            let deleted = opened.store.delete(&ids).map_err(io)?;
            report_deleted(deleted, &ids, false, json, out);
            Ok(0)
        }
        Verb::Prune {
            resolved,
            archived,
            revisions,
            dry_run,
        } => {
            if !resolved && !archived && revisions.is_none() {
                return Err(
                    "say what to prune: --resolved, --archived, or -r REVSET for every note on those changes"
                        .to_owned(),
                );
            }
            let opened = open(repo, cwd, false)?;
            let scope = scope(&opened.repo, revisions.as_deref())?;
            let threads: Vec<Thread> = opened
                .store
                .load()
                .into_iter()
                .filter(|thread| in_scope(&scope, thread))
                .collect();
            let places = locate(&opened.repo, &threads);
            let ids: Vec<String> = threads
                .iter()
                .zip(&places)
                .filter(|(thread, place)| {
                    let by_state =
                        (resolved && thread.resolved.is_some()) || (archived && place.archived());
                    by_state || (!resolved && !archived)
                })
                .map(|(thread, _)| thread.id.clone())
                .collect();
            let deleted = if dry_run {
                ids.len()
            } else {
                opened.store.delete(&ids).map_err(io)?
            };
            report_deleted(deleted, &ids, dry_run, json, out);
            Ok(0)
        }
    }
}

fn no_note(id: &str) -> String {
    format!("no note {id} (see `diffui notes list --resolved`)")
}

fn update(
    store: &Store,
    id: &str,
    change: impl FnOnce(&mut Thread) -> bool,
) -> Result<Thread, String> {
    store
        .update(id, change)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => no_note(id),
            _ => format!("couldn't write the notes store: {error}"),
        })
}

fn report_deleted(deleted: usize, ids: &[String], dry_run: bool, json: bool, out: &mut dyn Write) {
    if json {
        let _ = writeln!(
            out,
            "{}",
            json!({ "ok": true, "deleted": deleted, "ids": ids, "dry_run": dry_run })
        );
    } else {
        let verb = if dry_run { "Would delete" } else { "Deleted" };
        let noun = if deleted == 1 { "note" } else { "notes" };
        let _ = writeln!(out, "{verb} {deleted} {noun}");
    }
}

fn read_body(body: Option<String>, stdin: &mut dyn std::io::Read) -> Result<String, String> {
    let body = match body.as_deref() {
        None => return Err("give the text as an argument, or `-` to read it from stdin".to_owned()),
        Some("-") => {
            let mut text = String::new();
            stdin
                .read_to_string(&mut text)
                .map_err(|error| format!("couldn't read stdin: {error}"))?;
            text
        }
        Some(text) => text.to_owned(),
    };
    let body = body.trim().to_owned();
    if body.is_empty() {
        return Err("the text is empty".to_owned());
    }
    Ok(body)
}

/// The change ids a revset names, or every change when there's none.
fn scope(repo: &NotesRepo, revisions: Option<&str>) -> Result<Option<HashSet<String>>, String> {
    let Some(revisions) = revisions else {
        return Ok(None);
    };
    let changes = repo
        .resolve(revisions)
        .map_err(|error| format!("revset `{revisions}`: {error}"))?;
    Ok(Some(
        changes.into_iter().map(|change| change.change_id).collect(),
    ))
}

fn in_scope(scope: &Option<HashSet<String>>, thread: &Thread) -> bool {
    scope
        .as_ref()
        .is_none_or(|changes| changes.contains(thread.change_id()))
}

/// `--line 40` or `--line 40-52`, as (first, last).
fn parse_lines(text: &str) -> Result<(usize, usize), String> {
    let bad = || format!("--line takes a line or a range like 40-52, not `{text}`");
    let (first, last) = match text.split_once('-') {
        Some((first, last)) => (first.trim(), last.trim()),
        None => (text.trim(), text.trim()),
    };
    let first: usize = first.parse().map_err(|_| bad())?;
    let last: usize = last.parse().map_err(|_| bad())?;
    if first == 0 || last < first {
        return Err(bad());
    }
    Ok((first, last))
}

/// `path` as the repository names it: relative to its root, `/`-separated.
fn repo_path(root: &Path, cwd: &Path, path: &str) -> Result<String, String> {
    let given = Path::new(path);
    let absolute = if given.is_absolute() {
        given.canonicalize().unwrap_or_else(|_| given.to_owned())
    } else {
        // Relative to where the agent is, which may be below the root.
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
        if cwd.starts_with(root) {
            cwd.join(given)
        } else {
            root.join(given)
        }
    };
    let mut parts: Vec<String> = Vec::new();
    let relative = absolute
        .strip_prefix(root)
        .map_err(|_| format!("{path} isn't in the repository at {}", root.display()))?;
    for part in relative.components() {
        match part {
            std::path::Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return Err(format!("{path} is the repository root, not a file"));
    }
    Ok(parts.join("/"))
}

#[allow(clippy::too_many_arguments)]
fn new_thread(
    repo: &NotesRepo,
    cwd: &Path,
    revision: &str,
    file: Option<String>,
    line: Option<String>,
    old: bool,
    author: &str,
    body: String,
) -> Result<Thread, String> {
    let changes = repo
        .resolve(revision)
        .map_err(|error| format!("revset `{revision}`: {error}"))?;
    let change = match changes.as_slice() {
        [change] => change.clone(),
        [] => return Err(format!("revset `{revision}` names no revision")),
        _ => {
            return Err(format!(
                "revset `{revision}` names {} revisions; a note goes on one",
                changes.len()
            ));
        }
    };
    let short = &change.change_id[..change.change_id.len().min(8)];
    let anchor = match file {
        None => None,
        Some(file) => {
            let path = repo_path(repo.root(), cwd, &file)?;
            let side = if old { Side::Old } else { Side::New };
            let text = repo.read(&change.commit_id, &path, side);
            let lines = match line {
                None => {
                    let other = if old { Side::New } else { Side::Old };
                    if text.is_none() && repo.read(&change.commit_id, &path, other).is_none() {
                        return Err(format!("{short} has no file {path}"));
                    }
                    None
                }
                Some(line) => {
                    let (first, last) = parse_lines(&line)?;
                    let Some(text) = text else {
                        let side = if old {
                            "before the change (it's added here: drop --old)"
                        } else {
                            "after the change (deleted? use --old)"
                        };
                        return Err(format!("{path} isn't in {short} {side}"));
                    };
                    let all: Vec<&str> = text.lines().collect();
                    if last > all.len() {
                        return Err(format!(
                            "{path} has {} lines {} {short}; --line {line} is past the end",
                            all.len(),
                            if old { "before" } else { "in" },
                        ));
                    }
                    Some(Lines {
                        side,
                        start: first,
                        text: all[first - 1..last]
                            .iter()
                            .map(|line| (*line).to_owned())
                            .collect(),
                    })
                }
            };
            Some(Anchor { path, lines })
        }
    };
    Ok(Thread::on_change(
        &change,
        anchor,
        Message {
            author: author.to_owned(),
            role: Role::Agent,
            at: store::now(),
            body,
        },
    ))
}

/// `90s`, `10m`, `1h`, a bare number of seconds; `0` is no limit.
fn parse_duration(text: &str) -> Result<Option<Duration>, String> {
    let text = text.trim();
    let (number, unit) = text
        .find(|c: char| !c.is_ascii_digit())
        .map_or((text, ""), |at| text.split_at(at));
    let number: u64 = number
        .parse()
        .map_err(|_| format!("--timeout takes 90s, 10m, 1h or 0, not `{text}`"))?;
    let seconds = match unit {
        "" | "s" => number,
        "m" => number * 60,
        "h" => number * 3600,
        _ => return Err(format!("--timeout takes 90s, 10m, 1h or 0, not `{text}`")),
    };
    Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

/// What a thread has said so far, to tell a new reply from an old one.
fn said(thread: &Thread) -> (String, usize) {
    (thread.id.clone(), thread.messages.len())
}

fn wait(
    opened: &Opened,
    scope: &Option<HashSet<String>>,
    timeout: Option<Duration>,
    new: bool,
    json: bool,
    out: &mut dyn Write,
) -> Result<i32, String> {
    use notify::Watcher as _;
    let dir = opened.store.threads_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("couldn't make {}: {error}", dir.display()))?;
    let (events, changed) = std::sync::mpsc::channel::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = events.send(());
        }
    })
    .map_err(|error| format!("couldn't watch the notes store: {error}"))?;
    watcher
        .watch(&dir, notify::RecursiveMode::NonRecursive)
        .map_err(|error| format!("couldn't watch the notes store: {error}"))?;
    let waiting = || -> Vec<Thread> {
        opened
            .store
            .load()
            .into_iter()
            .filter(|thread| in_scope(scope, thread) && thread.waiting_on_agent())
            .collect()
    };
    let seen: HashSet<(String, usize)> = if new {
        waiting().iter().map(said).collect()
    } else {
        HashSet::new()
    };
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    // A missed event only delays the answer until this rescan; it never
    // spins.
    const RESCAN: Duration = Duration::from_secs(30);
    loop {
        let fresh: Vec<Thread> = waiting()
            .into_iter()
            .filter(|thread| !seen.contains(&said(thread)))
            .collect();
        if !fresh.is_empty() {
            print_threads(&opened.repo, &fresh, json, out);
            return Ok(0);
        }
        let left = match deadline {
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => left.min(RESCAN),
                _ => {
                    if json {
                        let _ = writeln!(
                            out,
                            "{}",
                            json!({ "ok": true, "timed_out": true, "threads": [] })
                        );
                    } else {
                        let _ = writeln!(out, "No replies yet");
                    }
                    return Ok(TIMED_OUT);
                }
            },
            None => RESCAN,
        };
        match changed.recv_timeout(left) {
            // A burst of events is one change: let it finish, then look.
            Ok(()) => {
                std::thread::sleep(Duration::from_millis(50));
                while changed.try_recv().is_ok() {}
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the store watch stopped".to_owned());
            }
        }
    }
}

/// The status word a thread gets in the CLI, which is the agent's view.
fn status(thread: &Thread) -> &'static str {
    if thread.resolved.is_some() {
        "resolved"
    } else if thread.waiting_on_agent() {
        "replied"
    } else {
        "open"
    }
}

/// `src/lib.rs:40-52`, `src/lib.rs`, or the change.
fn where_text(thread: &Thread, located: &Located) -> String {
    let Some(anchor) = &thread.anchor else {
        return "change".to_owned();
    };
    let old = |side: Side| if side == Side::Old { " (old)" } else { "" };
    match (&located.place, &anchor.lines) {
        (Place::Lines { start, end }, Some(lines)) if start == end => {
            format!("{}:{start}{}", anchor.path, old(lines.side))
        }
        (Place::Lines { start, end }, Some(lines)) => {
            format!("{}:{start}-{end}{}", anchor.path, old(lines.side))
        }
        (_, Some(lines)) if lines.start == lines.end() => {
            format!("{}:{}{}", anchor.path, lines.start, old(lines.side))
        }
        (_, Some(lines)) => format!(
            "{}:{}-{}{}",
            anchor.path,
            lines.start,
            lines.end(),
            old(lines.side)
        ),
        (_, None) => anchor.path.clone(),
    }
}

fn thread_json(thread: &Thread, located: &Located) -> Value {
    let lines = thread
        .anchor
        .as_ref()
        .and_then(|anchor| anchor.lines.as_ref());
    let (start, end) = match located.place {
        Place::Lines { start, end } => (Some(start), Some(end)),
        _ => (None, None),
    };
    json!({
        "id": thread.id,
        "change_id": thread.change_id(),
        "commit_id": located.now.as_ref().map(|now| now.commit_id.clone()),
        "title": located.now.as_ref().map_or(thread.title().to_owned(), |now| now.title.clone()),
        "path": thread.anchor.as_ref().map(|anchor| anchor.path.clone()),
        "side": lines.map(|lines| match lines.side { Side::Old => "old", Side::New => "new" }),
        "start": start,
        "end": end,
        "text": lines.map(|lines| lines.text.clone()),
        "status": status(thread),
        "outdated": located.outdated(),
        "archived": located.archived(),
        "messages": thread.messages.iter().map(|message| json!({
            "author": message.author,
            "role": message.role,
            "at": message.at,
            "body": message.body,
        })).collect::<Vec<_>>(),
    })
}

/// Threads grouped by change, the most recently active change first, each
/// change's notes in document order.
fn print_threads(repo: &NotesRepo, threads: &[Thread], json: bool, out: &mut dyn Write) {
    let places = locate(repo, threads);
    let mut order: Vec<usize> = (0..threads.len()).collect();
    let latest = |change: &str| {
        threads
            .iter()
            .filter(|thread| thread.change_id() == change)
            .map(Thread::activity)
            .max()
            .unwrap_or(0)
    };
    order.sort_by(|&a, &b| {
        let (a_thread, b_thread) = (&threads[a], &threads[b]);
        latest(b_thread.change_id())
            .cmp(&latest(a_thread.change_id()))
            .then_with(|| a_thread.change_id().cmp(b_thread.change_id()))
            .then_with(|| doc_order(a_thread, &places[a]).cmp(&doc_order(b_thread, &places[b])))
    });
    if json {
        let threads: Vec<Value> = order
            .iter()
            .map(|&index| thread_json(&threads[index], &places[index]))
            .collect();
        let _ = writeln!(out, "{}", json!({ "ok": true, "threads": threads }));
        return;
    }
    let now = store::now() as i64 * 1000;
    let mut change = None;
    for index in order {
        let (thread, located) = (&threads[index], &places[index]);
        if change != Some(thread.change_id()) {
            change = Some(thread.change_id());
            let title = located
                .now
                .as_ref()
                .map_or(thread.title(), |now| now.title.as_str());
            let title = if title.is_empty() {
                "(no description)"
            } else {
                title
            };
            let gone = if located.archived() {
                "  [abandoned]"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "{}  {title}{gone}",
                &thread.change_id()[..thread.change_id().len().min(8)]
            );
        }
        let mut flags = vec![status(thread)];
        if located.outdated() {
            flags.push("outdated");
        }
        let _ = writeln!(
            out,
            "  {}  {}  {}",
            thread.id,
            where_text(thread, located),
            flags.join(", ")
        );
        for message in &thread.messages {
            let age = crate::meta_line::age_phrase(message.at as i64 * 1000, now);
            let who = match message.role {
                Role::Agent => message.author.clone(),
                Role::User => "user".to_owned(),
            };
            let _ = writeln!(out, "    {who}, {age}:");
            for line in message.body.lines() {
                let _ = writeln!(out, "      {line}");
            }
        }
        if let Some(resolved) = &thread.resolved {
            let age = crate::meta_line::age_phrase(resolved.at as i64 * 1000, now);
            let _ = writeln!(out, "    resolved by {}, {age}", resolved.by);
        }
    }
}

/// Change notes, then each file's notes by line.
fn doc_order<'a>(thread: &'a Thread, located: &Located) -> (Option<&'a str>, usize) {
    let line = match located.place {
        Place::Lines { start, .. } => start,
        _ => 0,
    };
    (
        thread.anchor.as_ref().map(|anchor| anchor.path.as_str()),
        line,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_durations_parse() {
        assert_eq!(parse_lines("40"), Ok((40, 40)));
        assert_eq!(parse_lines("40-52"), Ok((40, 52)));
        assert!(parse_lines("0").is_err());
        assert!(parse_lines("9-3").is_err());
        assert_eq!(parse_duration("90s"), Ok(Some(Duration::from_secs(90))));
        assert_eq!(parse_duration("10m"), Ok(Some(Duration::from_secs(600))));
        assert_eq!(parse_duration("0"), Ok(None));
        assert!(parse_duration("soon").is_err());
    }

    #[test]
    fn paths_are_relative_to_the_root() {
        let root = Path::new("/r");
        assert_eq!(
            repo_path(root, Path::new("/r/src"), "a.rs"),
            Ok("src/a.rs".to_owned())
        );
        assert_eq!(
            repo_path(root, Path::new("/elsewhere"), "src/a.rs"),
            Ok("src/a.rs".to_owned())
        );
        assert_eq!(
            repo_path(root, Path::new("/r"), "/r/b/../c.rs"),
            Ok("c.rs".to_owned())
        );
        assert!(repo_path(root, Path::new("/r"), "/x/c.rs").is_err());
    }
}

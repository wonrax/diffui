//! Review notes end to end: the CLI on scratch jj repositories with the app
//! closed, and the real app on them (and on the fixture) picking notes up
//! live, replying, stepping and cleaning up.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;

use super::app::{Filter, NotesMsg};
use super::cli::{self, TIMED_OUT};
use super::store::{self, Role, Store};
use crate::Action;
use crate::diff_view::DocSpot;
use crate::harness::{Harness, fixture_jj, fixture_repo, scratch_home};

/// `jj <args>` in `repo`, its stdout.
pub(crate) fn jj(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new(fixture_jj())
        .current_dir(repo)
        .args(["--color", "never"])
        .args(args)
        .output()
        .expect("run jj");
    assert!(
        output.status.success(),
        "jj {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8")
}

/// A new jj repository holding `a.txt`, described "Add a".
fn scratch_repo(name: &str) -> PathBuf {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let dir = scratch_home()
        .join("repos/notes")
        .join(format!("{name}-{}", COUNT.fetch_add(1, Ordering::Relaxed)))
        .join("repo");
    std::fs::create_dir_all(&dir).expect("make the repo dir");
    jj(&dir, &["git", "init"]);
    std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").expect("write a.txt");
    jj(&dir, &["describe", "-m", "Add a"]);
    dir.canonicalize().expect("the repo")
}

/// `diffui notes <args>` in `cwd`: exit code, stdout, stderr.
pub(crate) fn notes(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    notes_with_stdin(cwd, args, "")
}

fn notes_with_stdin(cwd: &Path, args: &[&str], stdin: &str) -> (i32, String, String) {
    scratch_home();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(
        args.iter().map(OsString::from).collect(),
        cwd,
        &mut stdin.as_bytes(),
        &mut out,
        &mut err,
    );
    (
        code,
        String::from_utf8(out).expect("utf-8"),
        String::from_utf8(err).expect("utf-8"),
    )
}

/// `diffui notes <args> --json`, which must succeed.
fn notes_json(cwd: &Path, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--json");
    let (code, out, err) = notes(cwd, &args);
    assert_eq!(code, 0, "notes {args:?}: {out}{err}");
    serde_json::from_str(&out).unwrap_or_else(|error| panic!("{error}: {out}"))
}

fn threads(cwd: &Path, args: &[&str]) -> Vec<Value> {
    notes_json(cwd, args)["threads"]
        .as_array()
        .expect("threads")
        .clone()
}

/// Add a note and hand back its id.
pub(crate) fn add(cwd: &Path, args: &[&str]) -> String {
    let mut full = vec!["add"];
    full.extend_from_slice(args);
    let added = threads(cwd, &full);
    added[0]["id"].as_str().expect("an id").to_owned()
}

fn store_of(repo: &Path) -> Store {
    let dir = diffui_core::notes::repo_dir(repo).expect("a jj repo");
    Store::for_repo(&dir).expect("a home")
}

/// The user's reply to `id`, as the app writes it.
pub(crate) fn user_reply(repo: &Path, id: &str, body: &str) {
    store_of(repo)
        .update(id, |thread| {
            store::reply(thread, "you", Role::User, body);
            true
        })
        .expect("reply");
}

#[test]
fn add_reply_resolve_and_list_with_the_app_closed() {
    let repo = scratch_repo("cli");
    let id = add(
        &repo,
        &[
            "--as",
            "claude",
            "--file",
            "a.txt",
            "--line",
            "2",
            "Why **two**?",
        ],
    );
    let listed = threads(&repo, &["list"]);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["start"], 2);
    assert_eq!(listed[0]["text"], serde_json::json!(["two"]));
    assert_eq!(listed[0]["status"], "open");
    assert_eq!(listed[0]["messages"][0]["author"], "claude");

    // A change note, its text from stdin, from a subdirectory.
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    let (code, out, err) = notes_with_stdin(
        &repo.join("sub"),
        &["add", "--as", "codex", "-"],
        "A longer note.\n\nIn two paragraphs.\n",
    );
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("change  open"), "{out}");
    assert!(out.contains("In two paragraphs."), "{out}");

    // Nothing is waiting on the agent until the user replies.
    assert!(threads(&repo, &["list", "--waiting"]).is_empty());
    user_reply(&repo, &id, "Make it three.");
    let waiting = threads(&repo, &["list", "--waiting"]);
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0]["status"], "replied");
    assert_eq!(waiting[0]["messages"][1]["role"], "user");
    assert_eq!(threads(&repo, &["list", "--by", "codex"]).len(), 1);

    let replied = threads(
        &repo,
        &["reply", &id, "--as", "claude", "--resolve", "Done."],
    );
    assert_eq!(replied[0]["status"], "resolved");
    assert_eq!(threads(&repo, &["list"]).len(), 1);
    assert_eq!(threads(&repo, &["list", "--resolved"]).len(), 2);
    assert_eq!(threads(&repo, &["reopen", &id])[0]["status"], "open");
    assert_eq!(threads(&repo, &["resolve", &id])[0]["status"], "resolved");

    // The errors say what to do.
    let (code, _, err) = notes(&repo, &["add", "--file", "a.txt", "--line", "9", "x"]);
    assert_eq!(code, 1);
    assert!(err.contains("a.txt has 3 lines"), "{err}");
    let (code, _, err) = notes(&repo, &["add", "--file", "nope.txt", "x"]);
    assert_eq!(code, 1);
    assert!(err.contains("no file nope.txt"), "{err}");
    let (code, _, err) = notes(&repo, &["add", "-r", "all()", "x"]);
    assert_eq!(code, 1);
    assert!(err.contains("names 2 revisions"), "{err}");
    let (code, _, err) = notes(&repo, &["reply", "zzzzzz", "x"]);
    assert_eq!(code, 1);
    assert!(err.contains("no note zzzzzz"), "{err}");
}

#[test]
fn wait_returns_on_a_reply_and_exits_on_its_timeout() {
    let repo = scratch_repo("wait");
    let id = add(&repo, &["--as", "claude", "Look at this."]);
    let waiter = {
        let repo = repo.clone();
        std::thread::spawn(move || notes(&repo, &["wait", "--timeout", "20s", "--json"]))
    };
    std::thread::sleep(Duration::from_millis(500));
    assert!(!waiter.is_finished(), "nothing was waiting yet");
    user_reply(&repo, &id, "Please rename it.");
    let (code, out, err) = waiter.join().unwrap();
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("Please rename it."), "{out}");

    // Already waiting: at once. With --new: only a newer reply counts.
    assert_eq!(notes(&repo, &["wait", "--timeout", "5s"]).0, 0);
    let started = std::time::Instant::now();
    let (code, out, _) = notes(&repo, &["wait", "--new", "--timeout", "1s"]);
    assert_eq!(code, TIMED_OUT, "{out}");
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(out.contains("No replies yet"), "{out}");
}

#[test]
fn notes_follow_amends_and_rebases_and_go_outdated_when_their_lines_do() {
    let repo = scratch_repo("reattach");
    let a = jj(&repo, &["log", "--no-graph", "-r", "@", "-T", "change_id"]);
    jj(&repo, &["new", "-m", "Use a"]);
    std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    jj(&repo, &["status"]);
    let on_two = add(
        &repo,
        &["--as", "claude", "--file", "a.txt", "--line", "2", "two"],
    );
    let on_four = add(
        &repo,
        &["--as", "claude", "--file", "a.txt", "--line", "4", "four"],
    );
    let start = |id: &str| threads(&repo, &["show", id])[0]["start"].clone();
    assert_eq!(start(&on_two), 2);

    // An amend that moves the lines: they're found where they went.
    std::fs::write(repo.join("a.txt"), "zero\none\ntwo\nthree\nfour\n").unwrap();
    jj(&repo, &["status"]);
    assert_eq!(start(&on_two), 3);
    assert_eq!(start(&on_four), 5);

    // A rebase under it (its parent gains lines): still found.
    jj(&repo, &["edit", &a]);
    std::fs::write(repo.join("a.txt"), "one\nhalf\ntwo\nthree\n").unwrap();
    jj(&repo, &["status"]);
    jj(&repo, &["edit", "children(@)"]);
    let after = threads(&repo, &["show", &on_two]);
    assert_eq!(after[0]["outdated"], false);
    assert_eq!(after[0]["start"], 4);

    // The line rewritten: the note stays on its file, outdated.
    let text = std::fs::read_to_string(repo.join("a.txt")).unwrap();
    std::fs::write(repo.join("a.txt"), text.replace("two", "TWO")).unwrap();
    jj(&repo, &["status"]);
    let gone = threads(&repo, &["show", &on_two]);
    assert_eq!(gone[0]["outdated"], true);
    assert_eq!(gone[0]["start"], Value::Null);
    assert_eq!(gone[0]["path"], "a.txt");
    assert!(
        notes(&repo, &["list"])
            .1
            .contains("a.txt:2  open, outdated")
    );
}

#[test]
fn workspaces_of_one_repository_share_notes_and_abandoned_changes_archive_them() {
    let repo = scratch_repo("workspaces");
    let other = repo.parent().unwrap().join("other");
    jj(
        &repo,
        &[
            "workspace",
            "add",
            "--name",
            "other",
            other.to_str().unwrap(),
        ],
    );
    let change = jj(&repo, &["log", "--no-graph", "-r", "@", "-T", "change_id"]);
    let id = add(
        &other,
        &[
            "--as",
            "second",
            "-r",
            &change,
            "--file",
            "a.txt",
            "From over there.",
        ],
    );
    assert_eq!(store_of(&repo), store_of(&other));
    let seen = threads(&repo, &["list"]);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["id"], id.as_str());

    jj(&repo, &["new", "-m", "Next"]);
    jj(&repo, &["abandon", &change]);
    let archived = threads(&repo, &["list"]);
    assert_eq!(archived[0]["archived"], true);
    let (code, out, _) = notes(&repo, &["prune", "--archived", "--dry-run"]);
    assert_eq!((code, out.as_str()), (0, "Would delete 1 note\n"));
    assert_eq!(threads(&repo, &["list"]).len(), 1);
    let (code, out, _) = notes(&repo, &["prune", "--archived"]);
    assert_eq!((code, out.as_str()), (0, "Deleted 1 note\n"));
    assert!(threads(&repo, &["list", "--resolved"]).is_empty());
    let (code, _, err) = notes(&repo, &["prune"]);
    assert_eq!(code, 1);
    assert!(err.contains("say what to prune"), "{err}");
}

/// Several agents writing to one repository's notes at once lose nothing.
#[test]
fn concurrent_cli_writers_lose_nothing() {
    let repo = scratch_repo("concurrent");
    let id = add(&repo, &["--as", "claude", "Shared."]);
    let writers: Vec<_> = (0..4)
        .map(|n| {
            let (repo, id) = (repo.clone(), id.clone());
            std::thread::spawn(move || {
                for k in 0..5 {
                    let name = format!("agent-{n}");
                    let (code, out, err) =
                        notes(&repo, &["reply", &id, "--as", &name, &format!("{k}")]);
                    assert_eq!(code, 0, "{out}{err}");
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let thread = threads(&repo, &["show", &id]);
    assert_eq!(thread[0]["messages"].as_array().unwrap().len(), 21);
}

/// The notes' line in the diff: its row, by the line number it has now.
fn spot_of_note(h: &mut Harness, id: &str) -> Option<DocSpot> {
    let id = id.to_owned();
    h.read(move |ui| {
        let tab = ui.active();
        super::app::diff_notes(tab)
            .into_iter()
            .find(|(_, note)| note.thread.id == id)
            .map(|(spot, _)| spot)
    })
}

#[test]
fn the_open_app_shows_notes_live_and_replies_into_the_store() {
    let repo = scratch_repo("live");
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    h.wait_until("the notes to load", |ui| ui.active().notes.settled());

    let id = add(
        &repo,
        &[
            "--as", "claude", "--file", "a.txt", "--line", "2", "Why two?",
        ],
    );
    h.wait_until("the note to show", |ui| {
        ui.active().notes.get(&id).is_some()
    });
    h.wait_until("the diff", |ui| {
        !ui.active().session.document.files.is_empty()
    });
    let spot = spot_of_note(&mut h, &id).expect("the note in the diff");
    let DocSpot::Line(at) = spot else {
        panic!("{spot:?}");
    };
    let line = h.read(|ui| {
        ui.active().session.document.files[at.file].hunks[at.hunk].lines[at.line]
            .content
            .clone()
    });
    assert_eq!(line, "two");
    h.frames();
    // Its card is laid under the line, inside the diff; opened, its
    // actions sit together in its footer.
    h.perform(Action::Notes(NotesMsg::Toggle(id.clone())));
    h.frames();
    let reply = h.bounds(gpui_kit::ElementId::Name(format!("note-{id}-reply").into()));
    let resolve = h.bounds(gpui_kit::ElementId::Name(
        format!("note-{id}-resolve").into(),
    ));
    assert!(reply.size.height > gpui_kit::px(10.0), "{reply:?}");
    assert_eq!(reply.origin.y, resolve.origin.y);
    assert!(reply.right() <= resolve.left());
    // The log row and the file row count it.
    let change = h.read(|ui| super::app::shown_change(ui.active()).unwrap().to_owned());
    assert_eq!(h.read(|ui| ui.active().notes.open_on_change(&change)), 1);
    assert_eq!(h.read(|ui| super::app::file_note_count(ui, at.file)), 1);

    // Reply in the app: the reply mode owns ⌘⏎.
    h.perform(Action::Notes(NotesMsg::StartReply(id.clone())));
    h.perform(Action::Notes(NotesMsg::ReplyEdited(
        "Make it three.".to_owned(),
    )));
    h.press("cmd-enter");
    h.wait_until("the reply in the store", |ui| {
        ui.active()
            .notes
            .get(&id)
            .is_some_and(|note| note.thread.waiting_on_agent())
    });
    assert!(h.read(|ui| ui.active().note_reply().is_none()));
    let thread = store_of(&repo).get(&id).unwrap();
    assert_eq!(thread.messages[1].body, "Make it three.");
    assert_eq!(thread.messages[1].role, Role::User);

    // The agent answers and resolves from its shell: the app follows.
    notes(
        &repo,
        &["reply", &id, "--as", "claude", "--resolve", "Done."],
    );
    h.wait_until("resolved", |ui| {
        ui.active()
            .notes
            .get(&id)
            .is_some_and(|note| note.thread.resolved.is_some())
    });
    assert_eq!(h.read(|ui| ui.active().notes.open_on_change(&change)), 0);
    h.perform(Action::Notes(NotesMsg::ToggleResolved));
    assert!(spot_of_note(&mut h, &id).is_none());
}

#[test]
fn the_notes_view_filters_and_deletes_in_bulk() {
    let repo = scratch_repo("view");
    let first = add(&repo, &["--as", "claude", "One."]);
    let second = add(&repo, &["--as", "codex", "--file", "a.txt", "Two."]);
    add(&repo, &["--as", "claude", "Three."]);
    user_reply(&repo, &first, "Hm?");
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    h.wait_until("the notes", |ui| {
        ui.active().notes.settled() && ui.active().notes.notes.len() == 3
    });
    h.perform(Action::SetMainView(crate::MainView::Notes));
    assert_eq!(h.read(|ui| ui.active().main_view), crate::MainView::Notes);
    h.settle();
    h.frames();
    // The left pane is the notes list now, its head the filter, which
    // starts on Waiting on you.
    let filter = h.bounds("notes-view-waiting-on-you-filter");
    assert!(filter.size.height > gpui_kit::px(10.0), "{filter:?}");
    let listed = |h: &mut Harness| {
        h.read(|ui| {
            super::view::listed(ui.active())
                .iter()
                .map(|note| note.thread.id.clone())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(listed(&mut h).len(), 2);
    h.perform(Action::Notes(NotesMsg::Filter(Filter::WaitingOnAgent)));
    assert_eq!(listed(&mut h), std::slice::from_ref(&first));
    h.perform(Action::Notes(NotesMsg::Filter(Filter::WaitingOnYou)));
    h.perform(Action::Notes(NotesMsg::FilterNamed(
        "author:codex".to_owned(),
    )));
    assert_eq!(listed(&mut h), std::slice::from_ref(&second));
    h.perform(Action::Notes(NotesMsg::FilterNamed("everyone".to_owned())));
    h.perform(Action::Notes(NotesMsg::FilterNamed("open".to_owned())));
    assert_eq!(listed(&mut h).len(), 3);

    // A thread selected: its detail on the right, with Open in diff.
    h.perform(Action::Notes(NotesMsg::SelectThread(second.clone())));
    h.settle();
    h.frames();
    let open = h.bounds("notes-view-open");
    assert!(open.size.height > gpui_kit::px(10.0), "{open:?}");

    // Select all, delete: asked first, with how many.
    h.perform(Action::Notes(NotesMsg::SelectAll(true)));
    let selected: Vec<String> =
        h.read(|ui| ui.active().notes.view.selected.iter().cloned().collect());
    assert_eq!(selected.len(), 3);
    h.perform(Action::Notes(NotesMsg::AskDelete(selected.clone())));
    assert_eq!(
        h.read(|ui| ui.active().notes.view.confirm_delete.as_ref().map(Vec::len)),
        Some(3)
    );
    assert_eq!(
        store_of(&repo).load().len(),
        3,
        "nothing goes before the answer"
    );
    h.perform(Action::Notes(NotesMsg::Delete(selected)));
    h.wait_until("the notes gone", |ui| {
        ui.active().notes.settled() && ui.active().notes.notes.is_empty()
    });
    assert!(store_of(&repo).load().is_empty());
}

#[test]
fn next_and_previous_step_through_notes_and_conflicts_across_files() {
    let repo = fixture_repo();
    let commit = std::fs::read_to_string(repo.join(".jj/fixture-manifest"))
        .unwrap()
        .lines()
        .find_map(|line| {
            let mut parts = line.split(' ');
            let names = parts.next()?;
            let commit = parts.nth(1)?;
            names
                .split(',')
                .any(|it| it == "conflicted")
                .then(|| commit.to_owned())
        })
        .expect("the conflicted revision");
    add(&repo, &["--as", "claude", "-r", &commit, "On the change."]);
    add(
        &repo,
        &[
            "--as",
            "claude",
            "-r",
            &commit,
            "--file",
            "src/main.rs",
            "On main.rs.",
        ],
    );
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    h.perform(Action::SelectRevision(
        diffui_core::RevisionSelection::Commit(commit),
    ));
    h.wait_until("the diff and its notes", |ui| {
        let tab = ui.active();
        tab.notes.settled()
            && !tab.session.document.files.is_empty()
            && super::app::diff_notes(tab).len() == 2
    });
    let spots = h.read(|ui| super::app::spots(ui.active()));
    let notes = spots
        .iter()
        .filter(|(_, kind)| matches!(kind, super::app::SpotKind::Note { .. }))
        .count();
    assert_eq!(notes, 2);
    assert!(spots.len() > notes, "conflicts too: {spots:?}");
    let files: std::collections::HashSet<usize> = spots
        .iter()
        .filter_map(|(spot, _)| match spot {
            DocSpot::File(file) => Some(*file),
            DocSpot::Line(at) => Some(at.file),
            DocSpot::Top => None,
        })
        .collect();
    assert!(files.len() > 1, "across files: {spots:?}");

    let revealed = |h: &mut Harness| h.read(|ui| ui.active().notes.reveal.map(|(spot, _)| spot));
    let mut seen = Vec::new();
    for _ in 0..spots.len() {
        h.press("]");
        seen.push(revealed(&mut h).unwrap());
    }
    let order: Vec<DocSpot> = spots.iter().map(|(spot, _)| *spot).collect();
    assert_eq!(seen, order, "in document order");
    h.press("]");
    assert_eq!(revealed(&mut h), Some(order[0]), "round again");
    h.press("[");
    assert_eq!(revealed(&mut h), order.last().copied());
    h.press("[");
    assert_eq!(revealed(&mut h), order.get(order.len() - 2).copied());
}

/// A stand-in AI provider (`[ai] command`) that prints `reply`.
fn fake_reviewer(reply: &str) -> PathBuf {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let dir = scratch_home().join("repos");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join(format!(
        "fake-reviewer-{}",
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat > /dev/null\ncat <<'EOF'\n{reply}\nEOF\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn review(reply: &str) -> (Harness, PathBuf) {
    let repo = scratch_repo("review");
    let script = fake_reviewer(reply);
    let mut h = Harness::open_fresh(
        Some(&repo),
        (1280.0, 800.0),
        &format!("[ai]\ncommand = [{:?}]\n", script.display().to_string()),
        "",
    );
    h.wait_for_graph();
    h.wait_until("the notes to load", |ui| ui.active().notes.settled());
    h.act(|ui, cx| {
        ui.run_command(
            "ai.review",
            crate::commands::CommandArg::Revision(diffui_core::RevisionSelection::WorkingCopy),
            cx,
        )
    });
    (h, repo)
}

#[test]
fn an_ai_review_lands_as_notes_on_the_lines_it_cites() {
    let (mut h, repo) =
        review("- `a.txt:2`: two is suspicious.\n- `a.txt:40`: past the end.\n- No tests.");
    h.wait_until("the review's notes", |ui| {
        ui.active().notes.notes.len() == 2
    });
    let mut threads = store_of(&repo).load();
    threads.sort_by_key(|thread| thread.anchor.is_none());
    let lines = threads[0].anchor.as_ref().unwrap().lines.as_ref().unwrap();
    assert_eq!(
        (lines.start, lines.text.clone()),
        (2, vec!["two".to_owned()])
    );
    assert_eq!(threads[0].messages[0].body, "two is suspicious.");
    assert_eq!(threads[0].messages[0].author, "Custom command");
    // What didn't cite a line it has goes on the change, nothing lost.
    assert!(threads[1].anchor.is_none());
    assert_eq!(
        threads[1].messages[0].body,
        "- `a.txt:40`: past the end.\n- No tests."
    );
    h.wait_until("the panel closed", |ui| {
        ui.mode(crate::modes::ModeKind::CliOutput).is_none()
    });
}

#[test]
fn an_ai_review_without_findings_stays_in_the_panel() {
    let (mut h, repo) = review("Nothing stands out.");
    h.wait_until("the run to end", |ui| {
        ui.active()
            .cmdline
            .runs
            .iter()
            .any(|run| run.exit.is_some())
    });
    h.settle();
    assert!(h.read(|ui| ui.mode(crate::modes::ModeKind::CliOutput).is_some()));
    assert!(store_of(&repo).load().is_empty());
}

#[test]
fn the_skill_teaches_every_notes_verb() {
    for verb in [
        "add", "reply", "resolve", "reopen", "list", "show", "wait", "export", "delete", "prune",
    ] {
        assert!(
            crate::ctl::SKILL.contains(&format!("`diffui notes {verb}")),
            "{verb}"
        );
    }
}

#[test]
fn export_writes_the_markdown_copy_writes() {
    let repo = scratch_repo("export");
    let id = add(
        &repo,
        &[
            "--as",
            "claude",
            "--file",
            "a.txt",
            "--line",
            "2-3",
            "Why these?",
        ],
    );
    add(&repo, &["--as", "claude", "Done otherwise."]);
    let resolved = add(&repo, &["--as", "codex", "--file", "a.txt", "Old."]);
    notes(&repo, &["resolve", &resolved]);
    user_reply(&repo, &id, "Make it one line.");
    let change = jj(&repo, &["log", "--no-graph", "-r", "@", "-T", "change_id"]);
    let (code, out, err) = notes(&repo, &["export"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        format!(
            "# Review notes

## Change `{change}`: Add a

### The whole change

**claude:** Done otherwise.

### `a.txt:2-3`

```
two
three
```

**claude:** Why these?

**Reviewer:** Make it one line.
"
        )
    );
    // Resolved ones on asking, by id too; the same text as JSON.
    let (_, all, _) = notes(&repo, &["export", "--resolved"]);
    assert!(
        all.contains("### `a.txt` (the whole file) (resolved)"),
        "{all}"
    );
    let one = notes_json(&repo, &["export", &resolved]);
    assert_eq!(one["count"], 1);
    assert!(
        one["markdown"]
            .as_str()
            .unwrap()
            .contains("**codex:** Old.")
    );
}

#[test]
fn the_user_starts_notes_that_wait_on_the_agent_and_copies_them() {
    let repo = scratch_repo("user-notes");
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    h.wait_until("the diff and notes", |ui| {
        ui.active().notes.settled() && !ui.active().session.document.files.is_empty()
    });
    let row = |h: &mut Harness, text: &str| {
        let text = text.to_owned();
        h.read(move |ui| {
            let file = &ui.active().session.document.files[0];
            file.hunks.iter().enumerate().find_map(|(hunk, view)| {
                let line = view.lines.iter().position(|line| line.content == text)?;
                Some(crate::diff_view::LineAt {
                    file: 0,
                    hunk,
                    line,
                })
            })
        })
        .expect("the row")
    };
    let press = |line, extend| crate::diff_view::LinePress {
        line,
        extend,
        one_side: false,
    };
    // A gutter press on "one", shift on "three": the note is on lines 1-3.
    let (one, three) = (row(&mut h, "one"), row(&mut h, "three"));
    h.perform(Action::Notes(NotesMsg::StartNote(super::NoteTarget::Line(
        press(one, false),
    ))));
    h.perform(Action::Notes(NotesMsg::StartNote(super::NoteTarget::Line(
        press(three, true),
    ))));
    h.perform(Action::Notes(NotesMsg::ReplyEdited(
        "Use a loop here.".to_owned(),
    )));
    h.frames();
    let field = h.bounds("note-reply-send");
    assert!(field.size.height > gpui_kit::px(10.0));
    h.press("cmd-enter");
    h.wait_until("the note", |ui| ui.active().notes.notes.len() == 1);
    h.perform(Action::Notes(NotesMsg::StartNote(super::NoteTarget::File(
        "a.txt".to_owned(),
    ))));
    h.perform(Action::Notes(NotesMsg::ReplyEdited(
        "Rename the file.".to_owned(),
    )));
    h.perform(Action::Notes(NotesMsg::SendReply));
    h.perform(Action::Notes(NotesMsg::StartNote(
        super::NoteTarget::Change,
    )));
    h.perform(Action::Notes(NotesMsg::ReplyEdited(
        "Split this change.".to_owned(),
    )));
    h.perform(Action::Notes(NotesMsg::SendReply));
    h.wait_until("three notes", |ui| {
        ui.active().notes.settled() && ui.active().notes.notes.len() == 3
    });

    // The agent sees all three waiting on it.
    let waiting = threads(&repo, &["list", "--waiting"]);
    assert_eq!(waiting.len(), 3);
    assert!(waiting.iter().all(|thread| thread["status"] == "replied"));
    assert!(
        waiting
            .iter()
            .all(|thread| thread["messages"][0]["role"] == "user")
    );
    let lines = waiting
        .iter()
        .find(|thread| thread["start"] != Value::Null)
        .expect("the line note");
    assert_eq!(
        (lines["start"].clone(), lines["end"].clone()),
        (1.into(), 3.into())
    );
    assert_eq!(lines["text"], serde_json::json!(["one", "two", "three"]));
    assert_eq!(notes(&repo, &["wait", "--timeout", "5s"]).0, 0);
    // The Notes view's agent filter has them too.
    h.perform(Action::Notes(NotesMsg::Filter(Filter::WaitingOnAgent)));
    assert_eq!(h.read(|ui| super::view::listed(ui.active()).len()), 3);

    // Copy: the change's open notes, as the export writes them, and a toast.
    h.perform(Action::Notes(NotesMsg::CopyChange { resolved: false }));
    let copied = h
        .with_window(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .expect("copied text");
    let (_, exported, _) = notes(&repo, &["export"]);
    assert_eq!(copied, exported);
    assert!(
        copied.contains("**Reviewer:** Use a loop here."),
        "{copied}"
    );
    assert_eq!(
        h.read(|ui| ui.toasts.last().map(|toast| toast.title.to_string())),
        Some("Copied 3 notes as Markdown".to_owned())
    );
}

/// The "+" of the line under the pointer: where it is, if one shows.
fn add_button(h: &mut Harness) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
    use gpui_kit::test::TestWindowExt as _;
    h.with_window(|window, _| {
        window
            .try_find("diff-add-note-button")
            .map(|found| found.bounds())
    })
}

/// The lines of the note being started, if one is.
fn new_note_lines(h: &mut Harness) -> Option<(usize, Vec<String>)> {
    h.read(|ui| {
        let reply = ui.active().note_reply()?;
        let super::app::ReplyTo::New(note) = &reply.to else {
            return None;
        };
        let lines = note.anchor.as_ref()?.lines.as_ref()?;
        Some((lines.start, lines.text.clone()))
    })
}

#[test]
fn the_plus_on_a_hovered_line_starts_a_note_and_drags_a_range() {
    let repo = scratch_repo("plus");
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    h.wait_until("the diff and notes", |ui| {
        ui.active().notes.settled() && !ui.active().session.document.files.is_empty()
    });
    h.frames();
    // Hover down the diff's text: each of a.txt's rows shows its "+".
    let x = h.read(|ui| ui.left_pane_width() + ui.files_pane_width()) + 300.0;
    let mut rows: Vec<(f32, gpui_kit::Bounds<gpui_kit::Pixels>)> = Vec::new();
    let mut y = 60.0;
    while y < 780.0 {
        h.hover_point(crate::harness::at(x, y));
        h.frames();
        if let Some(button) = add_button(&mut h)
            && rows
                .last()
                .is_none_or(|(_, last)| last.origin.y != button.origin.y)
        {
            rows.push((y, button));
        }
        y += 4.0;
    }
    assert_eq!(rows.len(), 3, "{rows:?}");
    let (first_y, first) = rows[0];
    let (third_y, _) = rows[2];
    // It overlays the gutter, left of the text, on its row.
    assert!(f32::from(first.right()) < x - 100.0, "{first:?}");
    assert!((f32::from(first.top()) - 6.0..f32::from(first.bottom()) + 6.0).contains(&first_y));

    // A click on it: a note on that line.
    h.hover_point(crate::harness::at(x, first_y));
    h.frames();
    let plus = add_button(&mut h).expect("the +");
    h.click_point(plus.center());
    h.settle();
    assert_eq!(new_note_lines(&mut h), Some((1, vec!["one".to_owned()])));
    h.press("escape");
    assert!(new_note_lines(&mut h).is_none());

    // A drag from it down to the third line: the range.
    h.pointer_down(plus.center(), gpui_kit::MouseButton::Left);
    h.pointer_move(
        crate::harness::at(x, third_y),
        Some(gpui_kit::MouseButton::Left),
    );
    h.pointer_up(crate::harness::at(x, third_y), gpui_kit::MouseButton::Left);
    h.settle();
    assert_eq!(
        new_note_lines(&mut h),
        Some((
            1,
            vec!["one".to_owned(), "two".to_owned(), "three".to_owned()]
        ))
    );
    h.press("escape");

    // The line numbers are what they were: a press there starts no note, and
    // a drag from them selects text to copy.
    let numbers = crate::harness::at(f32::from(plus.left()) - 30.0, first_y);
    h.click_point(numbers);
    h.settle();
    assert!(new_note_lines(&mut h).is_none());
    h.drag(numbers, crate::harness::at(x + 200.0, third_y));
    h.settle();
    h.press("cmd-c");
    let copied = h
        .with_window(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert!(copied.contains("two"), "{copied:?}");
}

#[test]
fn the_cursor_line_takes_a_note_from_the_keyboard() {
    let repo = scratch_repo("keyboard");
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    h.wait_until("the diff and notes", |ui| {
        ui.active().notes.settled() && !ui.active().session.document.files.is_empty()
    });
    h.act(|ui, cx| ui.run_command("lines.toggle", crate::commands::CommandArg::None, cx));
    h.press("j");
    h.press("n");
    h.settle();
    assert!(new_note_lines(&mut h).is_some_and(|(_, text)| text.len() == 1));
}

/// The rows the diff washes, as (first line, last line, strong).
fn washed(h: &mut Harness) -> Vec<(usize, usize, bool)> {
    h.read(|ui| {
        super::app::highlights(ui.active())
            .into_iter()
            .map(|highlight| (highlight.from.line, highlight.to.line, highlight.strong))
            .collect()
    })
}

#[test]
fn picked_lines_are_washed_while_dragging_composing_and_reading() {
    let repo = scratch_repo("wash");
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    h.wait_until("the diff and notes", |ui| {
        ui.active().notes.settled() && !ui.active().session.document.files.is_empty()
    });
    h.frames();
    let x = h.read(|ui| ui.left_pane_width() + ui.files_pane_width()) + 300.0;
    let mut rows = Vec::new();
    let mut y = 60.0;
    while y < 780.0 {
        h.hover_point(crate::harness::at(x, y));
        h.frames();
        if let Some(button) = add_button(&mut h)
            && rows
                .last()
                .is_none_or(|(_, last): &(f32, gpui_kit::Bounds<gpui_kit::Pixels>)| {
                    last.origin.y != button.origin.y
                })
        {
            rows.push((y, button));
        }
        y += 4.0;
    }
    assert_eq!(rows.len(), 3);
    assert!(washed(&mut h).is_empty());

    // Mid-drag: the range so far, strong.
    h.hover_point(crate::harness::at(x, rows[0].0));
    h.frames();
    let plus = add_button(&mut h).expect("the first line's +");
    h.pointer_down(plus.center(), gpui_kit::MouseButton::Left);
    h.pointer_move(
        crate::harness::at(x, rows[2].0),
        Some(gpui_kit::MouseButton::Left),
    );
    h.settle();
    assert_eq!(washed(&mut h), [(0, 2, true)]);
    h.pointer_up(
        crate::harness::at(x, rows[2].0),
        gpui_kit::MouseButton::Left,
    );
    h.settle();
    // Composing: still washed; the composer names the range.
    assert_eq!(washed(&mut h), [(0, 2, true)]);
    // Canceled: gone.
    h.press("escape");
    assert!(washed(&mut h).is_empty());

    // A shift-click extends the range being written.
    let press = |line, extend| crate::diff_view::LinePress {
        line,
        extend,
        one_side: false,
    };
    let at = |line| crate::diff_view::LineAt {
        file: 0,
        hunk: 0,
        line,
    };
    h.perform(Action::Notes(NotesMsg::StartNote(super::NoteTarget::Line(
        press(at(1), false),
    ))));
    assert_eq!(washed(&mut h), [(1, 1, true)]);
    h.perform(Action::Notes(NotesMsg::StartNote(super::NoteTarget::Line(
        press(at(2), true),
    ))));
    assert_eq!(washed(&mut h), [(1, 2, true)]);
    h.perform(Action::Notes(NotesMsg::ReplyEdited("Here.".to_owned())));
    h.perform(Action::Notes(NotesMsg::SendReply));
    h.wait_until("the note", |ui| {
        ui.active().notes.settled() && ui.active().notes.notes.len() == 1
    });
    // A folded thread washes nothing; opened or hovered, its lines softly.
    assert!(washed(&mut h).is_empty());
    let id = h.read(|ui| ui.active().notes.notes[0].thread.id.clone());
    h.perform(Action::Notes(NotesMsg::Toggle(id.clone())));
    assert_eq!(washed(&mut h), [(1, 2, false)]);
    h.perform(Action::Notes(NotesMsg::Toggle(id.clone())));
    h.perform(Action::Notes(NotesMsg::Hover(id.clone(), true)));
    assert_eq!(washed(&mut h), [(1, 2, false)]);
    h.perform(Action::Notes(NotesMsg::Hover(id, false)));
    assert!(washed(&mut h).is_empty());
}

#[test]
fn the_file_header_note_button_starts_a_file_note_and_counts_them() {
    let repo = scratch_repo("header-note");
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    h.wait_until("the diff and notes", |ui| {
        ui.active().notes.settled() && !ui.active().session.document.files.is_empty()
    });
    h.frames();
    let button = gpui_kit::ElementId::NamedInteger("diff-file-note".into(), 0);
    let narrow = h.bounds(button.clone());
    assert_eq!(narrow.size.width, narrow.size.height, "an icon alone");
    h.click_point(narrow.center());
    h.settle();
    let anchor = h.read(|ui| {
        let reply = ui.active().note_reply()?;
        let super::app::ReplyTo::New(note) = &reply.to else {
            return None;
        };
        note.anchor.clone()
    });
    let anchor = anchor.expect("a file note being written");
    assert_eq!(
        (anchor.path.as_str(), anchor.lines.is_none()),
        ("a.txt", true)
    );
    h.press("escape");
    // With notes on the file, the button counts them and still starts one.
    add(&repo, &["--as", "claude", "--file", "a.txt", "One."]);
    h.wait_until("the note", |ui| ui.active().notes.notes.len() == 1);
    h.frames();
    let wide = h.bounds(button.clone());
    assert!(
        wide.size.width > wide.size.height,
        "the count shows: {wide:?}"
    );
    h.click_point(wide.center());
    h.settle();
    assert!(h.read(|ui| ui.active().note_reply().is_some()));
}

#[test]
fn the_notes_view_shows_a_threads_lines_as_the_diff_draws_them() {
    let repo = scratch_repo("excerpt");
    std::fs::write(
        repo.join("a.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
    )
    .unwrap();
    jj(&repo, &["status"]);
    jj(&repo, &["new", "-m", "Change five"]);
    std::fs::write(
        repo.join("a.txt"),
        "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nten\n",
    )
    .unwrap();
    jj(&repo, &["status"]);
    let id = add(
        &repo,
        &[
            "--as",
            "claude",
            "--file",
            "a.txt",
            "--line",
            "5",
            "Shouting.",
        ],
    );
    let mut h = Harness::open(Some(&repo));
    h.wait_for_graph();
    h.wait_until("the notes", |ui| {
        ui.active().notes.settled() && ui.active().notes.notes.len() == 1
    });
    h.perform(Action::SetMainView(crate::MainView::Notes));
    h.perform(Action::Notes(NotesMsg::SelectThread(id)));
    h.wait_until("the excerpt", |ui| {
        ui.active().notes.view.excerpt_file.is_some()
    });
    let rows = h.read(|ui| {
        ui.active().notes.view.excerpt[0]
            .lines
            .iter()
            .map(|line| {
                (
                    line.kind,
                    line.old_line,
                    line.new_line,
                    line.content.clone(),
                )
            })
            .collect::<Vec<_>>()
    });
    use diffui_core::DiffLineKind::{Addition, Context, Deletion};
    // The changed line both ways, three rows of context either side, both
    // sides' numbers.
    assert_eq!(rows.first().map(|row| row.3.as_str()), Some("two"));
    assert_eq!(rows.last().map(|row| row.3.as_str()), Some("eight"));
    assert!(
        rows.contains(&(Deletion, Some(5), None, "five".to_owned())),
        "{rows:?}"
    );
    assert!(
        rows.contains(&(Addition, None, Some(5), "FIVE".to_owned())),
        "{rows:?}"
    );
    assert!(
        rows.contains(&(Context, Some(4), Some(4), "four".to_owned())),
        "{rows:?}"
    );
    h.frames();
    let open = h.bounds("notes-view-open");
    assert!(open.size.height > gpui_kit::px(10.0));
}

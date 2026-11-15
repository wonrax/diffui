//! Patches and compares in the real app (the offscreen harness), the
//! handover to a running app over its socket, and diffui as jj's and git's
//! diff tool end to end: the real jj and git run this test binary as their
//! tool, which then waits on the harness's tab.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use diffui_core::patch::{DiffInput, PatchOrigin};

use crate::ctl::{Incoming, Request};
use crate::diff_view::{LineAt, LinePress};
use crate::harness::{Harness, fixture_jj, scratch_home};
use crate::notes::{NoteTarget, NotesMsg};
use crate::review::{Scope, Steps};
use crate::{Action, Diffui, TabSource};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("core/tests/fixtures/patches")
        .join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = scratch_home().join("opened").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    dir
}

fn loaded(ui: &Diffui) -> bool {
    let session = &ui.active().session;
    session.outline.is_some()
        && matches!(session.status, crate::LoadStatus::Loaded)
        && !session.diff_in_flight()
        && !session.document.files.is_empty()
}

/// What `ctl state` calls the active tab.
fn kind(ui: &Diffui) -> String {
    ui.ctl_state()["repos"]
        .as_array()
        .and_then(|repos| repos.iter().find(|repo| repo["active"] == true))
        .and_then(|repo| repo["kind"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn header(h: &mut Harness) -> crate::commit_info::Header {
    h.read(|ui| {
        let theme = ui.resolved_theme().spec();
        crate::commit_info::header(ui, ui.look(theme)).expect("a header")
    })
}

fn paths(h: &mut Harness) -> Vec<String> {
    h.read(|ui| {
        ui.active()
            .session
            .document
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect()
    })
}

#[test]
fn a_format_patch_email_opens_as_a_patch_tab_with_its_message() {
    let mut h = Harness::open(None);
    h.act(|ui, cx| {
        ui.open_diff(
            DiffInput::Patch(PatchOrigin::File(fixture("format-patch.patch"))),
            cx,
        )
    });
    h.wait_until("the patch", loaded);
    assert_eq!(h.read(kind), "patch");
    assert_eq!(h.read(|ui| ui.active_steps()), Steps::None);
    assert_eq!(
        h.read(|ui| ui.tabs[ui.active].name.clone()),
        "format-patch.patch"
    );
    let header = header(&mut h);
    assert_eq!(header.title.as_deref(), Some("Shout beta and add delta"));
    assert_eq!(
        header.description,
        "The list reads better with one loud entry.\nDelta was missing."
    );
    assert_eq!(header.kind, crate::review::TextKind::Message);
    // The signature under the hunk isn't a row of it.
    let rows = h.read(|ui| ui.active().session.document.files[0].hunks[0].lines.len());
    assert_eq!((paths(&mut h), rows), (vec!["list.txt".to_owned()], 5));
}

#[test]
fn a_series_steps_through_its_patches_under_its_cover_letter() {
    let mut h = Harness::open(None);
    h.act(|ui, cx| {
        ui.open_diff(
            DiffInput::Patch(PatchOrigin::File(fixture("series.mbox"))),
            cx,
        )
    });
    h.wait_until("the series", loaded);
    assert!(matches!(
        h.read(|ui| ui.active_steps()),
        Steps::List(names) if names.step == "Patch"
    ));
    let cover = header(&mut h);
    assert_eq!(cover.title.as_deref(), Some("Make the list louder"));
    assert_eq!(cover.kind, crate::review::TextKind::Markdown);
    assert_eq!(
        paths(&mut h),
        ["copy.txt", "list.txt"],
        "the whole: each path once"
    );
    h.perform(Action::Review(crate::review::ReviewAction::Scope(
        Scope::Step(1),
    )));
    h.wait_until("the second patch", |ui| {
        ui.active().scope() == Some(Scope::Step(1)) && loaded(ui)
    });
    assert_eq!(header(&mut h).title.as_deref(), Some("Copy the list"));
    assert_eq!(paths(&mut h), ["copy.txt"]);
}

#[test]
fn the_clipboard_and_piped_text_open_as_patch_tabs() {
    let mut h = Harness::open(None);
    let text = std::fs::read_to_string(fixture("svn.diff")).unwrap();
    h.with_window(|_, cx| crate::write_clipboard(text.clone(), cx));
    h.perform(Action::OpenClipboardPatch);
    h.wait_until("the clipboard's patch", loaded);
    assert_eq!(h.read(kind), "patch");
    assert_eq!(h.read(|ui| ui.tabs[ui.active].name.clone()), "Clipboard");
    assert_eq!(header(&mut h).title.as_deref(), Some("Clipboard"));

    h.with_window(|_, cx| crate::write_clipboard("not a diff".to_owned(), cx));
    h.perform(Action::OpenClipboardPatch);
    assert_eq!(h.read(|ui| ui.tabs.len()), 1, "nothing opens for prose");

    h.act(|ui, cx| ui.ctl_patch(None, Some(text), cx))
        .expect("opens");
    h.wait_until("the piped patch", loaded);
    assert_eq!(h.read(|ui| ui.tabs[ui.active].name.clone()), "stdin");
    assert_eq!(h.read(|ui| ui.tabs.len()), 2);
}

/// What tells a patch from a repository on the command line.
#[test]
fn a_named_path_is_a_patch_only_by_its_name_or_outside_a_repository() {
    assert_eq!(
        super::patch_origin(Path::new("-")),
        Some(PatchOrigin::Stdin)
    );
    assert!(matches!(
        super::patch_origin(&fixture("series.mbox")),
        Some(PatchOrigin::File(_))
    ));
    // A source file in a repository opens the repository, as before.
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    assert_eq!(super::patch_origin(&source), None);
    // A folder, a repository or not, is never a patch.
    assert_eq!(
        super::patch_origin(Path::new(env!("CARGO_MANIFEST_DIR"))),
        None
    );
    // A diff with no telling name, outside any repository, reads as one.
    let dir = scratch("sniff");
    let loose = dir.join("changes.txt");
    std::fs::copy(fixture("diff-u.diff"), &loose).unwrap();
    assert_eq!(
        super::patch_origin(&loose),
        Some(PatchOrigin::File(loose.clone()))
    );
    let prose = dir.join("notes.txt");
    std::fs::write(&prose, "just words\n").unwrap();
    assert_eq!(super::patch_origin(&prose), None);
    // Repositories and PR references don't hand off: they launch as before.
    assert_eq!(super::hand_off(&[&PathBuf::from(".")]), None);
    assert_eq!(super::hand_off(&[&PathBuf::from("owner/repo#12")]), None);
    assert_eq!(super::hand_off(&[]), None);
}

/// The socket a test's "running app" answers on, and its requests.
struct App {
    dir: PathBuf,
    socket: PathBuf,
    requests: futures::channel::mpsc::UnboundedReceiver<Incoming>,
}

impl App {
    fn start(name: &str) -> Self {
        // Unix socket paths are short: not under the scratch home.
        let dir = std::env::temp_dir().join(format!("diffui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s");
        let (sender, requests) = futures::channel::mpsc::unbounded();
        crate::ctl::serve(&socket, sender).expect("binds");
        Self {
            dir,
            socket,
            requests,
        }
    }

    /// Where a process whose HOME is `home` looks for the running app.
    fn advertise(&self, home: &Path) {
        let file = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/diffui/ctl-socket")
        } else {
            home.join(".local/state/diffui/ctl-socket")
        };
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, self.socket.as_os_str().as_encoded_bytes()).unwrap();
    }

    /// The next request, while `child` (when there is one) still runs.
    fn next(&mut self, mut child: Option<&mut Child>) -> Incoming {
        let start = Instant::now();
        loop {
            if let Ok(incoming) = self.requests.try_recv() {
                return incoming;
            }
            if let Some(child) = child.as_deref_mut() {
                let status = child.try_wait().unwrap();
                assert!(
                    status.is_none(),
                    "the tool exited without asking: {status:?}"
                );
            }
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "no request came"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A second `diffui` with a patch hands it to the running app and is done.
#[test]
fn a_second_diffui_hands_its_patch_to_the_running_app() {
    let mut h = Harness::open(None);
    let mut app = App::start("handoff");
    let socket = app.socket.clone();
    let request = Request::Patch {
        path: Some(fixture("hg.diff").display().to_string()),
        text: None,
    };
    let client = std::thread::spawn(move || super::wait_on(&socket, &request));
    let (request, reply) = app.next(None);
    h.act(|ui, cx| ui.on_ctl_request(request, reply, cx));
    client.join().unwrap().expect("the app took it");
    h.wait_until("the handed patch", loaded);
    assert_eq!(h.read(kind), "patch");
    assert_eq!(header(&mut h).title.as_deref(), Some("Count the bugs"));
}

/// Kills the tool's process if the test fails before it ends, so no
/// process outlives the run.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// The tool's command line: this test binary, running only
/// [`as_the_diff_tool`], with `diffui diff`'s arguments after a marker.
const TOOL_TEST: &str = "opened::scenarios::as_the_diff_tool";
const MARK: &str = "diffui-argv";

/// Run by jj or git as their diff tool: `diffui diff LEFT RIGHT`.
#[test]
#[ignore = "the diff tool tests start it"]
fn as_the_diff_tool() {
    let args: Vec<OsString> = std::env::args_os()
        .skip_while(|arg| arg != MARK)
        .skip(2)
        .collect();
    if args.is_empty() {
        return;
    }
    let code = super::diff_main(std::iter::once(OsString::from("diffui diff")).chain(args));
    std::process::exit(code);
}

/// The tool's tab opens and its folders read; the tool waits on it;
/// closing it lets the tool exit, and its folders go.
fn drive_the_tool(mut h: Harness, mut app: App, child: Child, by: &str) {
    let mut child = Reaped(child);
    let (request, reply) = app.next(Some(&mut child.0));
    let Request::Compare {
        left,
        right,
        by: waiting,
    } = &request
    else {
        panic!("not a compare: {request:?}");
    };
    assert_eq!(waiting.as_deref(), Some(by));
    let (left, right) = (PathBuf::from(left), PathBuf::from(right));
    h.act(|ui, cx| ui.on_ctl_request(request, reply, cx));
    h.wait_until("the comparison", loaded);
    assert_eq!(h.read(kind), "compare");
    let rows: Vec<(diffui_core::DiffLineKind, String)> = h.read(|ui| {
        let file = &ui.active().session.document.files[0];
        assert_eq!(file.path, "a.txt");
        file.hunks[0]
            .lines
            .iter()
            .map(|row| (row.kind, row.content.clone()))
            .collect()
    });
    assert_eq!(
        rows,
        [
            (diffui_core::DiffLineKind::Deletion, "one".to_owned()),
            (diffui_core::DiffLineKind::Addition, "two".to_owned()),
        ]
    );
    assert!(h.read(|ui| ui.active().waiting.is_waited_on()));
    // Still waiting, its folders still there, however long the review.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "{by} stopped waiting"
    );
    assert!(std::fs::read_to_string(left.join("a.txt")).is_ok());
    assert!(std::fs::read_to_string(right.join("a.txt")).is_ok());

    h.perform(Action::CloseActiveTab);
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "{by} never ended"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{by} failed: {status:?}");
    assert!(
        !left.exists(),
        "{by} cleans its folders up once the tool exits"
    );
}

/// The config `--help` gives for a tool, from its first line to the blank
/// line after; the agent guide gives the same lines.
fn documented(first: &str) -> String {
    let lines: Vec<&str> = super::TOOL_HELP
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != first)
        .take_while(|line| !line.is_empty())
        .collect();
    assert!(!lines.is_empty(), "--help has no {first}");
    for line in &lines {
        let guide = crate::ctl::topic("patches").expect("the patches topic");
        assert!(guide.contains(line), "patches.md lacks {line}");
    }
    lines.join("\n") + "\n"
}

/// `text` with `from` replaced, which must be in it.
fn swap(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "{from} isn't in {text}");
    text.replace(from, to)
}

/// A HOME and config for jj or git that nothing of the user's reaches.
fn tool_env(command: &mut Command, home: &Path) {
    command
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join("gitconfig"))
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
}

fn run(command: &mut Command) {
    let output = command.output().expect("runs");
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn jj_diff_tool_waits_on_its_tab() {
    let h = Harness::open(None);
    let app = App::start("jj-tool");
    let dir = scratch("jj-tool");
    let (home, repo) = (dir.join("home"), dir.join("repo"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    app.advertise(&home);
    let exe = std::env::current_exe().unwrap();
    let config = dir.join("jj.toml");
    // The documented config, with this test binary in diffui's place.
    let tool = swap(
        &swap(
            &documented("[merge-tools.diffui]"),
            "program = \"diffui\"",
            &format!("program = {exe:?}"),
        ),
        "diff-args = [\"diff\"",
        &format!(
            "diff-args = [\"--exact\", \"--ignored\", \"--nocapture\", \"{TOOL_TEST}\", \"{MARK}\", \"diff\""
        ),
    );
    std::fs::write(
        &config,
        format!("[user]\nname = \"Test\"\nemail = \"test@example.com\"\n\n{tool}"),
    )
    .unwrap();
    let jj = |args: &[&str]| {
        let mut command = Command::new(fixture_jj());
        command
            .args(args)
            .current_dir(&repo)
            .env("JJ_CONFIG", &config);
        tool_env(&mut command, &home);
        command
    };
    run(&mut jj(&["git", "init"]));
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    run(&mut jj(&["commit", "-m", "first"]));
    std::fs::write(repo.join("a.txt"), "two\n").unwrap();
    let child = jj(&["diff", "--tool", "diffui"])
        .spawn()
        .expect("jj starts");
    drive_the_tool(h, app, child, "jj");
}

#[test]
fn git_difftool_dir_diff_waits_on_its_tab() {
    let h = Harness::open(None);
    let app = App::start("git-tool");
    let dir = scratch("git-tool");
    let (home, repo) = (dir.join("home"), dir.join("repo"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    app.advertise(&home);
    let exe = std::env::current_exe().unwrap();
    // The documented config, with this test binary in diffui's place.
    let tool = swap(
        &documented("[difftool \"diffui\"]"),
        "cmd = diffui diff",
        &format!("cmd = {exe:?} --exact --ignored --nocapture {TOOL_TEST} {MARK} diff"),
    );
    std::fs::write(home.join("gitconfig"), tool).unwrap();
    let git = |args: &[&str]| {
        let mut command = Command::new("git");
        command.args(args).current_dir(&repo);
        tool_env(&mut command, &home);
        command
    };
    run(&mut git(&["init", "-q"]));
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    run(&mut git(&["add", "a.txt"]));
    run(&mut git(&["commit", "-qm", "first"]));
    std::fs::write(repo.join("a.txt"), "two\n").unwrap();
    let child = git(&["difftool", "--dir-diff", "--tool", "diffui", "--no-prompt"])
        .spawn()
        .expect("git starts");
    drive_the_tool(h, app, child, "git");
}

fn press(file: usize, line: usize) -> NoteTarget {
    NoteTarget::Line(LinePress {
        line: LineAt {
            file,
            hunk: 0,
            line,
        },
        extend: false,
        one_side: false,
    })
}

fn write_note(h: &mut Harness, target: NoteTarget, text: &str) {
    h.perform(Action::Notes(NotesMsg::StartNote(target)));
    h.perform(Action::Notes(NotesMsg::ReplyEdited(text.to_owned())));
    h.perform(Action::Notes(NotesMsg::SendReply));
}

/// Notes on a patch: started and replied to as on any diff, copied, said
/// to be temporary, and gone with the tab.
#[test]
fn notes_on_a_patch_live_in_its_tab() {
    let mut h = Harness::open(None);
    let input = DiffInput::Patch(PatchOrigin::File(fixture("format-patch.patch")));
    h.act(|ui, cx| ui.open_diff(input.clone(), cx));
    h.wait_until("the patch", loaded);
    // On the added `delta` line: row 4 of the one hunk.
    write_note(&mut h, press(0, 4), "Why delta?");
    h.wait_until("the note", |ui| ui.active().notes.notes.len() == 1);
    assert!(
        h.read(|ui| ui
            .toasts
            .iter()
            .any(|toast| toast.title.to_string().contains("aren't saved"))),
        "the first note says it isn't kept"
    );
    let (id, spot) = h.read(|ui| {
        let tab = ui.active();
        let note = &tab.notes.notes[0];
        (
            note.thread.id.clone(),
            crate::notes::app::spot_of(tab, note),
        )
    });
    assert_eq!(
        spot,
        Some(crate::diff_view::DocSpot::Line(LineAt {
            file: 0,
            hunk: 0,
            line: 4
        }))
    );
    assert_eq!(h.read(|ui| crate::notes::app::spots(ui).len()), 1);
    h.frames();
    assert!(
        h.bounds("opened-copy-notes").size.width > gpui_kit::px(0.0),
        "the hint shows"
    );

    h.perform(Action::Notes(NotesMsg::StartReply(id)));
    h.perform(Action::Notes(NotesMsg::ReplyEdited(
        "It was missing.".to_owned(),
    )));
    h.perform(Action::Notes(NotesMsg::SendReply));
    h.wait_until("the reply", |ui| {
        ui.active().notes.notes[0].thread.messages.len() == 2
    });

    h.perform(Action::Notes(NotesMsg::CopyChange { resolved: true }));
    let copied = h
        .with_window(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default();
    assert!(copied.contains("## Shout beta and add delta"), "{copied}");
    assert!(
        copied.contains("`list.txt:4`") && copied.contains("It was missing."),
        "{copied}"
    );
    assert!(!copied.contains("Change `"), "no key from inside the tab");

    // Closing doesn't ask, and a new tab of the same patch starts clean.
    h.perform(Action::CloseActiveTab);
    assert!(h.read(|ui| ui.tabs.is_empty()));
    h.act(|ui, cx| ui.open_diff(input, cx));
    h.wait_until("the patch again", loaded);
    h.settle();
    assert!(h.read(|ui| ui.active().notes.notes.is_empty()));
}

/// A compare of folders the user named reloads on an edit, and a note
/// follows its lines there.
#[test]
fn a_named_compare_refreshes_and_its_notes_follow_their_lines() {
    let mut h = Harness::open(None);
    let dir = scratch("refresh");
    let (left, right) = (dir.join("old"), dir.join("new"));
    std::fs::create_dir_all(&left).unwrap();
    std::fs::create_dir_all(&right).unwrap();
    std::fs::write(left.join("f.txt"), "a\nb\nc\n").unwrap();
    std::fs::write(right.join("f.txt"), "a\nB\nc\n").unwrap();
    h.act(|ui, cx| {
        ui.open_diff(
            DiffInput::Compare {
                left: left.clone(),
                right: right.clone(),
            },
            cx,
        )
    });
    h.wait_until("the comparison", loaded);
    assert!(matches!(
        h.read(|ui| ui.tabs[ui.active].source.clone()),
        TabSource::Diff(DiffInput::Compare { .. })
    ));
    let added = h.read(|ui| {
        ui.active().session.document.files[0].hunks[0]
            .lines
            .iter()
            .position(|row| row.content == "B")
            .unwrap()
    });
    write_note(&mut h, press(0, added), "Capital?");
    h.wait_until("the note", |ui| ui.active().notes.notes.len() == 1);

    // The watch comes up a moment after the tab: save until it sees one.
    let start = Instant::now();
    loop {
        std::fs::write(right.join("f.txt"), "new first\na\nB\nc\n").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        h.pump();
        let refreshed = h.read(|ui| {
            ui.active().session.document.files[0].hunks[0]
                .lines
                .iter()
                .any(|row| row.content == "new first")
        });
        if refreshed {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "no refresh");
    }
    h.wait_until("the note on its moved line", |ui| {
        matches!(
            ui.active()
                .notes
                .notes
                .first()
                .map(|note| &note.located.place),
            Some(crate::notes::locate::Place::Lines { start: 3, .. })
        )
    });
}

/// The toolbar offers what each source can do: Refresh reads a patch
/// file or a compare again, not stdin or the clipboard; only a repository
/// fetches or shows its sync status.
#[test]
fn refresh_reads_a_patch_file_again_and_nothing_fetches() {
    let mut h = Harness::open(None);
    let dir = scratch("refresh-patch");
    let file = dir.join("fix.patch");
    std::fs::copy(fixture("format-patch.patch"), &file).unwrap();
    h.act(|ui, cx| ui.open_diff(DiffInput::Patch(PatchOrigin::File(file.clone())), cx));
    h.wait_until("the patch", loaded);
    let actions = h.read(|ui| ui.active_actions());
    assert_eq!(
        (actions.refresh, actions.fetch),
        (true, false),
        "a patch file reads again"
    );
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, text.replace("Shout beta", "Whisper beta")).unwrap();
    h.perform(Action::Refresh);
    h.wait_until("the patch read again", |ui| {
        let theme = ui.resolved_theme().spec();
        crate::commit_info::header(ui, ui.look(theme))
            .and_then(|header| header.title)
            .as_deref()
            == Some("Whisper beta and add delta")
    });

    h.act(|ui, cx| ui.ctl_patch(None, Some(text), cx))
        .expect("opens");
    h.wait_until("the piped patch", loaded);
    let actions = h.read(|ui| ui.active_actions());
    assert_eq!(
        (actions.refresh, actions.fetch),
        (false, false),
        "stdin can't"
    );
    assert!(
        h.read(|ui| {
            ui.registry
                .iter()
                .find(|command| command.id.as_str() == "repo.refresh")
                .is_some_and(|command| {
                    command
                        .enabled(ui, &crate::commands::CommandArg::None)
                        .is_err()
                })
        }),
        "nor can its command"
    );
}

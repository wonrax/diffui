//! Diffs opened without a repository: a patch (`diffui -`, `diffui
//! fix.patch`, the clipboard) or two files or folders compared (`diffui diff
//! LEFT RIGHT`), which is how jj and git run diffui as their diff tool.
//!
//! A second `diffui` hands its tab to the app already running, over the
//! `ctl` socket. `diffui diff` then waits until that tab closes: jj and git
//! delete the folders they compare once their tool exits.

#[cfg(unix)]
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use diffui_core::patch::{DiffInput, PatchOrigin};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, div};
use serde_json::{Value, json};

use crate::ctl::{Reply, Request};
use crate::ui::Look;
use crate::{Action, Cx, Diffui, TabId, TabSource, icons, ui};

/// What `diffui diff --help` and the main `--help` say about being a diff
/// tool, with the config that sets it up.
pub(crate) const TOOL_HELP: &str = "\
Compare two files or two folders. diffui waits until the comparison's tab \
closes, which is what jj and git need from a diff tool.

As jj's diff tool (~/.config/jj/config.toml):

  [merge-tools.diffui]
  program = \"diffui\"
  diff-args = [\"diff\", \"$left\", \"$right\"]
  diff-invocation-mode = \"dir\"

  then: jj diff --tool diffui

As git's (~/.gitconfig):

  [difftool \"diffui\"]
  cmd = diffui diff \"$LOCAL\" \"$REMOTE\"

  then: git difftool --dir-diff --tool diffui";

/// The rest of the main `--help`: patches, the diff tool, then the agent
/// guide.
pub(crate) const AFTER_HELP: &str = "\
Patches: `diffui -` reads a diff from stdin; `diffui fix.patch` opens a \
patch file (any unified diff, or git format-patch emails). A path opens as a \
patch when it ends in .patch, .diff, .mbox or .eml, or when it's a file no \
repository holds and it reads as a diff; every other path opens its \
repository. With diffui already running, the patch opens as a tab there.

`diffui diff LEFT RIGHT` compares two files or two folders (see `diffui diff \
--help` for the jj and git diff tool config).

For agents and scripts: `diffui skill` prints a guide to `diffui notes` \
(review notes), `diffui ctl` (drive the running app), `diffui plugin` and \
`diffui jj`.";

/// Whether `path`, named on the command line, is a patch rather than a
/// repository: `-` is stdin; a file ending in `.patch`, `.diff`, `.mbox`
/// or `.eml` is a patch; another file is one only when no repository holds
/// it and it reads as a diff, so `diffui src/main.rs` still opens its
/// repository scoped to that file.
pub(crate) fn patch_origin(path: &Path) -> Option<PatchOrigin> {
    if path.as_os_str() == "-" {
        return Some(PatchOrigin::Stdin);
    }
    if !path.is_file() {
        return None;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let named = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "patch" | "diff" | "mbox" | "eml"));
    if named {
        return Some(PatchOrigin::File(absolute));
    }
    if diffui_core::prepare_repository(path).is_ok() {
        return None;
    }
    let mut head = Vec::new();
    let file = std::fs::File::open(path).ok()?;
    std::io::Read::read_to_end(&mut std::io::Read::take(file, 64 * 1024), &mut head).ok()?;
    diffui_core::patch::looks_like_patch(&String::from_utf8_lossy(&head))
        .then_some(PatchOrigin::File(absolute))
}

/// Text handed over whole (piped into another `diffui`, or the clipboard),
/// told apart from the others by a number.
pub(crate) fn text_origin(name: &str, text: String) -> PatchOrigin {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    PatchOrigin::Text {
        name: name.to_owned(),
        id: NEXT.fetch_add(1, Ordering::Relaxed),
        text: Arc::from(text),
    }
}

/// A path's last name, or the whole path when it has none.
fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// What a tab of `input` is called: the dimmed part, then the name.
pub(crate) fn tab_label(input: &DiffInput) -> (String, String) {
    match input {
        DiffInput::Patch(PatchOrigin::File(path)) => {
            (path.parent().map(name).unwrap_or_default(), name(path))
        }
        DiffInput::Patch(origin) => (String::new(), origin.name()),
        DiffInput::Compare { left, right } => (String::new(), compare_title(left, right)),
    }
}

/// `old → new`, by the two sides' names; by their folders' too when the
/// names alone don't tell them apart (jj names its sides `left` and
/// `right`).
pub(crate) fn compare_title(left: &Path, right: &Path) -> String {
    let (mut from, mut to) = (name(left), name(right));
    if from == to || (from == "left" && to == "right") {
        let parent = |path: &Path| path.parent().map(name).unwrap_or_default();
        (from, to) = (
            format!("{}/{from}", parent(left)),
            format!("{}/{to}", parent(right)),
        );
    }
    format!("{from} \u{2192} {to}")
}

/// The commands a `diffui diff` is waiting on to finish, by tab: each gets
/// its answer when the tab closes (the app quitting ends them too).
#[derive(Debug, Default)]
pub(crate) struct Waiting {
    waiters: Vec<Waiter>,
    /// What's waiting, for the banner: "jj", "git", or none known.
    by: Option<String>,
}

impl Waiting {
    pub(crate) fn is_waited_on(&self) -> bool {
        !self.waiters.is_empty()
    }
}

/// A `diffui diff` waiting on its tab; dropped with the tab, it answers.
#[derive(Debug)]
struct Waiter(Option<Reply>);

impl Drop for Waiter {
    fn drop(&mut self) {
        if let Some(reply) = self.0.take() {
            let _ = reply.send(json!({ "ok": true, "closed": true }));
        }
    }
}

impl Diffui {
    /// Open `input` as a tab, or bring forward the one already showing it.
    pub(crate) fn open_diff(&mut self, input: DiffInput, cx: &mut Cx) -> TabId {
        let source = TabSource::Diff(input.clone());
        if let Some(existing) = self.tabs.iter().find(|tab| tab.source == source) {
            let id = existing.id;
            self.activate_tab(id, cx);
            return id;
        }
        let (owner, name) = tab_label(&input);
        self.open_source_tab(owner, name, source, cx)
    }

    /// Open the clipboard's text as a patch.
    pub(crate) fn open_clipboard_patch(&mut self, cx: &mut Cx) {
        let text = cx.read_from_clipboard().and_then(|item| item.text());
        match text {
            Some(text) if diffui_core::patch::looks_like_patch(&text) => {
                self.open_diff(DiffInput::Patch(text_origin("Clipboard", text)), cx);
            }
            _ => self.push_info_toast("The clipboard holds no diff", ""),
        }
    }

    /// `ctl patch`: a patch another `diffui` handed over, by path or whole.
    pub(crate) fn ctl_patch(
        &mut self,
        path: Option<String>,
        text: Option<String>,
        cx: &mut Cx,
    ) -> Result<Value, String> {
        let origin = match (path, text) {
            (Some(path), _) => PatchOrigin::File(PathBuf::from(path)),
            (None, Some(text)) => text_origin("stdin", text),
            (None, None) => return Err("Name a patch file or send its text".to_owned()),
        };
        let tab = self.open_diff(DiffInput::Patch(origin), cx);
        Ok(json!({ "tab": tab.0 }))
    }

    /// `ctl compare`: open the comparison and answer `reply` once its tab
    /// closes.
    pub(crate) fn ctl_compare(
        &mut self,
        left: String,
        right: String,
        by: Option<String>,
        reply: Reply,
        cx: &mut Cx,
    ) {
        let (left, right) = (PathBuf::from(left), PathBuf::from(right));
        for side in [&left, &right] {
            if !side.exists() {
                let _ = reply.send(crate::ctl::failure(format!(
                    "{} doesn't exist",
                    side.display()
                )));
                return;
            }
        }
        let tab = self.open_diff(DiffInput::Compare { left, right }, cx);
        if let Some(state) = self.tab_mut(tab) {
            state.waiting.waiters.push(Waiter(Some(reply)));
            state.waiting.by = by.or(state.waiting.by.take());
        }
    }
}

/// The strips atop a diff with no repository: the command waiting on the
/// tab, and that the tab's notes aren't kept.
pub(crate) fn banners(ui: &Diffui, look: Look, cx: &mut Cx) -> Vec<AnyElement> {
    let Look { theme, .. } = look;
    let mut out = Vec::new();
    let tab = ui.active();
    if tab.waiting.is_waited_on() {
        let by = tab.waiting.by.as_deref().unwrap_or("A command");
        out.push(
            ui::Banner::new(ui::Tone::Accent)
                .shape(ui::BannerShape::Strip)
                .icon(icons::TERMINAL)
                .child(
                    div()
                        .text_color(theme.text)
                        .child(format!("{by} is waiting for this comparison")),
                )
                .end(
                    look.button("opened-done", "Done")
                        .ghost()
                        .compact()
                        .on_click(Diffui::on_action(Action::CloseActiveTab, cx))
                        .tip(format!("Close the tab and let {by} go on")),
                )
                .build(look)
                .into_any_element(),
        );
    }
    if crate::notes::app::in_memory(ui) && !tab.notes.notes.is_empty() {
        out.push(
            ui::Banner::new(ui::Tone::Muted)
                .shape(ui::BannerShape::Strip)
                .icon(icons::MESSAGES)
                .child(div().text_color(theme.muted_text).child(
                    "Notes here aren't saved: they go when the tab closes or diffui quits. Copy keeps them.",
                ))
                .end(
                    look.button("opened-copy-notes", "Copy")
                        .ghost()
                        .compact()
                        .on_click(Diffui::on_action(
                            Action::Notes(crate::notes::NotesMsg::CopyChange { resolved: true }),
                            cx,
                        ))
                        .tip("Copy this diff's notes as Markdown"),
                )
                .build(look)
                .into_any_element(),
        );
    }
    out
}

// ── The command line ────────────────────────────────────────────────────────

#[derive(Debug, clap::Parser)]
#[command(name = "diffui diff", about = "Compare two files or two folders", long_about = TOOL_HELP)]
struct DiffCli {
    /// The old side: a file or a folder.
    left: PathBuf,
    /// The new side, of the same kind.
    right: PathBuf,
}

/// `diffui diff LEFT RIGHT`: the exit code, or `None` for any other launch.
pub(crate) fn client_main() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != "diff" {
        return None;
    }
    Some(diff_main(
        std::iter::once(std::ffi::OsString::from("diffui diff")).chain(args),
    ))
}

fn diff_main(args: impl IntoIterator<Item = std::ffi::OsString>) -> i32 {
    use clap::Parser as _;
    let cli = DiffCli::parse_from(args);
    let absolute = |path: &Path| std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let request = Request::Compare {
        left: absolute(&cli.left).display().to_string(),
        right: absolute(&cli.right).display().to_string(),
        by: waiting_command(),
    };
    let result = match crate::ctl::running_socket() {
        Some(socket) => wait_on(&socket, &request),
        None => start_app().and_then(|socket| wait_on(&socket, &request)),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("diffui diff: {error}");
            1
        }
    }
}

/// Hand the patches `paths` names to the app already running, when every
/// one of them is a patch: the exit code, or `None` to start the app
/// (nothing is running, or something named isn't a patch).
pub(crate) fn hand_off(paths: &[&PathBuf]) -> Option<i32> {
    let origins: Vec<PatchOrigin> = paths
        .iter()
        .map(|path| patch_origin(path))
        .collect::<Option<_>>()?;
    if origins.is_empty() {
        return None;
    }
    let socket = crate::ctl::running_socket()?;
    for origin in origins {
        let request = match origin {
            PatchOrigin::File(path) => Request::Patch {
                path: Some(path.display().to_string()),
                text: None,
            },
            PatchOrigin::Stdin => {
                let mut bytes = Vec::new();
                if let Err(error) = std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes) {
                    eprintln!("diffui: couldn't read stdin: {error}");
                    return Some(1);
                }
                Request::Patch {
                    path: None,
                    text: Some(String::from_utf8_lossy(&bytes).into_owned()),
                }
            }
            PatchOrigin::Text { .. } => continue,
        };
        if let Err(error) = wait_on(&socket, &request) {
            eprintln!("diffui: {error}");
            return Some(1);
        }
    }
    Some(0)
}

/// Send `request` to the app at `socket` and wait for its answer, as long
/// as it takes: a compare answers when its tab closes. The app quitting
/// first is an answer too.
pub(crate) fn wait_on(socket: &Path, request: &Request) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(socket)
            .map_err(|_| "diffui isn't running (start it, then try again)".to_owned())?;
        let line = serde_json::to_string(request).map_err(|error| error.to_string())?;
        writeln!(stream, "{line}").map_err(|error| error.to_string())?;
        let mut answer = String::new();
        let read = BufReader::new(&stream)
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(());
        }
        let answer: Value =
            serde_json::from_str(&answer).map_err(|error| format!("bad answer: {error}"))?;
        if answer["ok"] == Value::Bool(true) {
            Ok(())
        } else {
            Err(answer["error"].as_str().unwrap_or("failed").to_owned())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, request);
        Err("diffui diff needs a unix platform".to_owned())
    }
}

/// Start the app in the background, as a launcher would, and wait for its
/// socket. It outlives this process, as `code --wait` leaves its editor.
fn start_app() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = std::process::Command::new(exe);
    // From `/`, so it doesn't also open the repository this runs in.
    command
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
        .spawn()
        .map_err(|error| format!("couldn't start diffui: {error}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if let Some(socket) = crate::ctl::running_socket() {
            return Ok(socket);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err("diffui didn't start".to_owned())
}

/// Which of `jj` and `git` started this process, looking a few parents up
/// (git runs its tool through a shell).
fn waiting_command() -> Option<String> {
    #[cfg(unix)]
    {
        let mut pid = std::os::unix::process::parent_id();
        for _ in 0..4 {
            let output = std::process::Command::new("ps")
                .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
                .output()
                .ok()?;
            let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let (parent, command) = line.split_once(char::is_whitespace)?;
            let name = Path::new(command.trim())
                .file_name()?
                .to_string_lossy()
                .into_owned();
            if name == "jj" || name == "git" {
                return Some(name);
            }
            pid = parent.trim().parse().ok()?;
        }
    }
    None
}

#[cfg(all(test, unix))]
#[path = "opened_scenarios.rs"]
mod scenarios;

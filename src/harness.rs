//! See your work without a display: the real root [`Diffui`] in a window rendered
//! through the offscreen renderer, on a scratch repository with a scratch `HOME`.
//!
//! ```ignore
//! let mut h = Harness::open(Some(&fixture_repo()));
//! h.wait_for_graph();
//! h.press("cmd-k");
//! h.perform(Action::ToggleWrap);
//! h.wait_until("the diff", |ui| !ui.active().session.document.files.is_empty());
//! h.shot("diff", "wrapped");
//! ```
//!
//! # Time
//!
//! GPUI's test dispatcher runs a simulated clock and has no idea real threads
//! exist: `run_until_parked` returns the moment its own queue is empty, even with
//! the repository actor mid-load on its own thread. So a harness step alternates
//! the two worlds. [`Harness::settle`] pumps the dispatcher, sleeps a real moment
//! so the actor and the tokio runtime can make progress, and repeats until nothing
//! has changed for a while; [`Harness::wait_until`] does the same until a condition
//! on the app holds. Timers built on [`Diffui::after`] run on real time (tokio), so
//! debounces fire without touching the simulated clock.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use gpui_kit::test::{TestEventExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Entity, HeadlessAppContext, Pixels, Point, ScrollDelta,
    Window, point, px, size,
};

use crate::window_state::WindowState;
use crate::{Action, Cli, Cx, Diffui};

/// One window on one repository (or none: the welcome screen).
pub(crate) struct Harness {
    cx: HeadlessAppContext,
    window: AnyWindowHandle,
    app: Entity<Diffui>,
    /// One at a time: they share `HOME` and the fixture.
    _turn: MutexGuard<'static, ()>,
    /// The UI thread runs inside the tokio runtime, as `main` does.
    _tokio: tokio::runtime::EnterGuard<'static>,
}

/// The window size of the iced baselines.
const DEFAULT_SIZE: (f32, f32) = (1280.0, 800.0);
/// How long a settle or a wait may take before the test fails.
const TIMEOUT: Duration = Duration::from_secs(60);
/// A real-time beat between dispatcher pumps.
const BEAT: Duration = Duration::from_millis(10);
/// How long nothing may change for a settle to call it done.
const QUIET: Duration = Duration::from_millis(250);

/// The process-wide scratch `HOME`: window state, recents and config all derive
/// from it, and the owner's real session must survive a test run.
fn scratch_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = std::env::temp_dir().join(format!("diffui-harness-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("create the scratch home");
        // SAFETY: tests that touch the environment go through here first, and the
        // only readers on other threads read it to find config files, for which
        // either value is harmless at this point.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("XDG_STATE_HOME");
            // The fixture and any mutation need an identity; only this file has one.
            let config = home.join("jj-config.toml");
            std::fs::write(
                &config,
                "[user]\nname = \"Harness\"\nemail = \"harness@example.com\"\n",
            )
            .expect("write the jj config");
            std::env::set_var("JJ_CONFIG", config);
        }
        // `Diffui::new` opens the repository it's started in, and these tests start
        // inside a workspace: that must not become the repo under test.
        std::env::set_current_dir(&home).expect("enter the scratch home");
        home
    })
}

/// The shared fixture: the repository `fixtures/make-fixture.sh` builds, made once
/// per test run under the scratch home. `DIFFUI_FIXTURE_REPO` names an existing one
/// instead (it is opened, and never written to by the harness itself).
pub(crate) fn fixture_repo() -> PathBuf {
    if let Some(repo) = std::env::var_os("DIFFUI_FIXTURE_REPO") {
        return PathBuf::from(repo);
    }
    static REPO: OnceLock<PathBuf> = OnceLock::new();
    REPO.get_or_init(|| {
        let home = scratch_home();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/make-fixture.sh");
        let repo = home.join("fixture");
        let output = std::process::Command::new("bash")
            .arg(&script)
            .arg(&repo)
            .env("HOME", home)
            .output()
            .unwrap_or_else(|error| panic!("run {}: {error}", script.display()));
        assert!(
            output.status.success(),
            "{} failed:\n{}",
            script.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        repo
    })
    .clone()
}

fn turn() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Harness {
    /// The app on `repo` at the baselines' size; `None` opens the welcome screen.
    pub(crate) fn open(repo: Option<&Path>) -> Self {
        Self::open_sized(repo, DEFAULT_SIZE)
    }

    pub(crate) fn open_sized(repo: Option<&Path>, (width, height): (f32, f32)) -> Self {
        let turn = turn();
        let tokio = crate::spawn::runtime().enter();
        scratch_home();
        let mut cx = HeadlessAppContext::with_platform(
            crate::measure::platform_text_system(),
            Arc::new(gpui_kit::assets::Assets),
            gpui_kit::platform::current_headless_renderer,
        );
        cx.allow_parking();
        cx.update(crate::root::init);
        let cli = Cli {
            paths: repo.map(Path::to_path_buf).into_iter().collect(),
            repos: Vec::new(),
        };
        let saved = WindowState {
            width: Some(width),
            height: Some(height),
            ..WindowState::default()
        };
        let mut built = None;
        let window = cx
            .open_window(size(px(width), px(height)), |window, cx| {
                let app = crate::root::build_root(cli, saved, window, cx);
                built = Some(app.clone());
                cx.new(|cx| gpui_kit::base::Root::new(app, window, cx))
            })
            .expect("open the headless window");
        let mut harness = Self {
            cx,
            window: window.into(),
            app: built.expect("open_window ran its build closure"),
            _turn: turn,
            _tokio: tokio,
        };
        harness.pump();
        harness
    }

    /// Runs what's queued on the dispatcher, once.
    fn pump(&mut self) {
        self.cx.run_until_parked();
    }

    /// Pumps until the app has stopped changing: nothing queued, and no frame
    /// requested, for a quiet stretch of real time. For after a click or an action
    /// whose results come from another thread.
    pub(crate) fn settle(&mut self) {
        let start = Instant::now();
        let mut quiet_since = Instant::now();
        let mut last = self.fingerprint();
        while quiet_since.elapsed() < QUIET {
            self.pump();
            std::thread::sleep(BEAT);
            let now = self.fingerprint();
            if now != last {
                last = now;
                quiet_since = Instant::now();
            }
            assert!(start.elapsed() < TIMEOUT, "the app never settled");
        }
    }

    /// What changes when work lands: a cheap stand-in for "did anything happen".
    fn fingerprint(&mut self) -> (u64, usize, usize, bool, bool) {
        self.read(|ui| {
            let tab = ui.active();
            (
                ui.document_version,
                tab.session.commits.len(),
                ui.modes.len(),
                tab.session.loading_since.is_some() || tab.session.diff_in_flight(),
                tab.activities.any_running(),
            )
        })
    }

    /// Pumps until `done` holds, failing the test (naming `what`) after a minute.
    pub(crate) fn wait_until(&mut self, what: &str, mut done: impl FnMut(&Diffui) -> bool) {
        let start = Instant::now();
        loop {
            self.pump();
            if self.read(&mut done) {
                return;
            }
            assert!(start.elapsed() < TIMEOUT, "timed out waiting for {what}");
            std::thread::sleep(BEAT);
        }
    }

    /// Waits for the real repository actor to deliver the graph and the first
    /// diff, then for things to go quiet.
    pub(crate) fn wait_for_graph(&mut self) {
        self.wait_until("the revision graph", |ui| {
            let session = &ui.active().session;
            !session.commits.is_empty() && matches!(session.status, crate::LoadStatus::Loaded)
        });
        self.settle();
    }

    /// Reads app state.
    pub(crate) fn read<R>(&mut self, read: impl FnOnce(&Diffui) -> R) -> R {
        let app = self.app.clone();
        self.cx.update(|cx| read(app.read(cx)))
    }

    /// Runs a change inside `act`, as a listener would.
    pub(crate) fn act<R>(&mut self, change: impl FnOnce(&mut Diffui, &mut Cx) -> R) -> R {
        let app = self.app.clone();
        let out = self
            .cx
            .update(|cx| app.update(cx, |app, cx| app.act(cx, change)));
        self.pump();
        out
    }

    /// Performs an action, as a button or a menu would.
    pub(crate) fn perform(&mut self, action: Action) {
        self.act(|app, cx| app.perform(action, cx));
    }

    /// Runs `f` with the window, for what the methods here don't cover.
    pub(crate) fn with_window<R>(&mut self, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        let out = self
            .cx
            .update_window(self.window, |_, window, cx| f(window, cx))
            .expect("the window is open");
        self.pump();
        out
    }

    /// Presses a key through the real dispatch path: `"cmd-k"`, `"escape"`, `"j"`.
    pub(crate) fn press(&mut self, key: &str) {
        self.with_window(|window, cx| window.press(key, cx));
    }

    /// Types text at the current focus.
    pub(crate) fn type_text(&mut self, text: &str) {
        self.with_window(|window, cx| window.input(text, cx));
    }

    /// Clicks an element by id. kit components register themselves; your own
    /// element needs `gpui_kit::base::TestSupportExt::test_support()` before its
    /// listeners, which is free outside tests.
    pub(crate) fn click(&mut self, id: &'static str) {
        self.with_window(|window, cx| window.click(id, cx));
    }

    /// Clicks a window position (logical pixels, top-left origin).
    pub(crate) fn click_point(&mut self, position: Point<Pixels>) {
        self.with_window(|window, cx| {
            window.pointer_down(position, gpui_kit::MouseButton::Left, cx);
            window.pointer_up(position, gpui_kit::MouseButton::Left, cx);
        });
    }

    /// Moves the pointer without a button held.
    pub(crate) fn hover_point(&mut self, position: Point<Pixels>) {
        self.with_window(|window, cx| window.pointer_move(position, None, cx));
    }

    /// Drags with the left button.
    pub(crate) fn drag(&mut self, from: Point<Pixels>, to: Point<Pixels>) {
        self.with_window(|window, cx| window.drag(from, to, cx));
    }

    /// Scrolls the element named `id`.
    pub(crate) fn scroll(&mut self, id: &'static str, by: Point<Pixels>) {
        self.with_window(|window, cx| window.scroll(id, ScrollDelta::Pixels(by), cx));
    }

    /// Renders the window and writes it to `shots/<lane>/<scene>.png` beside the
    /// workspace, returning the path. Settles first.
    pub(crate) fn shot(&mut self, lane: &str, scene: &str) -> PathBuf {
        self.settle();
        let image = self
            .cx
            .capture_screenshot(self.window)
            .expect("capture the window");
        let dir = shots_dir().join(lane);
        std::fs::create_dir_all(&dir).expect("create the shots directory");
        let path = dir.join(format!("{scene}.png"));
        image.save(&path).expect("write the screenshot");
        path
    }
}

/// `DIFFUI_SHOTS_DIR`, else `/Users/wonrax/code/diffui-gpui-ws/shots` beside the
/// workspaces.
fn shots_dir() -> PathBuf {
    std::env::var_os("DIFFUI_SHOTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../shots"))
}

/// A point, for `click_point` and friends.
pub(crate) fn at(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}

#[cfg(test)]
mod scenes {
    use super::*;

    /// With nothing to open the welcome screen owns the window, and its prompt
    /// exists before anything is typed.
    #[test]
    fn welcome_screen() {
        let mut h = Harness::open(None);
        assert!(h.read(|ui| ui.tabs.is_empty()));
        assert!(h.read(|ui| ui.open_repo_dialog().is_some()));
        h.shot("foundation", "welcome");
    }

    /// The real actor loads a real repository through the real event loop, and the
    /// root layout renders with every lane still a stub.
    #[test]
    fn repository_shell() {
        let repo = fixture_repo();
        let mut h = Harness::open(Some(&repo));
        h.wait_for_graph();
        assert!(h.read(|ui| ui.active().session.commits.len() > 1));
        let path = h.shot("foundation", "shell");
        assert!(path.exists());
    }

    /// A key reaches the keymap from the root: ⌘K opens the palette, and Esc closes
    /// it again even with its input focused.
    #[test]
    fn keys_reach_the_keymap() {
        let repo = fixture_repo();
        let mut h = Harness::open(Some(&repo));
        h.wait_for_graph();
        h.press("cmd-k");
        assert!(
            h.read(|ui| ui.palette().is_some()),
            "cmd-k opens the palette"
        );
        h.press("escape");
        assert!(h.read(|ui| ui.palette().is_none()), "esc closes it");
    }
}

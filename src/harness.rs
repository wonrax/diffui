//! See your work without a display: the real root [`Diffui`] in a window rendered
//! through the offscreen renderer, on a scratch repository with a scratch `HOME`.
//!
//! ```ignore
//! let mut h = Harness::open(Some(&fixture_repo())); // a fresh copy of the fixture
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
//! on the app holds. Timers ([`Diffui::after`], [`crate::spawn::Debounce`]) run on
//! real time (tokio), so debounces fire without touching the simulated clock.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use gpui_kit::InputEvent as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Entity, HeadlessAppContext, KeyDownEvent,
    KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, PlatformInput, Point, ScrollDelta, ScrollEvent, Window, point, px, size,
};

use crate::theme::ThemeSpec;
use crate::window_state::WindowState;
use crate::{Action, Cli, Cx, Diffui};

/// A view to show instead of the app: see [`Harness::show`].
pub(crate) type Scene = Rc<dyn Fn(&Diffui, ThemeSpec, &mut Window, &mut Cx) -> AnyElement>;

/// One window on one repository (or none: the welcome screen).
pub(crate) struct Harness {
    cx: HeadlessAppContext,
    window: AnyWindowHandle,
    app: Entity<Diffui>,
    /// One at a time: they share `HOME`.
    _turn: Turn,
    /// The UI thread runs inside the tokio runtime, as `main` does.
    _tokio: tokio::runtime::EnterGuard<'static>,
}

/// A scene's window, unless it asks for another.
const DEFAULT_SIZE: (f32, f32) = (1280.0, 800.0);
/// How long a settle or a wait may take before the test fails.
const TIMEOUT: Duration = Duration::from_secs(60);
/// A real-time beat between dispatcher pumps.
const BEAT: Duration = Duration::from_millis(10);
/// How long nothing may change for a settle to call it done.
const QUIET: Duration = Duration::from_millis(250);

/// The process-wide scratch `HOME`: window state, recents and config all derive
/// from it, and the owner's real session must survive a test run.
pub(crate) fn scratch_home() -> &'static Path {
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
            // The app warns when the jj on PATH doesn't match its jj-lib, with a
            // toast that sits over every shot.
            if let Some(dir) = fixture_jj().parent() {
                let path = std::env::var("PATH").unwrap_or_default();
                std::env::set_var("PATH", format!("{}:{path}", dir.display()));
            }
            // A test never reaches a forge: `gh` and `glab` are fakes that answer
            // only what a test told them to (see [`answer_forge`]).
            let forge_bin = home.join("forge-bin");
            std::fs::create_dir_all(&forge_bin).expect("make the fake forge directory");
            let fake =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("core/tests/fixtures/fake-forge-cli.sh");
            for cli in ["gh", "glab"] {
                std::fs::copy(&fake, forge_bin.join(cli)).expect("install a fake forge CLI");
            }
            std::fs::write(forge_bin.join("rules"), "").expect("write the fake forge rules");
            let path = std::env::var("PATH").unwrap_or_default();
            std::env::set_var("PATH", format!("{}:{path}", forge_bin.display()));
        }
        // `Diffui::new` opens the repository it's started in, and these tests start
        // inside a workspace: that must not become the repo under test.
        std::env::set_current_dir(&home).expect("enter the scratch home");
        home
    })
}

/// What the fake `gh` and `glab` on the harness's PATH answer: `(glob over
/// the arguments, response)` pairs, the first match winning. Anything else
/// fails, as a CLI that can't reach its forge would.
pub(crate) fn answer_forge(rules: &[(&str, &str)]) {
    let dir = scratch_home().join("forge-bin");
    let mut table = String::new();
    for (index, (pattern, response)) in rules.iter().enumerate() {
        let file = format!("response-{index}");
        std::fs::write(dir.join(&file), response).expect("write a fake forge response");
        table.push_str(&format!("{pattern}\t{file}\t0\n"));
    }
    std::fs::write(dir.join("rules"), table).expect("write the fake forge rules");
}

/// The jj the fixture needs: `$JJ`, which the dev shell sets.
pub(crate) fn fixture_jj() -> PathBuf {
    std::env::var_os("JJ")
        .map(PathBuf::from)
        .expect("JJ names a jj matching the app's jj-lib; run the tests in `nix develop`")
}

/// The master copy of what `fixtures/make-fixture.sh` builds, made once per test run
/// under the scratch home, or the existing one `DIFFUI_FIXTURE_REPO` names. Nothing
/// opens it: see [`fixture_repo`].
fn master_fixture() -> &'static Path {
    static REPO: OnceLock<PathBuf> = OnceLock::new();
    REPO.get_or_init(|| {
        if let Some(repo) = std::env::var_os("DIFFUI_FIXTURE_REPO") {
            return PathBuf::from(repo);
        }
        let home = scratch_home();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/make-fixture.sh");
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
}

/// A fresh copy of the fixture, the caller's to do anything to. A key press can apply
/// a rebase, and a shared repository would carry that into whatever opens it next.
pub(crate) fn fixture_repo() -> PathBuf {
    fresh_repo_named("repo")
}

/// [`fixture_repo`] at `<a new directory>/<name>`, for a scene that shows the path.
pub(crate) fn fresh_repo_named(name: &str) -> PathBuf {
    static COPIES: AtomicUsize = AtomicUsize::new(0);
    let master = master_fixture();
    let copy = COPIES.fetch_add(1, Ordering::Relaxed);
    let repo = scratch_home()
        .join("repos")
        .join(copy.to_string())
        .join(name);
    std::fs::create_dir_all(repo.parent().expect("a new directory")).expect("make the copy's home");
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(master)
        .arg(&repo)
        .status()
        .expect("run cp");
    assert!(status.success(), "copy the fixture to {}", repo.display());
    // The copy's remote must not be the master's, or a push would reach every later copy.
    let config = repo.join(".git/config");
    let text = std::fs::read_to_string(&config).expect("the copy's git config");
    std::fs::write(
        &config,
        text.replace(
            master.to_str().expect("a UTF-8 path"),
            repo.to_str().expect("a UTF-8 path"),
        ),
    )
    .expect("point the copy's remote at itself");
    repo
}

/// The right to run a harness. Harnesses on other threads wait for it. One on the
/// thread that already holds it would wait forever, so that fails instead.
struct Turn {
    _guard: MutexGuard<'static, ()>,
}

static TURN_HOLDER: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

fn turn() -> Turn {
    static TURN: Mutex<()> = Mutex::new(());
    let me = std::thread::current().id();
    let holder = |value: Option<std::thread::ThreadId>| {
        let mut slot = TURN_HOLDER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::replace(&mut *slot, value)
    };
    let current = *TURN_HOLDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_ne!(
        current,
        Some(me),
        "Harness::open while this thread's harness is alive: only one runs at a time, so this \
         would wait for itself forever. Drop the first one before opening another."
    );
    let guard = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    holder(Some(me));
    Turn { _guard: guard }
}

impl Drop for Turn {
    fn drop(&mut self) {
        *TURN_HOLDER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

impl Harness {
    /// The app on `repo` at the baselines' size; `None` opens the welcome screen.
    pub(crate) fn open(repo: Option<&Path>) -> Self {
        Self::open_sized(repo, DEFAULT_SIZE)
    }

    pub(crate) fn open_sized(repo: Option<&Path>, size: (f32, f32)) -> Self {
        Self::open_with_settings(repo, size, "")
    }

    /// [`Self::open_sized`] with `settings` as its `settings.toml`. What an
    /// earlier harness's theme or refresh change saved there would otherwise
    /// carry into this one.
    pub(crate) fn open_with_settings(
        repo: Option<&Path>,
        size: (f32, f32),
        settings: &str,
    ) -> Self {
        let turn = turn();
        let dir = scratch_home().join(".config/diffui");
        std::fs::create_dir_all(&dir).expect("make the config directory");
        std::fs::write(dir.join("settings.toml"), settings).expect("write the settings");
        Self::start(turn, repo, size)
    }

    /// [`Self::open_sized`] on a `HOME` with no saved session or recents, and
    /// `config` and `settings` as its `config.toml` and `settings.toml`, as a
    /// first launch would see it.
    pub(crate) fn open_fresh(
        repo: Option<&Path>,
        size: (f32, f32),
        config: &str,
        settings: &str,
    ) -> Self {
        let turn = turn();
        let home = scratch_home();
        for entry in std::fs::read_dir(home).expect("list the scratch home") {
            let path = entry.expect("a scratch home entry").path();
            let keep = ["fixture", "repos", "jj-config.toml", "forge-bin"]
                .iter()
                .any(|name| path.file_name().is_some_and(|file| file == *name));
            if !keep {
                let removed = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                removed.expect("clear the scratch home");
            }
        }
        let dir = home.join(".config/diffui");
        std::fs::create_dir_all(&dir).expect("make the config directory");
        std::fs::write(dir.join("config.toml"), config).expect("write the config");
        std::fs::write(dir.join("settings.toml"), settings).expect("write the settings");
        Self::start(turn, repo, size)
    }

    fn start(turn: Turn, repo: Option<&Path>, (width, height): (f32, f32)) -> Self {
        let tokio = crate::spawn::runtime().enter();
        scratch_home();
        let mut cx = HeadlessAppContext::with_platform(
            crate::measure::platform_text_system(),
            Arc::new(gpui_kit::assets::Assets),
            gpui_kit::platform::current_headless_renderer,
        );
        cx.allow_parking();
        cx.update(crate::root::init);
        // Kit's bars fade on a clock; shown for good, a frame doesn't depend on
        // when it was drawn, and the stale-pane check can compare two.
        cx.update(|cx| {
            gpui_kit::component::Theme::set_scrollbar_mode(
                gpui_kit::component::scroll::ScrollbarMode::Always,
                cx,
            );
            crate::scrollbar::place_kit_bars(cx);
            // Likewise a wheel event lands at once rather than over the next frames.
            diffui_widgets::scroll_resample::turn_off();
        });
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
    pub(crate) fn pump(&mut self) {
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
    fn fingerprint(&mut self) -> (u64, u32, usize, usize, bool, bool) {
        self.read(|ui| {
            let tab = ui.active();
            (
                ui.document_version,
                tab.paint_stamps
                    .iter()
                    .fold(0u32, |sum, stamp| sum.wrapping_add(*stamp)),
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

    /// Delivers the frames something asked for (an animation, a deferred scroll), a
    /// few times over.
    pub(crate) fn frames(&mut self) {
        for _ in 0..3 {
            self.with_window(|window, cx| {
                window.simulate_next_frame(cx);
            });
            self.pump();
        }
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

    /// Fills the window with `view` instead of the app, against the same state. For
    /// driving one view by itself, or what a real scene can't reach.
    pub(crate) fn show(
        &mut self,
        view: impl Fn(&Diffui, ThemeSpec, &mut Window, &mut Cx) -> AnyElement + 'static,
    ) {
        self.act(|app, _| app.background.scene = Some(Rc::new(view)));
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

    /// Times one update of the window, including the draw it flushes on return.
    /// No pumping and no settling: what the user's frame costs.
    pub(crate) fn timed(&mut self, f: impl FnOnce(&mut Window, &mut App)) -> Duration {
        let started = Instant::now();
        self.cx
            .update_window(self.window, |_, window, cx| f(window, cx))
            .expect("the window is open");
        started.elapsed()
    }

    /// Dispatches one platform event in its own update, as the platform would: it
    /// draws what it dirtied and nothing more. kit's own test helpers redraw the whole
    /// window around each event, which would hide a cached pane that went stale.
    fn event(&mut self, input: PlatformInput) {
        self.with_window(|window, cx| {
            window.dispatch_event(input, cx);
        });
    }

    /// [`Self::timed`] around a change to the app.
    pub(crate) fn timed_app(&mut self, f: impl FnOnce(&mut Diffui, &mut Cx)) -> Duration {
        let app = self.app.clone();
        let started = Instant::now();
        self.cx.update(|cx| app.update(cx, f));
        started.elapsed()
    }

    /// Changes app state with no notify at all, as a bug would.
    pub(crate) fn with_app(&mut self, f: impl FnOnce(&mut Diffui)) {
        let app = self.app.clone();
        self.cx.update(|cx| app.update(cx, |app, _| f(app)));
    }

    /// Presses a key through the real dispatch path: `"cmd-k"`, `"escape"`, `"j"`.
    pub(crate) fn press(&mut self, key: &str) {
        let keystroke = self
            .cx
            .update(|cx| Keystroke::parse_with_os(key, cx.platform_info().os))
            .unwrap_or_else(|error| panic!("invalid keystroke {key}: {error}"));
        let keystroke = if matches!(keystroke.key.as_str(), "enter" | "tab") {
            // GPUI's simulated IME would type these as text as well.
            self.event(
                KeyDownEvent {
                    keystroke: keystroke.clone(),
                    is_held: false,
                    prefer_character_input: false,
                }
                .to_platform_input(),
            );
            keystroke
        } else {
            let keystroke = keystroke.with_simulated_ime();
            let sent = keystroke.clone();
            self.with_window(|window, cx| {
                window.dispatch_keystroke(sent, cx);
            });
            keystroke
        };
        self.event(KeyUpEvent { keystroke }.to_platform_input());
    }

    /// Types text at the current focus.
    pub(crate) fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            let text = character.to_string();
            let mut keystroke = self
                .cx
                .update(|cx| Keystroke::parse_with_os(&text, cx.platform_info().os))
                .expect("a character is a valid keystroke");
            keystroke.key_char = Some(text);
            self.with_window(|window, cx| {
                window.dispatch_keystroke(keystroke, cx);
            });
        }
    }

    /// Clicks an element by id. kit components register themselves; your own
    /// element needs `gpui_kit::base::TestSupportExt::test_support()` before its
    /// listeners, which is free outside tests. kit redraws everything around this one.
    pub(crate) fn click(&mut self, id: &'static str) {
        self.with_window(|window, cx| window.click(id, cx));
    }

    /// Clicks a window position (logical pixels, top-left origin).
    pub(crate) fn click_point(&mut self, position: Point<Pixels>) {
        self.hover_point(position);
        self.pointer_down(position, MouseButton::Left);
        self.pointer_up(position, MouseButton::Left);
    }

    /// Moves the pointer without a button held.
    pub(crate) fn hover_point(&mut self, position: Point<Pixels>) {
        self.pointer_move(position, None);
    }

    pub(crate) fn pointer_move(&mut self, position: Point<Pixels>, pressed: Option<MouseButton>) {
        self.event(
            MouseMoveEvent {
                position,
                pressed_button: pressed,
                modifiers: Modifiers::default(),
            }
            .to_platform_input(),
        );
    }

    pub(crate) fn pointer_down(&mut self, position: Point<Pixels>, button: MouseButton) {
        self.event(
            MouseDownEvent {
                button,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            }
            .to_platform_input(),
        );
    }

    pub(crate) fn pointer_up(&mut self, position: Point<Pixels>, button: MouseButton) {
        self.event(
            MouseUpEvent {
                button,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            }
            .to_platform_input(),
        );
    }

    /// Drags with the left button.
    pub(crate) fn drag(&mut self, from: Point<Pixels>, to: Point<Pixels>) {
        self.hover_point(from);
        self.pointer_down(from, MouseButton::Left);
        for step in 1..=8 {
            let fraction = step as f32 / 8.0;
            self.pointer_move(
                point(
                    from.x + (to.x - from.x) * fraction,
                    from.y + (to.y - from.y) * fraction,
                ),
                Some(MouseButton::Left),
            );
        }
        self.pointer_up(to, MouseButton::Left);
    }

    /// Rolls the wheel at a window position.
    pub(crate) fn wheel(&mut self, position: Point<Pixels>, by: f32) {
        self.event(
            ScrollEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.0), px(by))),
                ..Default::default()
            }
            .to_platform_input(),
        );
    }

    /// Renders the window and writes it to `shots/<lane>/<scene>.png` beside the
    /// workspace, returning the path. Settles first.
    pub(crate) fn shot(&mut self, lane: &str, scene: &str) -> PathBuf {
        self.settle();
        self.shot_now(lane, scene)
    }

    /// [`Self::shot`] of the state right now, for a scene that's mid-flight.
    pub(crate) fn shot_now(&mut self, lane: &str, scene: &str) -> PathBuf {
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

impl Harness {
    /// Draws the window with every cache dropped and compares it with what the cached
    /// frames show. `Err` says where they differ: a view reading state that didn't
    /// notify it, which a user would see as a stale pane.
    /// The window's pixels, for a test to compare.
    pub(crate) fn screenshot(&mut self) -> image::RgbaImage {
        self.cx
            .capture_screenshot(self.window)
            .expect("capture the window")
    }

    pub(crate) fn same_after_a_refresh(&mut self) -> Result<(), String> {
        let cached = self
            .cx
            .capture_screenshot(self.window)
            .map_err(|error| error.to_string())?;
        self.with_window(|window, cx| window.render_frame(cx));
        let fresh = self
            .cx
            .capture_screenshot(self.window)
            .map_err(|error| error.to_string())?;
        if cached.dimensions() != fresh.dimensions() {
            return Err("the window changed size".to_owned());
        }
        let mut bounds: Option<(u32, u32, u32, u32)> = None;
        let mut pixels = 0usize;
        for (x, y, pixel) in cached.enumerate_pixels() {
            if pixel != fresh.get_pixel(x, y) {
                pixels += 1;
                let (x0, y0, x1, y1) = bounds.unwrap_or((x, y, x, y));
                bounds = Some((x0.min(x), y0.min(y), x1.max(x), y1.max(y)));
            }
        }
        match bounds {
            None => Ok(()),
            Some(bounds) => Err(format!("{pixels} pixels differ, within {bounds:?}")),
        }
    }
}

/// `DIFFUI_SHOTS_DIR`, else `target/shots`.
fn shots_dir() -> PathBuf {
    std::env::var_os("DIFFUI_SHOTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/shots"))
}

/// A point, for `click_point` and friends.
pub(crate) fn at(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}

#[cfg(test)]
mod scenes {
    use gpui_kit::div;
    use gpui_kit::prelude::*;

    use super::*;
    use crate::prose::Prose;
    use crate::theme::{self, ThemePreference};
    use crate::{chip, ellipsis, empty_states, icons, input, typography, ui};

    /// With nothing to open the welcome screen owns the window, and its prompt
    /// exists before anything is typed.
    #[test]
    fn welcome_screen() {
        let mut h = Harness::open(None);
        assert!(h.read(|ui| ui.tabs.is_empty()));
        assert!(h.read(|ui| ui.open_repo_dialog().is_some()));
        h.shot("foundation", "welcome");
    }

    /// A second harness on one thread fails with a message, rather than waiting for a
    /// turn it holds itself.
    #[test]
    #[should_panic(expected = "Drop the first one")]
    fn a_second_harness_on_a_thread_says_so() {
        let _first = Harness::open(None);
        let _second = Harness::open(None);
    }

    /// Two copies of the fixture share nothing, their remotes included.
    #[test]
    fn each_copy_of_the_fixture_is_its_own() {
        let (one, two) = (fixture_repo(), fixture_repo());
        assert_ne!(one, two);
        std::fs::write(one.join("poison"), "").expect("write to the first copy");
        assert!(!two.join("poison").exists());
        let config = std::fs::read_to_string(two.join(".git/config")).expect("its git config");
        assert!(
            !config.contains(one.to_str().expect("a UTF-8 path")),
            "{config}"
        );
    }

    /// The real actor loads a real repository through the real event loop, and the
    /// root layout renders.
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
    /// it again.
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

    /// A text field works as a controlled component: typing lands in app state, a
    /// plain key stays with the field instead of the keymap, update code can set the
    /// value, and Esc still reaches the app first.
    #[test]
    fn a_field_is_controlled_and_esc_reaches_the_app_first() {
        let repo = fixture_repo();
        let mut h = Harness::open(Some(&repo));
        h.wait_for_graph();
        h.show(|ui, _, window, cx| {
            let revset = ui.active().session.revset.clone();
            let on_input = cx.listener(|this: &mut Diffui, text: &String, _, cx| {
                this.act(cx, |this, _| {
                    this.active_mut().session.revset = text.clone()
                });
            });
            let spec = input::FieldSpec::new("scene-field", &revset)
                .on_input(move |text, window, cx| on_input(&text, window, cx));
            div()
                .w(px(300.0))
                .child(input::single_line(spec, ui.config.mono_font, window, cx))
                .into_any_element()
        });
        h.act(|ui, cx| {
            ui.active_mut().session.revset = String::new();
            input::focus("scene-field", cx);
        });
        h.settle();
        assert!(h.with_window(|window, _| input::input_has_focus(window)));

        let selected = h.read(|ui| ui.active().selected_file);
        h.type_text("jj");
        h.press("j");
        h.settle();
        assert_eq!(h.read(|ui| ui.active().session.revset.clone()), "jjj");
        assert_eq!(h.read(|ui| ui.active().selected_file), selected);

        // Set from outside, as a picked preset does.
        h.act(|ui, _| ui.active_mut().session.revset = "all()".to_owned());
        h.settle();

        h.act(|ui, cx| ui.perform(Action::OpenPalette, cx));
        assert!(h.read(|ui| ui.palette().is_some()));
        h.press("escape");
        assert!(h.read(|ui| ui.palette().is_none()), "esc went to the app");
    }

    /// The shared primitives, drawn: every button variant through kit's button,
    /// chips, an icon, a baseline row, the pattern panel. Also proves a click runs a
    /// listener through `act`.
    #[test]
    fn primitives_and_the_pattern_panel() {
        let repo = fixture_repo();
        let mut h = Harness::open(Some(&repo));
        h.wait_for_graph();
        h.act(|ui, _| {
            let state = ui.active_mut();
            state.session.revset = "nope()".to_owned();
            state.default_revset = Some("all()".to_owned());
        });
        h.perform(Action::SetTheme(ThemePreference::Dark));
        h.show(|ui, theme, window, cx| {
            let font = ui.config.ui_font;
            let mono = ui.config.mono_font;
            let spec = ui::Size::Compact;
            let look = ui.look(theme);
            let chip = chip::Chip {
                label: "main".to_owned(),
                font: mono,
                background: theme::chip_background(theme.lane_base),
                text_color: theme.lane_base,
                border_color: None,
                border_dashed: false,
                icon: Some(icons::GIT_BRANCH),
                icon_color: None,
                suffix: None,
                marks: Vec::new(),
                tip: None,
            };
            let dashed = chip::Chip {
                label: "empty".to_owned(),
                background: theme::TRANSPARENT,
                text_color: theme.subtle_text,
                border_color: Some(theme.subtle_text),
                border_dashed: true,
                icon: None,
                icon_color: None,
                suffix: None,
                marks: Vec::new(),
                tip: None,
                ..chip.clone()
            };
            let prose = Prose::from("Move ")
                .bookmark("main")
                .text(" to ")
                .rev("zqxlrpwv")
                .text(".");
            div()
                .size_full()
                .flex()
                .flex_col()
                .gap(px(16.0))
                .p(px(24.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .items_center()
                        .child(look.button("g", "Ghost").ghost().step(spec))
                        .child(
                            look.button("r", "Raised")
                                .step(spec)
                                .icon(icons::REFRESH, theme.muted_text),
                        )
                        .child(look.button("p", "Primary").primary().step(spec))
                        .child(look.button("o", "Off").primary().step(spec).disabled(true))
                        .child(look.button("d", "Destructive").destructive())
                        .child(
                            look.icon_button("i", icons::SEARCH, theme.muted_text)
                                .step(spec),
                        )
                        .child(look.close_button("c", theme.muted_text)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .items_center()
                        .child(chip::view(&chip))
                        .child(chip::view(&dashed))
                        .child(icons::icon(icons::HISTORY, 16.0, theme.accent)),
                )
                .child(typography::baseline_row(
                    prose.runs(look, theme.text, theme::text_size::BODY),
                    0.0,
                ))
                .child(
                    div().w(px(80.0)).child(
                        ellipsis::Line::new(
                            vec![ellipsis::Span::new("A title that doesn't fit")],
                            font,
                            theme::text_size::BODY,
                            theme.text,
                        )
                        .view(),
                    ),
                )
                .child(
                    div()
                        .w(px(420.0))
                        .h(px(200.0))
                        .children(empty_states::revision_list_placeholder(ui, look, cx)),
                )
                .child(div().w(px(420.0)).child(empty_states::load_error(
                    ui,
                    look,
                    "failed to parse revset:  --> 1:4",
                    cx,
                )))
                .child(input::single_line(
                    input::FieldSpec::new("gallery-field", "a field").placeholder("Type here"),
                    mono,
                    window,
                    cx,
                ))
                .into_any_element()
        });
        h.shot("foundation", "primitives");

        // The reset button is real: clicking it goes through `act` and `perform`.
        h.click("reset-revset");
        h.settle();
        assert_eq!(h.read(|ui| ui.active().session.revset.clone()), "all()");
    }
}

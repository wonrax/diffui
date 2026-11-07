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
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use gpui_kit::test::{TestEventExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Entity, HeadlessAppContext, Pixels, Point,
    ScrollDelta, Window, point, px, size,
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
    /// shooting or driving one view before the views around it exist.
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
            state.default_revset = "all()".to_owned();
        });
        h.perform(Action::SetTheme(ThemePreference::Dark));
        h.show(|ui, theme, window, cx| {
            let font = ui.config.ui_font;
            let mono = ui.config.mono_font;
            let spec = ui::Size::Compact;
            let chip = chip::Chip {
                label: "main".to_owned(),
                font: mono,
                background: theme::chip_background(theme.lane_base),
                text_color: theme.lane_base,
                border_color: None,
                border_dashed: false,
                icon: Some(icons::GIT_BRANCH),
            };
            let dashed = chip::Chip {
                label: "empty".to_owned(),
                background: theme::TRANSPARENT,
                text_color: theme.subtle_text,
                border_color: Some(theme.subtle_text),
                border_dashed: true,
                icon: None,
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
                        .child(ui::text_button(
                            "g",
                            None,
                            "Ghost",
                            ui::Variant::Ghost,
                            spec,
                            font,
                            theme,
                            cx,
                        ))
                        .child(ui::text_button(
                            "r",
                            Some((icons::REFRESH, theme.muted_text)),
                            "Raised",
                            ui::Variant::Raised,
                            spec,
                            font,
                            theme,
                            cx,
                        ))
                        .child(ui::text_button(
                            "p",
                            None,
                            "Primary",
                            ui::Variant::Primary,
                            spec,
                            font,
                            theme,
                            cx,
                        ))
                        .child(ui::primary_button("o", "Off", false, spec, font, theme, cx))
                        .child(ui::text_button(
                            "d",
                            None,
                            "Destructive",
                            ui::Variant::Destructive,
                            ui::Size::Standard,
                            font,
                            theme,
                            cx,
                        ))
                        .child(ui::icon_button(
                            "i",
                            icons::SEARCH,
                            theme.muted_text,
                            spec,
                            theme,
                            cx,
                        ))
                        .child(ui::close_button("c", theme.muted_text, theme, cx)),
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
                    prose.runs(font, mono, theme.text, theme, theme::text_size::BODY),
                    0.0,
                ))
                .child(
                    div().w(px(80.0)).child(
                        ellipsis::Line::new(
                            vec![ellipsis::Span::new("A title that doesn't fit")],
                            font,
                            theme::text_size::UI,
                            theme.text,
                        )
                        .view(),
                    ),
                )
                .child(
                    div()
                        .w(px(420.0))
                        .h(px(200.0))
                        .children(empty_states::revision_list_placeholder(ui, theme, cx)),
                )
                .child(div().w(px(420.0)).child(empty_states::load_error(
                    ui,
                    theme,
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

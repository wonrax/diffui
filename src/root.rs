//! The root view: the window, its layout, and the wiring around them.
//!
//! Three things every lane leans on:
//!
//! * [`Diffui::act`] wraps every state change. It runs the change, then
//!   [`Diffui::reconcile`], then notifies. Listeners go through it
//!   ([`Diffui::listen`], [`Diffui::on_action`]), and so does every async result
//!   ([`Diffui::run_async`]). A path that skips it leaves stale modes and dead
//!   menu items with no error. Only purely visual state (hover) may skip it.
//! * [`Render for Diffui`] builds the shell. A view is a free function taking
//!   `&Diffui`, the theme, `window` and `cx`, returning an element (see
//!   [`crate::empty_states`] for the pattern).
//! * Keys reach the keymap from [`init_keys`], ahead of action dispatch.

use std::collections::HashMap;
use std::time::Duration;

use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Bounds, Context, Div, Entity, Render, StyleRefinement, Subscription, Task,
    WeakEntity, Window, WindowBounds, WindowOptions, div, point, px, relative, size,
};

use crate::keys;
use crate::theme::{self, ThemeSpec};
use crate::ui::Look;
use crate::{
    Action, Cli, Cx, Diffui, MainView, WINDOW_STATE_DEBOUNCE, activity, chrome, diff_panel,
    empty_states, history_panel, icons, measure, menu, palette, resize_handle, sidebar,
    source_panel, tab_bar, toolbar, window_state::WindowState,
};
use diffui_core::OpenSpec;
use diffui_widgets::frame_log;

/// The loops that run in the background. Each lives while its condition holds;
/// [`Diffui::sync_background`] starts and drops them. Dropping the task cancels
/// the loop.
#[derive(Default)]
pub(crate) struct Background {
    /// One event loop per open repository. Dropping it drops the actor's stream,
    /// which shuts the actor (its thread, workspace and watcher) down.
    repos: HashMap<OpenSpec, Task<()>>,
    loading: Option<Task<()>>,
    toasts: Option<Task<()>>,
    geometry: Option<Task<()>>,
    /// What a harness scene shows in place of the app.
    #[cfg(test)]
    pub(crate) scene: Option<crate::harness::Scene>,
    /// The window observers. Non-empty once the view has a window: before that
    /// (and in tests of update code, which have none) nothing runs in the
    /// background.
    window: Vec<Subscription>,
    menu: Option<Task<()>>,
}

impl std::fmt::Debug for Background {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Background")
            .field("repos", &self.repos.len())
            .finish_non_exhaustive()
    }
}

/// App-wide setup, before any window: kit, the icon font, measuring, key
/// interception. Tests and the harness run this too.
pub(crate) fn init(cx: &mut App) {
    gpui_kit::init(cx);
    #[cfg(target_os = "macos")]
    bind_cocoa_text_keys(cx);
    measure::install(cx.text_system().clone());
    icons::register(cx);
    init_keys(cx);
}

/// Every key press reaches the keymap before GPUI dispatches it to anything else.
///
/// This has to be an interceptor rather than a root `on_key_down`. GPUI runs
/// action bindings (a focused kit input's Esc, Enter, arrows) ahead of key
/// listeners, in any phase, and we *must* see Esc even when an input is focused:
/// the input would take it to clear focus, and the user would have to press Esc
/// twice, once to unfocus and once to close. `consumed` says the focused input keeps
/// the key ([`crate::input::field_takes_key`]); that's what keeps the revset box's
/// typing out of the plain-key bindings while its ↑/↓ still walk the list behind it.
/// A key the keymap takes stops here; the rest carry on to the input as usual.
fn init_keys(cx: &mut App) {
    cx.intercept_keystrokes(|event, window, cx| {
        let Some(app) = cx
            .try_global::<RootView>()
            .and_then(|root| root.0.upgrade())
        else {
            return;
        };
        let keystroke = &event.keystroke;
        let key = keys::Key::from_keystroke(keystroke);
        let modifiers = keys::Modifiers::from(keystroke.modifiers);
        let mut consumed = crate::input::field_takes_key(window, &key, modifiers, cx);
        // Esc always lets go of a field. Before the keymap, so a mode that hands
        // focus on (a palette column) wins. That counts as using the key, so in the
        // contexts `needs_unconsumed` holds back it clears nothing else; the overlays
        // aren't among them, and close on the same press.
        if key == keys::Key::Named(keys::Named::Escape) && crate::input::input_has_focus(window) {
            window.blur(cx);
            consumed = true;
        }
        let escape = key == keys::Key::Named(keys::Named::Escape);
        let taken = app.update(cx, |app, cx| {
            app.modifiers = modifiers;
            let taken = app.act_on(cx, PaneSet::NONE, |app, cx| {
                app.on_key_pressed(&key, modifiers, consumed, cx)
            });
            // A key the keymap passes on changed nothing, so it rebuilds
            // nothing; Esc may have dropped a tab drag on the way.
            if taken || escape {
                app.notify_panes(cx, PaneSet::ALL);
            }
            taken
        });
        // A field with a selection of its own copies that; otherwise a live diff
        // selection copies, whichever element holds focus by now.
        let copied = !taken
            && !consumed
            && crate::input::is_copy(&key, modifiers)
            && crate::diff_view::copy_selection(window, cx);
        // Tab means nothing unless the keymap says so: no walking focus through
        // the buttons, which never hold it here.
        if taken || copied || key == keys::Key::Named(keys::Named::Tab) {
            cx.stop_propagation();
        }
    })
    .detach();
}

/// What [`keys::cocoa_text_key`] names, on the actions kit's inputs run. Kit binds
/// only ⌃A and ⌃E, and GPUI drops the selector AppKit answers the rest with.
#[cfg(target_os = "macos")]
fn bind_cocoa_text_keys(cx: &mut App) {
    use gpui_kit::KeyBinding;
    use gpui_kit::base::actions::{SelectDown, SelectLeft, SelectRight, SelectUp};
    use gpui_kit::base::input::{
        Backspace, Delete, DeleteToEndOfLine, MoveDown, MoveLeft, MoveRight, MoveUp,
    };
    const INPUT: Option<&str> = Some("Input");
    cx.bind_keys([
        KeyBinding::new("ctrl-b", MoveLeft, INPUT),
        KeyBinding::new("ctrl-f", MoveRight, INPUT),
        KeyBinding::new("ctrl-n", MoveDown, INPUT),
        KeyBinding::new("ctrl-p", MoveUp, INPUT),
        KeyBinding::new("ctrl-shift-b", SelectLeft, INPUT),
        KeyBinding::new("ctrl-shift-f", SelectRight, INPUT),
        KeyBinding::new("ctrl-shift-n", SelectDown, INPUT),
        KeyBinding::new("ctrl-shift-p", SelectUp, INPUT),
        KeyBinding::new("ctrl-h", Backspace, INPUT),
        KeyBinding::new("ctrl-d", Delete, INPUT),
        KeyBinding::new("ctrl-k", DeleteToEndOfLine, INPUT),
    ]);
}

/// The display a window's position is relative to. On macOS `Window::bounds` is
/// relative to the window's current screen and GPUI reports no screen origins, so
/// the position only means something alongside this.
fn window_display(window: &Window, cx: &App) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let uuid = window.display(cx)?.uuid().ok()?;
    Some(uuid.to_string())
}

/// Where the key interceptor finds the app.
struct RootView(gpui_kit::WeakEntity<Diffui>);

impl gpui_kit::Global for RootView {}

/// Opens the app's window.
pub(crate) fn open_window(cli: Cli, saved: WindowState, cx: &mut App) -> anyhow::Result<()> {
    let window_size = saved
        .size()
        .or_else(|| crate::window_state::default_window_size(crate::screen_visible_size()))
        .unwrap_or((
            crate::DEFAULT_WINDOW_SIZE.width,
            crate::DEFAULT_WINDOW_SIZE.height,
        ));
    let displays = cx.displays();
    let display = saved.display.as_deref().and_then(|saved| {
        displays
            .iter()
            .find(|display| display.uuid().is_ok_and(|uuid| uuid.to_string() == saved))
    });
    // A position on a display that has since been unplugged means nothing on the
    // others: leave the window to open centered.
    let position = saved
        .position()
        .filter(|_| saved.display.is_none() || display.is_some());
    let bounds = match position {
        Some((x, y)) => Bounds::new(
            point(px(x), px(y)),
            size(px(window_size.0), px(window_size.1)),
        ),
        None => Bounds::centered(None, size(px(window_size.0), px(window_size.1)), cx),
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        display_id: display.map(|display| display.id()),
        titlebar: Some(chrome::titlebar_options()),
        // The strip draws the title bar and moves the window itself (see
        // `tab_bar`). Left to AppKit, a press-drag on a tab would move the window
        // before the tab heard it, and every press in the strip would wait to
        // tell a click from a double-click.
        app_owns_titlebar_drag: cfg!(target_os = "macos"),
        app_id: Some("diffui".to_owned()),
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| build_root(cli, saved, window, cx))?;
    Ok(())
}

/// The window's content.
pub(crate) fn build_root(
    cli: Cli,
    saved: WindowState,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Diffui> {
    let app = cx.new(|cx| {
        let mut app = Diffui::new(cli, saved, cx);
        app.attach_window(window, cx);
        app
    });
    let panes = Panes::new(app.clone(), cx);
    app.update(cx, |app, _| app.panes = Some(panes));
    cx.set_global(RootView(app.downgrade()));
    app
}

impl Diffui {
    /// Runs a state change, then [`Diffui::reconcile`], then redraws the
    /// whole window. See the module docs.
    pub(crate) fn act<R>(
        &mut self,
        cx: &mut Cx,
        change: impl FnOnce(&mut Self, &mut Cx) -> R,
    ) -> R {
        self.act_on(cx, PaneSet::ALL, change)
    }

    /// [`Diffui::act`] for a change only `panes` show, such as a file's text
    /// arriving for the diff or a hover in an overlay: the other panes keep
    /// their last frame. Rebuilding all of them costs several milliseconds in
    /// a real window. Anything another pane reads must name that pane too, or
    /// it goes stale.
    pub(crate) fn act_on<R>(
        &mut self,
        cx: &mut Cx,
        panes: PaneSet,
        change: impl FnOnce(&mut Self, &mut Cx) -> R,
    ) -> R {
        frame_log::time(std::any::type_name_of_val(&change), || {
            let out = change(self, cx);
            self.reconcile(cx);
            self.notify_panes(cx, panes);
            out
        })
    }

    /// Redraws `panes`: the whole window for [`PaneSet::ALL`], or before the
    /// panes exist.
    pub(crate) fn notify_panes(&self, cx: &mut Cx, set: PaneSet) {
        match &self.panes {
            Some(panes) if set != PaneSet::ALL => {
                for pane in panes.select(set) {
                    pane.update(cx, |_, cx| cx.notify());
                }
            }
            _ if set.is_empty() => {}
            _ => cx.notify(),
        }
    }

    /// A listener that runs `change` inside [`Diffui::act`].
    pub(crate) fn listen<E: 'static>(
        cx: &mut Cx,
        change: impl Fn(&mut Diffui, &E, &mut Window, &mut Cx) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        Self::listen_on(cx, PaneSet::ALL, change)
    }

    /// A listener that runs `change` inside [`Diffui::act_on`] `panes`.
    pub(crate) fn listen_on<E: 'static>(
        cx: &mut Cx,
        panes: PaneSet,
        change: impl Fn(&mut Diffui, &E, &mut Window, &mut Cx) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        cx.listener(move |this: &mut Diffui, event: &E, window, cx| {
            this.act_on(cx, panes, |this, cx| change(this, event, window, cx));
        })
    }

    /// A listener that performs `action`: what a button's `on_click` usually is.
    pub(crate) fn on_action<E: 'static>(
        action: Action,
        cx: &mut Cx,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        Self::listen(cx, move |this, _, _, cx| this.perform(action.clone(), cx))
    }

    /// What must hold after any state change. It runs at the end of every
    /// [`Diffui::act`]; forgetting it produces stale modes and dead menu items
    /// with no error.
    pub(crate) fn reconcile(&mut self, cx: &mut Cx) {
        // A mode that isn't on screen must not keep the keyboard.
        self.prune_stale_modes();
        self.prune_panel_peek();
        crate::app_menu::refresh_availability(self);
        self.keep_graph_anchor();
        self.cancel_unwanted_preview();
        self.ensure_welcome_prompt(cx);
        self.sync_conflicts();
        self.sync_history(cx);
        self.sync_media(cx);
        self.sync_forge(cx);
        self.sync_plugins(cx);
        // Chokepoint: if the active tab coalesced a refresh while busy and it's
        // now idle, run it. A no-op when nothing's pending (the common case).
        if let Some(tab) = self.active_tab_id() {
            let effects = match self.tab_mut(tab) {
                Some(target) => target.session.take_pending_refresh(),
                None => Vec::new(),
            };
            self.run_effects(tab, effects);
        }
        self.sync_background(cx);
    }

    /// Starts the loops whose condition now holds and drops the rest.
    fn sync_background(&mut self, cx: &mut Cx) {
        if self.background.window.is_empty() {
            return;
        }
        // One actor per open repository. Keyed by what it opened, so a tab keeps
        // its actor across reorders and two tabs on one repository share it.
        let specs: Vec<OpenSpec> = self.tabs.iter().map(|tab| tab.open_spec()).collect();
        self.background.repos.retain(|spec, _| specs.contains(spec));
        for spec in specs {
            self.background
                .repos
                .entry(spec.clone())
                .or_insert_with(|| repository_loop(spec, cx));
        }

        // While anything is in flight (a load, a diff switch, or any running
        // activity) tick so the toolbar progress line and spinner animate and
        // reflect live progress.
        let active = self.active();
        let work_in_flight = active.session.loading_since.is_some()
            || active.session.diff_in_flight()
            || active.activities.any_running();
        // Only the toolbar draws progress, and the activity log floats over it.
        sync_ticker(
            &mut self.background.loading,
            work_in_flight,
            Duration::from_millis(120),
            cx,
            |this, cx| this.notify_panes(cx, PaneSet::TOOLBAR | PaneSet::FLOATS),
        );

        // The overlay menu's fixed clock: the trajectory apex and the sweep stall.
        let menu_open = self.menu().is_some();
        sync_ticker(
            &mut self.background.menu,
            menu_open,
            Duration::from_millis(16),
            cx,
            |this, cx| this.act(cx, |this, _| this.menu_on_tick()),
        );

        // Prunes expired error toasts; only while one that expires is up.
        sync_ticker(
            &mut self.background.toasts,
            self.toasts.iter().any(|toast| !toast.sticky),
            Duration::from_millis(500),
            cx,
            |this, cx| this.act_on(cx, PaneSet::FLOATS, |this, _| this.on_toast_tick()),
        );

        // Drives the debounced geometry save. Active only while a change is
        // pending; the handler writes once the changes settle, then clears the
        // dirty flag, which drops this loop.
        sync_ticker(
            &mut self.background.geometry,
            self.geometry_dirty_since.is_some(),
            WINDOW_STATE_DEBOUNCE,
            cx,
            |this, cx| this.act(cx, |this, _| this.on_window_persist_state()),
        );
    }

    /// Hooks the view up to its window: geometry, focus and appearance
    /// observers. These only ever record state; the handlers run inside `act`.
    pub(crate) fn attach_window(&mut self, window: &mut Window, cx: &mut Cx) {
        self.window = Some(window.window_handle());
        let bounds = window.bounds();
        self.window_display = window_display(window, cx);
        self.on_window_opened(Some(bounds.origin.into()), bounds.size.into());
        self.apply_theme(cx);

        let subscriptions = vec![
            cx.observe_window_bounds(window, |this, window, cx| {
                let bounds = window.bounds();
                let display = window_display(window, cx);
                this.act(cx, |this, _| {
                    this.on_window_resized(bounds.size.into());
                    this.on_window_moved(bounds.origin.into(), display);
                });
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                let focused = window.is_window_active();
                this.act(cx, |this, _| this.on_window_focus_changed(focused));
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.act(cx, |this, cx| {
                    this.system_theme = window.appearance().into();
                    this.apply_theme(cx);
                });
            }),
        ];
        self.background.window = subscriptions;

        // The close is ours to perform: the pending window state has to be
        // flushed first, and ⌘Q and the close button raise no focus loss.
        let this = cx.weak_entity();
        cx.on_app_quit(|this: &mut Diffui, _| {
            this.on_window_close_requested();
            async {}
        })
        .detach();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = this.update(cx, |this, _| this.on_window_close_requested());
            cx.quit();
            false
        });

        crate::app_menu::start(cx);
        crate::credentials::start(cx);
        crate::ctl::start(cx);
        crate::plugins::start(self, cx);
        self.sync_background(cx);
    }

    /// Re-derives kit's theme from ours. Call when the preference or the OS
    /// appearance changes.
    pub(crate) fn apply_theme(&self, cx: &mut Cx) {
        // Nothing to dress before there's a window; tests of update code have none.
        if self.window.is_none() {
            return;
        }
        theme::apply_to_kit(self.resolved_theme(), cx);
    }
}

/// Keeps `slot` running a loop that calls `tick` every `every` while `wanted`,
/// and drops the loop otherwise.
fn sync_ticker(
    slot: &mut Option<Task<()>>,
    wanted: bool,
    every: Duration,
    cx: &mut Cx,
    tick: impl Fn(&mut Diffui, &mut Cx) + 'static,
) {
    if !wanted {
        *slot = None;
        return;
    }
    if slot.is_some() {
        return;
    }
    *slot = Some(cx.spawn(async move |this, cx| {
        loop {
            let _ = crate::spawn::runtime()
                .spawn(tokio::time::sleep(every))
                .await;
            if this.update(cx, |this, cx| tick(this, cx)).is_err() {
                return;
            }
        }
    }));
}

/// One repository's actor and its event loop. The actor runs on its own thread
/// and reports through a channel; this folds what it says into the app. Events
/// that arrived together fold in one update, so a burst costs one reconcile.
fn repository_loop(spec: OpenSpec, cx: &mut Cx) -> Task<()> {
    use futures::{FutureExt as _, StreamExt as _};
    let mut events = diffui_core::repo::open(spec);
    cx.spawn(async move |this, cx| {
        while let Some(first) = events.next().await {
            let mut batch = vec![first];
            while let Some(Some(next)) = events.next().now_or_never() {
                batch.push(next);
            }
            // A file's text read for highlighting changes nothing the other panes
            // show, and these arrive one per file as the diff scrolls.
            let main_only = batch.iter().all(|event| {
                matches!(
                    event.payload,
                    diffui_core::repo::Payload::FilePairRead { .. }
                )
            });
            let panes = if main_only {
                PaneSet::MAIN
            } else {
                PaneSet::ALL
            };
            let updated = this.update(cx, |this, cx| {
                this.act_on(cx, panes, |this, cx| {
                    for event in batch {
                        this.on_repo_event(event, cx);
                    }
                });
            });
            if updated.is_err() {
                return;
            }
        }
    })
}

impl Diffui {
    /// Notes what has the keyboard as the first window-level mode (a dialog,
    /// the palette, a popover) opens, before its field takes focus on its
    /// first draw, and gives it back once the last mode has closed.
    fn restore_focus_after_modes(&mut self, window: &mut Window, cx: &mut App) {
        match (self.modes.is_empty(), self.focus_before_modes.is_some()) {
            (false, false) => self.focus_before_modes = Some(window.focused(cx)),
            (true, true) => {
                if let Some(Some(handle)) = self.focus_before_modes.take() {
                    window.focus(&handle, cx);
                }
            }
            _ => {}
        }
    }
}

impl Render for Diffui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        frame_log::begin();
        self.media.release(window);
        self.restore_focus_after_modes(window, cx);
        let theme = self.resolved_theme().spec();
        let look = self.look(theme);

        #[cfg(test)]
        if let Some(scene) = self.background.scene.clone() {
            request_overlay_frames(self, window);
            return self
                .frame(theme, cx)
                .child(scene(self, theme, window, cx))
                .into_any_element();
        }
        let body = if self.tabs.is_empty() {
            // No repositories open: the welcome screen owns the whole window, with
            // the open prompt inline rather than as a modal over it.
            div()
                .relative()
                .size_full()
                .child(empty_states::welcome(self, look, window, cx))
                .child(crate::shortcuts::build_overlay(self, look, window, cx))
                .child(crate::settings::build_overlay(self, look, window, cx))
        } else {
            let panes = self.panes.clone().expect("a drawn app has its panes");
            self.shell(&panes, theme, window, cx)
        };
        self.frame(theme, cx)
            .child(body)
            .when(frame_log::enabled(), |root| {
                // Last in paint order, so it marks the end of the frame.
                root.child(
                    gpui_kit::canvas(|_, _, _| {}, |_, _, _, _| frame_log::end())
                        .absolute()
                        .size_0(),
                )
            })
            .into_any_element()
    }
}

impl Diffui {
    /// The root element: the window's ground, and the type everything inherits.
    fn frame(&self, theme: ThemeSpec, cx: &mut Cx) -> gpui_kit::Stateful<Div> {
        div()
            .id("diffui-root")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(theme.background)
            .text_color(theme.text)
            .font(self.config.ui_font.gpui())
            .line_height(relative(crate::typography::DEFAULT_LINE_HEIGHT))
            // In the capture phase, so a surface that stops the press still counts,
            // and a field pressed on next can take focus after this lets go.
            .child(
                gpui_kit::canvas(
                    |_, _, _| {},
                    |_, _, window, _| {
                        window.on_mouse_event(
                            |event: &gpui_kit::MouseDownEvent, phase, window, cx| {
                                if phase == gpui_kit::DispatchPhase::Capture
                                    && event.button == gpui_kit::MouseButton::Left
                                {
                                    crate::input::blur_on_press_outside(event.position, window, cx);
                                }
                            },
                        );
                    },
                )
                .absolute()
                .size_0(),
            )
            // Live modifier state, for pointer gestures to read at decision time
            // (⌥-drop = rebase with descendants). Nothing on screen depends on it,
            // so this skips `act`.
            .on_modifiers_changed(cx.listener(
                |this: &mut Diffui, event: &gpui_kit::ModifiersChangedEvent, _, _| {
                    this.modifiers = event.modifiers.into();
                },
            ))
    }
}

/// A [`Pane`] in the shell. It fills what it's given, since a cached view isn't
/// measured from its contents.
fn embed(pane: &Entity<Pane>) -> AnyElement {
    // What the frame benchmark compares against.
    #[cfg(test)]
    if std::env::var_os("DIFFUI_UNCACHED_PANES").is_some() {
        return pane.clone().into_any_element();
    }
    pane.clone()
        .cached(StyleRefinement::default().size_full())
        .into_any_element()
}

/// Per-frame animations: the palette's push/pop, a menu's glow and the submenu
/// trajectory apex. Called from the view showing them, which is what the request
/// redraws.
fn request_overlay_frames(ui: &Diffui, window: &mut Window) {
    let palette_animating = ui
        .palette()
        .is_some_and(|palette| palette.is_animating(std::time::Instant::now()));
    let menu_ticking = ui
        .menu()
        .is_some_and(|menu| menu.glow.is_some() || !menu.open_path.is_empty());
    if palette_animating || menu_ticking {
        window.request_animation_frame();
    }
}

/// One region of the window as a view of its own, reused from frame to frame.
///
/// GPUI marks a notified view's ancestors dirty too, so [`Diffui::render`] runs
/// whenever anything in the window notifies: a scroll tick in the diff, a hover in
/// the list. Without panes that rebuilt every other region for each such frame. A
/// pane is an entity under the root, drawn with [`Entity::cached`]: its output is
/// replayed until it's notified itself. That leaves the skeleton and the one pane
/// holding the notifying view to run.
///
/// A cached view is only as fresh as its notifications, and an ancestor re-rendering
/// doesn't refresh it. A pane reads [`Diffui`], so it observes [`Diffui`]: every
/// `cx.notify()` there reaches all of them, and state a view reads has to change
/// behind one (`act`, `listen` and `on_action` do this). A focus change refreshes the
/// whole window on its own. Anything else a view reads has to arrange its own
/// notification.
pub(crate) struct Pane {
    app: WeakEntity<Diffui>,
    /// The span this pane's render is logged as (`diffui_widgets::frame_log`).
    name: &'static str,
    build: PaneBuild,
    #[cfg(test)]
    pub(crate) renders: usize,
    _app_changed: Subscription,
}

type PaneBuild = fn(&Diffui, Look, &mut Window, &mut Cx) -> AnyElement;

impl Pane {
    fn new(
        app: Entity<Diffui>,
        name: &'static str,
        build: PaneBuild,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| Self {
            app: app.downgrade(),
            name,
            build,
            #[cfg(test)]
            renders: 0,
            _app_changed: cx.observe(&app, |_, _, cx| cx.notify()),
        })
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let build = self.build;
        frame_log::time(self.name, || match self.app.upgrade() {
            Some(app) => app.update(cx, |ui, cx| {
                let look = ui.look(ui.resolved_theme().spec());
                build(ui, look, window, cx)
            }),
            None => div().into_any_element(),
        })
    }
}

/// A set of [`Panes`]: the ones a change redraws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneSet(u8);

impl PaneSet {
    pub(crate) const NONE: Self = Self(0);
    pub(crate) const TAB_BAR: Self = Self(1);
    pub(crate) const TOOLBAR: Self = Self(1 << 1);
    pub(crate) const LEFT: Self = Self(1 << 2);
    pub(crate) const FILES: Self = Self(1 << 3);
    pub(crate) const MAIN: Self = Self(1 << 4);
    pub(crate) const STATUS: Self = Self(1 << 5);
    pub(crate) const FLOATS: Self = Self(1 << 6);
    pub(crate) const ALL: Self = Self((1 << 7) - 1);

    fn is_empty(self) -> bool {
        self == Self::NONE
    }

    fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for PaneSet {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The window's regions.
#[derive(Clone)]
pub(crate) struct Panes {
    pub(crate) tab_bar: Entity<Pane>,
    pub(crate) toolbar: Entity<Pane>,
    pub(crate) left: Entity<Pane>,
    pub(crate) files: Entity<Pane>,
    pub(crate) main: Entity<Pane>,
    pub(crate) status: Entity<Pane>,
    pub(crate) floats: Entity<Pane>,
}

impl std::fmt::Debug for Panes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Panes").finish_non_exhaustive()
    }
}

impl Panes {
    fn select(&self, set: PaneSet) -> impl Iterator<Item = &Entity<Pane>> {
        [
            (PaneSet::TAB_BAR, &self.tab_bar),
            (PaneSet::TOOLBAR, &self.toolbar),
            (PaneSet::LEFT, &self.left),
            (PaneSet::FILES, &self.files),
            (PaneSet::MAIN, &self.main),
            (PaneSet::STATUS, &self.status),
            (PaneSet::FLOATS, &self.floats),
        ]
        .into_iter()
        .filter(move |(member, _)| set.contains(*member))
        .map(|(_, pane)| pane)
    }

    fn new(app: Entity<Diffui>, cx: &mut App) -> Self {
        Self {
            tab_bar: Pane::new(app.clone(), "tab_bar", tab_bar::build_tab_bar, cx),
            toolbar: Pane::new(app.clone(), "toolbar", toolbar::build_toolbar, cx),
            left: Pane::new(app.clone(), "left", left_pane, cx),
            files: Pane::new(app.clone(), "files", files_pane, cx),
            main: Pane::new(app.clone(), "main", main_pane, cx),
            status: Pane::new(
                app.clone(),
                "status",
                crate::status_bar::build_status_bar,
                cx,
            ),
            floats: Pane::new(app, "floats", floats_pane, cx),
        }
    }
}

/// The revision list, or what the view shows in its place.
fn left_pane(ui: &Diffui, look: Look, window: &mut Window, cx: &mut Cx) -> AnyElement {
    match ui.active().main_view {
        MainView::Diff => sidebar::build_sidebar(ui, look, window, cx),
        MainView::Source => source_panel::build_source_sidebar(ui, look, window, cx),
        MainView::History => history_panel::build_history_sidebar(ui, look, window, cx),
    }
}

/// The changed files, in the Diff view: the panel, or the narrower tree that
/// floats over the diff while the panel is hidden.
fn files_pane(ui: &Diffui, look: Look, window: &mut Window, cx: &mut Cx) -> AnyElement {
    if ui.active().files_panel_collapsed {
        diff_panel::build_file_navigator_peek(ui, look, window, cx)
    } else {
        diff_panel::build_file_navigator(ui, look, window, cx)
    }
}

/// What the view is about: the diff, a source file, the history.
fn main_pane(ui: &Diffui, look: Look, window: &mut Window, cx: &mut Cx) -> AnyElement {
    match ui.active().main_view {
        MainView::Diff => diff_panel::build_diff_panel(ui, look, window, cx),
        MainView::Source => source_panel::build_source_panel(ui, look, window, cx),
        MainView::History => history_panel::build_history_panel(ui, look, window, cx),
    }
}

/// Everything that floats above the shell. Each is empty when inactive, so they can
/// always be stacked.
fn floats_pane(ui: &Diffui, look: Look, window: &mut Window, cx: &mut Cx) -> AnyElement {
    request_overlay_frames(ui, window);
    div()
        .relative()
        .size_full()
        // Under every overlay: the picks' bar floats over the diff, not over them.
        .child(crate::changes::action_bar(ui, look, window, cx))
        .child(panel_peek(ui, look, cx))
        .child(palette::build_overlay(ui, look, window, cx))
        .child(activity::activity_popover(ui, look, window, cx))
        // Under the menu, which its font buttons open over it.
        .child(crate::settings::build_overlay(ui, look, window, cx))
        .child(menu::build_overlay(ui, look, window, cx))
        .child(tab_bar::build_open_repo_dialog(ui, look, window, cx))
        .child(crate::bookmarks::build_name_prompt(ui, look, window, cx))
        .child(crate::forge_dialog::build_dialog(ui, look, window, cx))
        .child(crate::changes::split_modal(ui, look, window, cx))
        .child(tab_bar::build_confirm_dialog(ui, look, window, cx))
        .child(crate::plugins::build_permission_dialog(
            ui, look, window, cx,
        ))
        .child(crate::cli_output::build_overlay(ui, look, window, cx))
        .child(crate::credentials::build_overlay(ui, look, window, cx))
        .child(crate::shortcuts::build_overlay(ui, look, window, cx))
        .child(activity::toast_layer(ui, look, window, cx))
        .into_any_element()
}

/// A panel the toolbar can hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Panel {
    /// The revision list beside the diff.
    Revisions,
    /// The changed files.
    Files,
}

/// A hidden panel shown over the diff for a moment, opened from the diff's
/// header bar. It closes on Esc, on a press anywhere else, and once something
/// in it is picked; it's the panel's own pane, not a copy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PanelPeek {
    pub(crate) panel: Panel,
    /// The control that opened it, in window coordinates: the file tree hangs
    /// from it, and a press on it toggles rather than closing and reopening.
    pub(crate) anchor: crate::geometry::Rectangle,
}

/// The file tree's peek: this wide, at most this tall.
const FILES_PEEK_WIDTH: f32 = diff_panel::FILE_NAV_PEEK_WIDTH;
const FILES_PEEK_MAX_HEIGHT: f32 = 560.0;
/// The file tree's peek: how far it sits under its trigger, how far its left
/// edge sits left of the trigger's, and the least room it keeps to the window.
const FILES_PEEK_DROP: f32 = 6.0;
const FILES_PEEK_LEAD: f32 = 10.0;
const FILES_PEEK_MARGIN: f32 = 8.0;

impl Diffui {
    /// Opens `panel` over the diff, or closes it if it's the one open. Only a
    /// hidden panel opens.
    pub(crate) fn toggle_panel_peek(&mut self, panel: Panel, anchor: crate::geometry::Rectangle) {
        if self.panel_peek.is_some_and(|peek| peek.panel == panel) {
            self.panel_peek = None;
        } else if self.panel_hidden(panel) {
            self.panel_peek = Some(PanelPeek { panel, anchor });
        }
    }

    pub(crate) fn close_panel_peek(&mut self) {
        self.panel_peek = None;
    }

    /// Whether `panel` is hidden in the Diff view, where only it can be.
    fn panel_hidden(&self, panel: Panel) -> bool {
        let tab = self.active();
        tab.main_view == MainView::Diff
            && match panel {
                Panel::Revisions => tab.history_panel_collapsed,
                Panel::Files => tab.files_panel_collapsed && !tab.session.document.files.is_empty(),
            }
    }

    /// A peek outlives nothing it stands in for: the panel coming back, a view
    /// or tab switch, the files going away.
    fn prune_panel_peek(&mut self) {
        if let Some(peek) = self.panel_peek
            && !self.panel_hidden(peek.panel)
        {
            self.panel_peek = None;
        }
    }
}

/// The peeked panel, over the panes: the revisions down the window's left edge
/// at the sidebar's width, the file tree under the control that opened it.
/// How far a peek's pane sits in from its rounded edges. GPUI clips to the
/// surface's rectangle, not its rounded corners, so a pane flush to the edge
/// would square them off: this keeps its corners inside the curve.
const PEEK_CORNER_INSET: f32 = theme::radius::LG * (1.0 - std::f32::consts::FRAC_1_SQRT_2) + 0.5;

fn panel_peek(ui: &Diffui, look: Look, cx: &mut Cx) -> AnyElement {
    let Look { theme, .. } = look;
    let (Some(peek), Some(panes)) = (ui.panel_peek, ui.panes.as_ref()) else {
        return div().into_any_element();
    };
    let top = toolbar::chrome_bottom();
    let bottom = crate::status_bar::TOTAL_HEIGHT;
    let shadow = theme::Elevation::Popover.shadow(theme);
    // The floating files peek is a squircle; the revisions peek, flush with the
    // window's left edge and edged on its right only, keeps its plain corners.
    let (surface, pane, squircle) = match peek.panel {
        Panel::Revisions => (
            div()
                .absolute()
                .left_0()
                .top(px(top))
                .bottom(px(bottom))
                .w(px(ui.sidebar_width))
                .rounded_r(px(theme::radius::LG))
                .border_r_1()
                .py(px(PEEK_CORNER_INSET))
                .pr(px(PEEK_CORNER_INSET)),
            &panes.left,
            false,
        ),
        Panel::Files => {
            let window = ui.window_size;
            let left = (peek.anchor.x - FILES_PEEK_LEAD)
                .min(window.width - FILES_PEEK_WIDTH - FILES_PEEK_MARGIN)
                .max(FILES_PEEK_MARGIN);
            let top = peek.anchor.y + peek.anchor.height + FILES_PEEK_DROP;
            let height = (window.height - bottom - FILES_PEEK_MARGIN - top)
                .clamp(0.0, FILES_PEEK_MAX_HEIGHT);
            (
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(FILES_PEEK_WIDTH))
                    .h(px(height))
                    .rounded(px(theme::radius::LG))
                    .border_1()
                    .p(px(PEEK_CORNER_INSET)),
                &panes.files,
                true,
            )
        }
    };
    // A press anywhere but the peek and its trigger closes it, and still does
    // what it would have; one on the trigger leaves the toggle to the trigger.
    let bounds = std::rc::Rc::new(std::cell::Cell::new(crate::geometry::Rectangle::default()));
    let recorded = bounds.clone();
    let close = Diffui::listen(cx, |this, _: &gpui_kit::MouseDownEvent, _, _| {
        this.close_panel_peek();
    });
    let anchor = peek.anchor;
    let watcher = gpui_kit::canvas(
        move |rect, _, _| recorded.set(rect.into()),
        move |_, _, window, _| {
            window.on_mouse_event(move |event: &gpui_kit::MouseDownEvent, phase, window, cx| {
                let at = crate::geometry::Point::from(event.position);
                if phase == gpui_kit::DispatchPhase::Capture
                    && !bounds.get().contains(at)
                    && !anchor.contains(at)
                {
                    close(event, window, cx);
                }
            });
        },
    )
    .absolute()
    .size_full();
    surface
        .id("panel-peek")
        .overflow_hidden()
        .map(|surface| {
            if squircle {
                surface
                    .border_color(theme::TRANSPARENT)
                    .child(crate::squircle::background(
                        crate::squircle::Fill::new(crate::squircle::Corners::all(
                            theme::radius::LG,
                        ))
                        .background(theme.panel_background)
                        .border(1.0, theme.border),
                        1.0,
                    ))
            } else {
                surface
                    .bg(theme.panel_background)
                    .border_color(theme.border)
            }
        })
        .shadow(vec![shadow])
        .occlude()
        .child(watcher)
        // The pane's own clip, a rectangle wholly inside the rounded edge: the
        // surface's clip is its square box, which would let a pane drawing
        // past its inset show at the corners.
        .child(div().size_full().overflow_hidden().child(embed(pane)))
        .into_any_element()
}

/// A 1px divider between panes.
pub(crate) fn vertical_divider(theme: ThemeSpec) -> Div {
    div().w(px(1.0)).h_full().flex_none().bg(theme.border)
}

/// A 1px rule between strips.
pub(crate) fn horizontal_divider(theme: ThemeSpec) -> Div {
    div().w_full().h(px(1.0)).flex_none().bg(theme.border)
}

impl Diffui {
    /// The whole window with a tab open: the skeleton, cheap enough to run on every
    /// frame, with the regions in it as cached [`Pane`]s.
    fn shell(&self, panes: &Panes, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> Div {
        // Every window-level mode floats above the shell.
        let modal_open = !self.modes.is_empty() || crate::changes::split_shown(self);

        let mut shell = div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .h(px(tab_bar::height()))
                    .child(embed(&panes.tab_bar)),
            )
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .h(px(toolbar::HEIGHT + activity::PROGRESS_LINE_HEIGHT))
                    .child(embed(&panes.toolbar)),
            )
            .child(horizontal_divider(theme))
            .child(self.panes(panes, theme, window, cx))
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .h(px(crate::status_bar::TOTAL_HEIGHT))
                    .child(embed(&panes.status)),
            );
        // While something floats above, nothing below it hovers or takes presses.
        if modal_open {
            shell = shell.child(div().absolute().inset_0().occlude());
        }

        // Overlays float above the whole shell.
        div()
            .relative()
            .size_full()
            .child(shell)
            .child(div().absolute().inset_0().child(embed(&panes.floats)))
    }

    /// The panes under the toolbar: the history pane, the changed files and the
    /// diff; or the source browser's or the history view's pair. All loading
    /// feedback lives in the toolbar; there's no full-window cold-load takeover and
    /// no diff-pane spinner. On a cold load the sidebar simply grows from empty as
    /// batches arrive, and a revision switch keeps the prior diff until the new one
    /// replaces it.
    /// Collapsing hides the Diff view's sidebar entirely. Source and History have
    /// no toggle of their own, so they keep their width whatever the flag says.
    pub(crate) fn left_pane_width(&self) -> f32 {
        if self.active().main_view == MainView::Diff && self.active().history_panel_collapsed {
            0.0
        } else {
            self.sidebar_width
        }
    }

    /// The changed files' width: none outside the Diff view, with no files, or
    /// collapsed.
    pub(crate) fn files_pane_width(&self) -> f32 {
        let tab = self.active();
        if tab.main_view != MainView::Diff
            || tab.files_panel_collapsed
            || tab.session.document.files.is_empty()
        {
            0.0
        } else {
            self.file_nav_width
        }
    }

    /// What the side panes take from the window, their dividers included: a
    /// hidden pane takes nothing, not even its divider.
    pub(crate) fn side_panes_width(&self) -> f32 {
        [self.left_pane_width(), self.files_pane_width()]
            .into_iter()
            .filter(|width| *width > 0.0)
            .map(|width| width + theme::HAIRLINE)
            .sum()
    }

    fn panes(&self, panes: &Panes, theme: ThemeSpec, _window: &mut Window, cx: &mut Cx) -> Div {
        let history_width = self.left_pane_width();
        let files_width = self.files_pane_width();

        let fixed = |width: f32, pane: &Entity<Pane>| {
            div()
                .h_full()
                .w(px(width))
                .flex_none()
                .overflow_hidden()
                .child(embed(pane))
        };
        let divider = || vertical_divider(theme);

        // A hidden pane isn't drawn here at all: its entity may be on screen as a
        // peek instead (see `floats_pane`).
        let mut row = div().flex().flex_row().size_full();
        if history_width > 0.0 {
            row = row
                .child(fixed(history_width, &panes.left))
                .child(divider());
        }
        if files_width > 0.0 {
            row = row.child(fixed(files_width, &panes.files)).child(divider());
        }
        row = row.child(
            div()
                .h_full()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(embed(&panes.main)),
        );

        let mut body = div().relative().flex_1().min_h_0().w_full().child(row);

        // The history pane's seam. Collapsed, it still hears the drag that
        // collapsed it, so the same drag can bring the pane back.
        {
            let reserve = if files_width > 0.0 {
                files_width + theme::HAIRLINE
            } else {
                0.0
            } + theme::HAIRLINE
                + resize_handle::MIN_DIFF_PANE_WIDTH;
            let on_resize = cx.listener(|this: &mut Diffui, width: &f32, _, cx| {
                this.act(cx, |this, _| this.on_sidebar_width_changed(*width));
            });
            body = body.child(
                resize_handle::ResizeHandle::new(
                    "history-seam",
                    history_width,
                    self.sidebar_min_width,
                    sidebar::RESIZE_HIT_PADDING,
                    move |width, window, cx| on_resize(&width, window, cx),
                )
                .right_reserve(reserve)
                .grabbable(history_width > 0.0)
                .view(),
            );
        }
        // The changed files' seam, likewise.
        if self.active().main_view == crate::MainView::Diff {
            let lead = if history_width > 0.0 {
                history_width + theme::HAIRLINE
            } else {
                0.0
            };
            let on_resize = cx.listener(|this: &mut Diffui, position: &f32, _, cx| {
                this.act(cx, |this, _| this.on_changed_files_width_changed(*position));
            });
            body = body.child(
                resize_handle::ResizeHandle::new(
                    "files-seam",
                    lead + files_width,
                    lead + diff_panel::MIN_FILE_NAV_WIDTH,
                    sidebar::RESIZE_HIT_PADDING,
                    move |position, window, cx| on_resize(&position, window, cx),
                )
                .right_reserve(theme::HAIRLINE + resize_handle::MIN_DIFF_PANE_WIDTH)
                .grabbable(files_width > 0.0)
                .view(),
            );
        }
        body
    }
}

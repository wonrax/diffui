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
use crate::modes::ModeKind;
use crate::theme::{self, ThemeSpec};
use crate::{
    Action, Cli, Cx, Diffui, MainView, WINDOW_STATE_DEBOUNCE, activity, chrome, diff_panel,
    empty_states, history_panel, icons, measure, menu, palette, resize_handle, sidebar,
    source_panel, tab_bar, toolbar, window_state::WindowState,
};
use diffui_core::OpenSpec;

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
        let consumed = crate::input::field_takes_key(window, &key, modifiers, cx);
        // Esc always lets go of a field, whoever else it means something to. Before
        // the keymap, so a mode that hands focus on (a palette column) wins.
        if key == keys::Key::Named(keys::Named::Escape) && crate::input::input_has_focus(window) {
            window.blur(cx);
        }
        let taken = app.update(cx, |app, cx| {
            app.modifiers = modifiers;
            app.act(cx, |app, cx| {
                app.on_key_pressed(&key, modifiers, consumed, cx)
            })
        });
        // Tab means nothing unless the keymap says so: no walking focus through
        // the buttons, which never hold it here.
        if taken || key == keys::Key::Named(keys::Named::Tab) {
            cx.stop_propagation();
        }
    })
    .detach();
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
    let bounds = match saved.position() {
        Some((x, y)) => Bounds::new(
            point(px(x), px(y)),
            size(px(window_size.0), px(window_size.1)),
        ),
        None => Bounds::centered(None, size(px(window_size.0), px(window_size.1)), cx),
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
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
    /// Runs a state change, then [`Diffui::reconcile`], then notifies. See the
    /// module docs.
    pub(crate) fn act<R>(
        &mut self,
        cx: &mut Cx,
        change: impl FnOnce(&mut Self, &mut Cx) -> R,
    ) -> R {
        let out = change(self, cx);
        self.reconcile(cx);
        cx.notify();
        out
    }

    /// A listener that runs `change` inside [`Diffui::act`].
    pub(crate) fn listen<E: 'static>(
        cx: &mut Cx,
        change: impl Fn(&mut Diffui, &E, &mut Window, &mut Cx) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        cx.listener(move |this: &mut Diffui, event: &E, window, cx| {
            change(this, event, window, cx);
            this.reconcile(cx);
            cx.notify();
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
        crate::app_menu::refresh_availability(self);
        self.keep_graph_anchor();
        self.cancel_unwanted_preview();
        self.ensure_welcome_prompt(cx);
        self.sync_conflicts();
        self.sync_history(cx);
        self.sync_media(cx);
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
        sync_ticker(
            &mut self.background.loading,
            work_in_flight,
            Duration::from_millis(120),
            cx,
            |_, cx| cx.notify(),
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

        // Prunes expired error toasts; only while any are up.
        sync_ticker(
            &mut self.background.toasts,
            !self.toasts.is_empty(),
            Duration::from_millis(500),
            cx,
            |this, cx| this.act(cx, |this, _| this.on_toast_tick()),
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
        self.on_window_opened(Some(bounds.origin.into()), bounds.size.into());
        self.apply_theme(cx);

        let subscriptions = vec![
            cx.observe_window_bounds(window, |this, window, cx| {
                let bounds = window.bounds();
                this.act(cx, |this, _| {
                    this.on_window_resized(bounds.size.into());
                    this.on_window_moved(bounds.origin.into());
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
            let updated = this.update(cx, |this, cx| {
                this.act(cx, |this, cx| {
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

impl Render for Diffui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let theme = self.resolved_theme().spec();

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
                .child(empty_states::welcome(self, theme, window, cx))
                .child(crate::shortcuts::build_overlay(self, theme, window, cx))
        } else {
            let panes = self.panes.clone().expect("a drawn app has its panes");
            self.shell(&panes, theme, window, cx)
        };
        self.frame(theme, cx).child(body).into_any_element()
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
    build: PaneBuild,
    #[cfg(test)]
    pub(crate) renders: usize,
    _app_changed: Subscription,
}

type PaneBuild = fn(&Diffui, ThemeSpec, &mut Window, &mut Cx) -> AnyElement;

impl Pane {
    fn new(app: Entity<Diffui>, build: PaneBuild, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self {
            app: app.downgrade(),
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
        match self.app.upgrade() {
            Some(app) => app.update(cx, |ui, cx| {
                let theme = ui.resolved_theme().spec();
                build(ui, theme, window, cx)
            }),
            None => div().into_any_element(),
        }
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
    pub(crate) floats: Entity<Pane>,
}

impl std::fmt::Debug for Panes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Panes").finish_non_exhaustive()
    }
}

impl Panes {
    fn new(app: Entity<Diffui>, cx: &mut App) -> Self {
        Self {
            tab_bar: Pane::new(app.clone(), tab_bar::build_tab_bar, cx),
            toolbar: Pane::new(app.clone(), toolbar::build_toolbar, cx),
            left: Pane::new(app.clone(), left_pane, cx),
            files: Pane::new(app.clone(), files_pane, cx),
            main: Pane::new(app.clone(), main_pane, cx),
            floats: Pane::new(app, floats_pane, cx),
        }
    }
}

/// The revision list, or what the view shows in its place.
fn left_pane(ui: &Diffui, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> AnyElement {
    match ui.active().main_view {
        MainView::Diff if ui.active().history_panel_collapsed => {
            sidebar::build_collapsed_sidebar(ui, theme, window, cx)
        }
        MainView::Diff => sidebar::build_sidebar(ui, theme, window, cx),
        MainView::Source => source_panel::build_source_sidebar(ui, theme, window, cx),
        MainView::History => history_panel::build_history_sidebar(ui, theme, window, cx),
    }
}

/// The changed files, in the Diff view.
fn files_pane(ui: &Diffui, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> AnyElement {
    if ui.active().files_panel_collapsed {
        diff_panel::build_collapsed_file_navigator(theme, window, cx)
    } else {
        diff_panel::build_file_navigator(ui, theme, window, cx)
    }
}

/// What the view is about: the diff, a source file, the history.
fn main_pane(ui: &Diffui, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> AnyElement {
    match ui.active().main_view {
        MainView::Diff => diff_panel::build_diff_panel(ui, theme, window, cx),
        MainView::Source => source_panel::build_source_panel(ui, theme, window, cx),
        MainView::History => history_panel::build_history_panel(ui, theme, window, cx),
    }
}

/// Everything that floats above the shell. Each is empty when inactive, so they can
/// always be stacked.
fn floats_pane(ui: &Diffui, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> AnyElement {
    request_overlay_frames(ui, window);
    div()
        .relative()
        .size_full()
        .child(palette::build_overlay(ui, theme, window, cx))
        .child(activity::activity_popover(ui, theme, window, cx))
        .child(menu::build_overlay(ui, theme, window, cx))
        .child(tab_bar::build_open_repo_dialog(ui, theme, window, cx))
        .child(crate::bookmarks::build_name_prompt(ui, theme, window, cx))
        .child(crate::changes::split_modal(ui, theme, window, cx))
        .child(tab_bar::build_confirm_dialog(ui, theme, window, cx))
        .child(crate::cli_output::build_overlay(ui, theme, window, cx))
        .child(crate::credentials::build_overlay(ui, theme, window, cx))
        .child(crate::shortcuts::build_overlay(ui, theme, window, cx))
        .child(activity::toast_layer(ui, theme, window, cx))
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
        let modal_open = [
            ModeKind::ActivityPopover,
            ModeKind::Palette,
            ModeKind::Menu,
            ModeKind::OpenRepo,
            ModeKind::Confirm,
            ModeKind::NamePrompt,
            ModeKind::Credential,
            ModeKind::CliOutput,
            ModeKind::Shortcuts,
        ]
        .into_iter()
        .any(|kind| self.mode(kind).is_some())
            || crate::changes::split_shown(self);

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
            .child(self.panes(panes, theme, window, cx));
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
    fn panes(&self, panes: &Panes, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> Div {
        let has_files = !self.active().session.document.files.is_empty();
        let history_collapsed = self.active().history_panel_collapsed;
        let files_collapsed = self.active().files_panel_collapsed;
        let main_view = self.active().main_view;
        let collapsed = theme::COLLAPSED_PANEL_WIDTH;

        let history_width = if history_collapsed {
            collapsed
        } else {
            self.sidebar_width
        };
        let files_width = if !has_files {
            0.0
        } else if files_collapsed {
            collapsed
        } else {
            self.file_nav_width
        };

        let fixed = |width: f32, pane: &Entity<Pane>| {
            div()
                .h_full()
                .w(px(width))
                .flex_none()
                .overflow_hidden()
                .child(embed(pane))
        };
        let divider = || vertical_divider(theme);

        let mut row = div().flex().flex_row().size_full();
        row = row
            .child(fixed(history_width, &panes.left))
            .child(divider());
        if main_view == MainView::Diff && has_files {
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

        // The history pane's seam.
        if !history_collapsed {
            let reserve = files_width
                + if files_width == 0.0 { 1.0 } else { 2.0 }
                + resize_handle::MIN_DIFF_PANE_WIDTH;
            let on_resize = cx.listener(|this: &mut Diffui, width: &f32, _, cx| {
                this.act(cx, |this, _| this.on_sidebar_width_changed(*width));
            });
            body = body.child(
                resize_handle::ResizeHandle::new(
                    "history-seam",
                    self.sidebar_width,
                    self.sidebar_min_width,
                    sidebar::RESIZE_HIT_PADDING,
                    move |width, window, cx| on_resize(&width, window, cx),
                )
                .right_reserve(reserve)
                .view(),
            );
        }
        // The changed files' seam.
        if main_view == MainView::Diff && has_files && !files_collapsed {
            let handle_x = history_width + 1.0 + self.file_nav_width;
            let on_resize = cx.listener(|this: &mut Diffui, position: &f32, _, cx| {
                this.act(cx, |this, _| this.on_changed_files_width_changed(*position));
            });
            body = body.child(
                resize_handle::ResizeHandle::new(
                    "files-seam",
                    handle_x,
                    history_width + 1.0 + diff_panel::MIN_FILE_NAV_WIDTH,
                    sidebar::RESIZE_HIT_PADDING,
                    move |position, window, cx| on_resize(&position, window, cx),
                )
                .right_reserve(1.0 + resize_handle::MIN_DIFF_PANE_WIDTH)
                .view(),
            );
        }
        // The collapsed file rail's temporary tree.
        if main_view == MainView::Diff
            && files_collapsed
            && self.changed_files_peek_open
            && has_files
        {
            body = body.child(
                div()
                    .absolute()
                    .top(px(crate::field::PANEL_TOGGLE_INSET))
                    .bottom(px(crate::field::PANEL_TOGGLE_INSET))
                    .left(px(history_width + 2.0 + collapsed))
                    .child(diff_panel::build_file_navigator_peek(
                        self, theme, window, cx,
                    )),
            );
        }
        body
    }
}

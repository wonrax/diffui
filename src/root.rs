//! The root view: the window, its layout, and the wiring that stands in for
//! iced's runtime.
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
    AnyElement, App, Bounds, Context, Div, Entity, Render, Subscription, Task, Window,
    WindowBounds, WindowOptions, div, point, px, relative, size,
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

/// The loops that stand in for iced's subscriptions. Each lives while its
/// condition holds; [`Diffui::sync_background`] starts and drops them. Dropping
/// the task cancels the loop, as a subscription leaving the batch did.
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
/// twice, once to unfocus and once to close. `consumed` is derived from whether an
/// input has the keyboard; that's what keeps the revset box's typing out of the
/// plain-key bindings. A key the keymap takes stops here; the rest carry on to the
/// input as usual.
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
        let consumed = crate::input::input_has_focus(window);
        let taken = app.update(cx, |app, cx| {
            app.modifiers = modifiers;
            app.act(cx, |app, cx| {
                app.on_key_pressed(&key, modifiers, consumed, cx)
            })
        });
        if taken {
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
        self.cancel_unwanted_preview(cx);
        self.ensure_welcome_prompt(cx);
        self.sync_conflicts(cx);
        self.sync_history(cx);
        self.sync_media(cx);
        // Chokepoint: if the active tab coalesced a refresh while busy and it's
        // now idle, run it. A no-op when nothing's pending (the common case).
        if let Some(tab) = self.active_tab_id() {
            let effects = match self.tab_mut(tab) {
                Some(target) => target.session.take_pending_refresh(),
                None => Vec::new(),
            };
            self.run_effects(tab, effects, cx);
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

        // Prunes expired error toasts; only while any are up.
        sync_ticker(
            &mut self.background.toasts,
            !self.toasts.is_empty(),
            Duration::from_millis(500),
            cx,
            |this, cx| this.act(cx, |this, cx| this.on_toast_tick(cx)),
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
                this.act(cx, |this, cx| this.on_window_focus_changed(focused, cx));
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
        let theme = self.resolved_theme().spec();
        // Per-frame animations: the palette's push/pop, a menu's glow and the
        // submenu trajectory apex. A frame request keeps redrawing while they run.
        let palette_animating = self
            .palette()
            .is_some_and(|palette| palette.is_animating(std::time::Instant::now()));
        let menu_ticking = self
            .menu()
            .is_some_and(|menu| menu.glow.is_some() || !menu.open_path.is_empty());
        if palette_animating || menu_ticking {
            window.request_animation_frame();
        }

        #[cfg(test)]
        if let Some(scene) = self.background.scene.clone() {
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
            self.shell(theme, window, cx)
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

/// A 1px divider between panes.
pub(crate) fn vertical_divider(theme: ThemeSpec) -> Div {
    div().w(px(1.0)).h_full().flex_none().bg(theme.border)
}

/// A 1px rule between strips.
pub(crate) fn horizontal_divider(theme: ThemeSpec) -> Div {
    div().w_full().h(px(1.0)).flex_none().bg(theme.border)
}

impl Diffui {
    /// The whole window with a tab open.
    fn shell(&mut self, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> Div {
        let tab_bar = tab_bar::build_tab_bar(self, theme, window, cx);
        let toolbar = toolbar::build_toolbar(self, theme, window, cx);
        let body = self.panes(theme, window, cx);

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
            .child(tab_bar)
            .child(toolbar)
            .child(horizontal_divider(theme))
            .child(body);
        // While something floats above, nothing below it hovers or takes presses.
        if modal_open {
            shell = shell.child(div().absolute().inset_0().occlude());
        }

        // Overlays float above the whole shell. Each is empty when inactive, so
        // they can always be stacked.
        div()
            .relative()
            .size_full()
            .child(shell)
            .child(palette::build_overlay(self, theme, window, cx))
            .child(activity::activity_popover(self, theme, window, cx))
            .child(menu::build_overlay(self, theme, window, cx))
            .child(tab_bar::build_open_repo_dialog(self, theme, window, cx))
            .child(crate::bookmarks::build_name_prompt(self, theme, window, cx))
            .child(crate::changes::split_modal(self, theme, window, cx))
            .child(tab_bar::build_confirm_dialog(self, theme, window, cx))
            .child(crate::cli_output::build_overlay(self, theme, window, cx))
            .child(crate::credentials::build_overlay(self, theme, window, cx))
            .child(crate::shortcuts::build_overlay(self, theme, window, cx))
            .child(activity::toast_layer(self, theme, window, cx))
    }

    /// The panes under the toolbar: the history pane, the changed files and the
    /// diff; or the source browser's or the history view's pair. All loading
    /// feedback lives in the toolbar; there's no full-window cold-load takeover and
    /// no diff-pane spinner. On a cold load the sidebar simply grows from empty as
    /// batches arrive, and a revision switch keeps the prior diff until the new one
    /// replaces it.
    fn panes(&mut self, theme: ThemeSpec, window: &mut Window, cx: &mut Cx) -> Div {
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

        let pane = |width: f32, child: AnyElement| {
            div()
                .h_full()
                .w(px(width))
                .flex_none()
                .overflow_hidden()
                .child(child)
        };
        let fill = |child: AnyElement| {
            div()
                .h_full()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(child)
        };

        let mut row = div().flex().flex_row().size_full();
        match main_view {
            MainView::Diff => {
                let sidebar_view = if history_collapsed {
                    sidebar::build_collapsed_sidebar(self, theme, window, cx)
                } else {
                    sidebar::build_sidebar(self, theme, window, cx)
                };
                row = row
                    .child(pane(history_width, sidebar_view))
                    .child(vertical_divider(theme));
                if has_files {
                    let navigator = if files_collapsed {
                        diff_panel::build_collapsed_file_navigator(theme, window, cx)
                    } else {
                        diff_panel::build_file_navigator(self, theme, window, cx)
                    };
                    row = row
                        .child(pane(files_width, navigator))
                        .child(vertical_divider(theme));
                }
                row = row.child(fill(diff_panel::build_diff_panel(self, theme, window, cx)));
            }
            MainView::Source => {
                row = row
                    .child(pane(
                        self.sidebar_width,
                        source_panel::build_source_sidebar(self, theme, window, cx),
                    ))
                    .child(vertical_divider(theme))
                    .child(fill(source_panel::build_source_panel(
                        self, theme, window, cx,
                    )));
            }
            MainView::History => {
                row = row
                    .child(pane(
                        self.sidebar_width,
                        history_panel::build_history_sidebar(self, theme, window, cx),
                    ))
                    .child(vertical_divider(theme))
                    .child(fill(history_panel::build_history_panel(
                        self, theme, window, cx,
                    )));
            }
        }

        let mut body = div().relative().flex_1().min_h_0().w_full().child(row);

        // The history pane's seam.
        if !history_collapsed {
            let reserve = files_width
                + if files_width == 0.0 { 1.0 } else { 2.0 }
                + resize_handle::MIN_DIFF_PANE_WIDTH;
            let on_resize = cx.listener(|this: &mut Diffui, width: &f32, _, cx| {
                this.act(cx, |this, cx| this.on_sidebar_width_changed(*width, cx));
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
                this.act(cx, |this, cx| {
                    this.on_changed_files_width_changed(*position, cx)
                });
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

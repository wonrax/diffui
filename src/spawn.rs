//! Async work: where it runs, and how its result gets back to the app.
//!
//! The app's tokio-dependent futures (`tokio::process`, `tokio::time`) run on one
//! process-wide runtime; GPUI's executors don't drive tokio's reactor. A result
//! comes back through a foreground task that runs its handler inside
//! [`Diffui::act`], so reconcile runs after it like after any other change.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use gpui_kit::Task;
use tokio::runtime::Runtime;

use crate::root::PaneSet;
use crate::{Cx, Diffui};

/// The runtime behind [`Diffui::run_async`].
pub(crate) fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("diffui-async")
            .build()
            .expect("build the tokio runtime")
    })
}

impl Diffui {
    /// Runs `future` off the UI thread, then `done` with its output, inside
    /// `act`. Detached: it finishes even if nothing is waiting, so a slot that
    /// must not outlive a superseded request checks for that in `done`.
    pub(crate) fn run_async<T: Send + 'static>(
        future: impl Future<Output = T> + Send + 'static,
        cx: &mut Cx,
        done: impl FnOnce(&mut Diffui, T, &mut Cx) + 'static,
    ) {
        Self::run_async_on(future, PaneSet::ALL, cx, done);
    }

    /// [`Diffui::run_async`] for a result only `panes` show: `done` runs in
    /// [`Diffui::act_on`].
    pub(crate) fn run_async_on<T: Send + 'static>(
        future: impl Future<Output = T> + Send + 'static,
        panes: PaneSet,
        cx: &mut Cx,
        done: impl FnOnce(&mut Diffui, T, &mut Cx) + 'static,
    ) {
        let handle = runtime().spawn(future);
        cx.spawn(async move |this, cx| {
            let Ok(output) = handle.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.act_on(cx, panes, |this, cx| done(this, output, cx));
            });
        })
        .detach();
    }

    /// `run` after `delay`, inside `act`. Something the user can supersede
    /// before it fires wants a [`Debounce`] instead.
    pub(crate) fn after(
        delay: Duration,
        cx: &mut Cx,
        run: impl FnOnce(&mut Diffui, &mut Cx) + 'static,
    ) {
        let wait = async move { tokio::time::sleep(delay).await };
        Self::run_async(wait, cx, move |this, (), cx| run(this, cx));
    }

    /// `run` once the current update has returned, inside its own `act`: the
    /// work happens after the update that asked for it (and its reconcile),
    /// not in the middle of it.
    pub(crate) fn later(cx: &mut Cx, run: impl FnOnce(&mut Diffui, &mut Cx) + 'static) {
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = this.update(cx, |this, cx| {
                this.act(cx, run);
            });
        });
    }

    /// Runs `f` with the window, once the current update has returned. Update
    /// code has no `Window` and can't get one mid-update (GPUI takes it out of
    /// the app while its view updates), so anything that truly needs it waits.
    /// A no-op before the window opens.
    pub(crate) fn with_window(
        &self,
        cx: &mut Cx,
        f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) + 'static,
    ) {
        let Some(handle) = self.window else {
            return;
        };
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| f(window, cx));
        });
    }
}

/// A timer that re-arming restarts. The pending run lives in the task held
/// here, so a superseded one is dropped before it fires rather than firing
/// and finding itself stale.
///
/// It waits on the tokio runtime, not GPUI's timer: the harness runs GPUI on
/// a simulated clock that only moves when told to.
#[derive(Default)]
pub(crate) struct Debounce(Option<Task<()>>);

impl std::fmt::Debug for Debounce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Debounce").field(&self.0.is_some()).finish()
    }
}

impl Debounce {
    /// `run` after `delay`, inside `act`, unless this is armed or cancelled
    /// again first.
    pub(crate) fn arm(
        &mut self,
        delay: Duration,
        cx: &mut Cx,
        run: impl FnOnce(&mut Diffui, &mut Cx) + 'static,
    ) {
        let wait = runtime().spawn(async move { tokio::time::sleep(delay).await });
        self.0 = Some(cx.spawn(async move |this, cx| {
            if wait.await.is_ok() {
                let _ = this.update(cx, |this, cx| this.act(cx, run));
            }
        }));
    }

    pub(crate) fn cancel(&mut self) {
        self.0 = None;
    }
}

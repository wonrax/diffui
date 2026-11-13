//! One QuickJS runtime and context holding one plugin.
//!
//! A plain (non-async) rquickjs runtime: promises are settled by messages the
//! actor feeds in, so there is no executor here, only [`Js::turn`], which
//! enters the script once, drains the job queue and reports what went wrong.
//! Each turn runs under a deadline the interrupt handler enforces.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rquickjs::convert::Coerced;
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::promise::PromiseState;
use rquickjs::{
    Context, Ctx, Exception, Function, Module, Object, Persistent, Promise, Runtime, TypedArray,
    Value,
};

use crate::Limits;
use crate::host::Registration;
use crate::manifest::Manifest;
use crate::memory::Capped;
use crate::protocol::{EVENT_NAMES, FORGE_METHODS};

/// The module a plugin imports, and the name its frames carry in a stack.
const HOST_MODULE: &str = "diffui";
const HOST_SOURCE: &str = include_str!("js/diffui.js");

/// What `import { … } from "diffui"` offers. A test holds this, the module
/// and `assets/diffui.d.ts` to the same list.
pub(crate) const EXPORTS: [&str; 9] = [
    "auth", "commands", "config", "events", "exec", "forge", "http", "kv", "ui",
];

/// An exception from a script.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct JsError {
    /// The error's `name` (`TypeError`, `PermissionError`, …).
    pub name: String,
    pub message: String,
    /// QuickJS's stack: one `    at function (file:line:col)` line per frame.
    #[serde(default)]
    pub stack: String,
}

impl JsError {
    pub(crate) fn plain(message: impl Into<String>) -> Self {
        Self {
            name: "Error".into(),
            message: message.into(),
            stack: String::new(),
        }
    }

    /// `file:line:col` of the innermost frame in the plugin's own files.
    pub fn location(&self) -> Option<&str> {
        self.stack.lines().find_map(|frame| {
            let frame = frame.trim().strip_prefix("at ")?;
            let place = match frame.rsplit_once('(') {
                Some((_, rest)) => rest.strip_suffix(')')?,
                None => frame,
            };
            let file = place.split(':').next()?;
            (file != HOST_MODULE && file != "native" && place.contains(':')).then_some(place)
        })
    }
}

/// `main.js:12:9: TypeError: x is not a function`.
impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(location) = self.location() {
            write!(f, "{location}: ")?;
        }
        write!(f, "{}: {}", self.name, self.message)
    }
}

/// Why a plugin can't go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// A turn ran past its time budget and was interrupted.
    Budget,
    Memory,
}

/// What one entry into the script left behind.
#[derive(Debug, Default)]
pub(crate) struct Turn {
    /// Exceptions nothing caught: thrown out of a timer or stream callback,
    /// or a rejected promise nobody handled by the end of the turn.
    pub uncaught: Vec<JsError>,
    pub fault: Option<Fault>,
}

/// Why the main module never started.
pub(crate) enum StartError {
    Fault(Fault),
    Threw(JsError),
}

pub(crate) enum MainState {
    Pending,
    Loaded,
    Failed(JsError),
}

struct Shared {
    /// Messages the script posted with `send`, in order.
    outbox: RefCell<Vec<String>>,
    /// How many bytes the outbox may still take this turn. It lives outside
    /// the capped heap, so it gets the same cap of its own.
    outbox_room: Cell<usize>,
    memory: usize,
    deadline: Cell<Instant>,
    interrupted: Cell<bool>,
    /// The heap cap refused an allocation.
    out_of_memory: Rc<Cell<bool>>,
    /// Promises rejected with no handler yet, and their reasons.
    rejections: RefCell<Vec<(Persistent<Value<'static>>, Persistent<Value<'static>>)>>,
}

impl Shared {
    fn fault(&self) -> Option<Fault> {
        if self.interrupted.get() {
            Some(Fault::Budget)
        } else if self.out_of_memory.get() {
            Some(Fault::Memory)
        } else {
            None
        }
    }
}

pub(crate) struct Js {
    // Field order is drop order: every handle into the engine goes before the
    // context, and the context before the runtime.
    dispatch: Persistent<Function<'static>>,
    finish_load: Persistent<Function<'static>>,
    main: Persistent<Promise<'static>>,
    context: Context,
    runtime: Runtime,
    shared: Rc<Shared>,
    budget: Duration,
}

impl Drop for Js {
    fn drop(&mut self) {
        // These hold engine values and are owned by a closure the runtime
        // owns; let go of them while the runtime is still there.
        self.shared.rejections.borrow_mut().clear();
    }
}

impl Js {
    /// Build the context, evaluate the host module, then start the plugin's
    /// main module. Its promise may still be pending (top-level `await`);
    /// [`Js::main_state`] says when it settles.
    pub(crate) fn start(
        manifest: &Manifest,
        config: &serde_json::Map<String, serde_json::Value>,
        limits: &Limits,
    ) -> Result<(Js, Turn), StartError> {
        let shared = Rc::new(Shared {
            outbox: RefCell::default(),
            outbox_room: Cell::new(limits.memory),
            memory: limits.memory,
            deadline: Cell::new(Instant::now() + limits.turn),
            interrupted: Cell::new(false),
            out_of_memory: Rc::default(),
            rejections: RefCell::default(),
        });
        let engine = |error: rquickjs::Error| {
            StartError::Threw(JsError::plain(format!(
                "the script engine did not start: {error}"
            )))
        };
        let heap = Capped::new(limits.memory, shared.out_of_memory.clone());
        let runtime = Runtime::new_with_alloc(heap).map_err(engine)?;
        runtime.set_max_stack_size(limits.stack);
        runtime.set_interrupt_handler(Some(Box::new({
            let shared = shared.clone();
            move || {
                let over = Instant::now() >= shared.deadline.get();
                if over {
                    shared.interrupted.set(true);
                }
                over
            }
        })));
        runtime.set_host_promise_rejection_tracker(Some(Box::new({
            let shared = shared.clone();
            move |ctx, promise, reason, handled| {
                let mut rejections = shared.rejections.borrow_mut();
                if handled {
                    rejections.retain(|(seen, _)| {
                        seen.clone().restore(&ctx).is_ok_and(|seen| seen != promise)
                    });
                } else {
                    rejections.push((
                        Persistent::save(&ctx, promise),
                        Persistent::save(&ctx, reason),
                    ));
                }
            }
        })));
        runtime.set_loader(
            PluginResolver {
                dir: manifest.dir.clone(),
            },
            PluginLoader {
                dir: manifest.dir.clone(),
            },
        );
        let context = Context::full(&runtime).map_err(engine)?;

        let boot = serde_json::json!({
            "config": config,
            "commands": manifest.commands.iter().map(|command| &command.id).collect::<Vec<_>>(),
            "forge": manifest.forge.is_some(),
            "forgeMethods": FORGE_METHODS,
            "events": EVENT_NAMES,
        })
        .to_string();
        let handles = context.with(|ctx| {
            let started = (|| {
                let boot: Object = ctx.json_parse(boot)?.get()?;
                boot.set("native", native(&ctx, &shared)?)?;
                ctx.globals().set("__diffui_boot", boot.clone())?;
                Module::evaluate(ctx.clone(), HOST_MODULE, HOST_SOURCE)?.finish::<()>()?;
                let dispatch: Function = boot.get("dispatch")?;
                let finish_load: Function = boot.get("finishLoad")?;
                let source = read_module(&manifest.dir, &manifest.main)
                    .map_err(|why| Exception::throw_message(&ctx, &why))?;
                let (_, main) =
                    Module::declare(ctx.clone(), manifest.main.as_str(), source)?.eval()?;
                Ok((
                    Persistent::save(&ctx, dispatch),
                    Persistent::save(&ctx, finish_load),
                    Persistent::save(&ctx, main),
                ))
            })();
            started.map_err(|error| caught(&ctx, error))
        });
        let (dispatch, finish_load, main) = match handles {
            Ok(handles) => handles,
            Err(error) => {
                // Same care as `Drop for Js`, which doesn't exist yet here.
                shared.rejections.borrow_mut().clear();
                // An interrupted or starved start throws too; the fault is
                // the cause worth reporting.
                return Err(match shared.fault() {
                    Some(fault) => StartError::Fault(fault),
                    None => StartError::Threw(error),
                });
            }
        };
        let js = Js {
            dispatch,
            finish_load,
            main,
            context,
            runtime,
            shared,
            budget: limits.turn,
        };
        // The main module's synchronous part ran above, inside the deadline
        // set at the top; this drains what it queued.
        let turn = js.finish_turn(Vec::new());
        Ok((js, turn))
    }

    /// Hand one message to the script's `dispatch` and run it to quiescence.
    pub(crate) fn dispatch(&self, message: &str) -> Turn {
        self.shared.deadline.set(Instant::now() + self.budget);
        self.shared.interrupted.set(false);
        let mut uncaught = Vec::new();
        self.context.with(|ctx| {
            let called = self
                .dispatch
                .clone()
                .restore(&ctx)
                .and_then(|dispatch| dispatch.call::<_, ()>((message,)));
            if let Err(error) = called {
                uncaught.push(caught(&ctx, error));
            }
        });
        self.finish_turn(uncaught)
    }

    /// Drain the job queue under the turn's deadline, then collect what
    /// nothing caught.
    fn finish_turn(&self, mut uncaught: Vec<JsError>) -> Turn {
        while !self.shared.interrupted.get() {
            match self.runtime.execute_pending_job() {
                Ok(false) => break,
                Ok(true) => {
                    // A chain of tiny jobs never trips the interrupt handler.
                    if Instant::now() >= self.shared.deadline.get() {
                        self.shared.interrupted.set(true);
                    }
                }
                Err(job) => job.0.with(|ctx| uncaught.push(describe(&ctx, ctx.catch()))),
            }
        }
        let rejections = std::mem::take(&mut *self.shared.rejections.borrow_mut());
        self.context.with(|ctx| {
            let main = self.main.clone().restore(&ctx).ok();
            for (promise, reason) in rejections {
                let (Ok(promise), Ok(reason)) = (promise.restore(&ctx), reason.restore(&ctx))
                else {
                    continue;
                };
                // A failed load is reported as that, by `main_state`.
                if main.as_ref().is_none_or(|main| main.as_value() != &promise) {
                    uncaught.push(describe(&ctx, reason));
                }
            }
        });
        Turn {
            uncaught,
            fault: self.shared.fault(),
        }
    }

    /// A limit the script has run into, whether or not a turn reported it
    /// (a panic can come first).
    pub(crate) fn fault(&self) -> Option<Fault> {
        self.shared.fault()
    }

    /// What the script posted since the last call.
    pub(crate) fn take_outbox(&self) -> Vec<String> {
        self.shared.outbox_room.set(self.shared.memory);
        std::mem::take(&mut *self.shared.outbox.borrow_mut())
    }

    pub(crate) fn main_state(&self) -> MainState {
        self.context.with(|ctx| {
            let Ok(main) = self.main.clone().restore(&ctx) else {
                return MainState::Failed(JsError::plain("the main module was lost"));
            };
            match main.state() {
                PromiseState::Pending => MainState::Pending,
                PromiseState::Resolved => MainState::Loaded,
                PromiseState::Rejected => match main.result::<Value>() {
                    Some(Err(error)) => MainState::Failed(caught(&ctx, error)),
                    _ => MainState::Failed(JsError::plain("the main module failed")),
                },
            }
        })
    }

    /// Close registration and read what the main module registered.
    pub(crate) fn finish_load(&self) -> Result<Registration, JsError> {
        self.context.with(|ctx| {
            let registered = self
                .finish_load
                .clone()
                .restore(&ctx)
                .and_then(|finish| finish.call::<_, String>(()))
                .map_err(|error| caught(&ctx, error))?;
            serde_json::from_str(&registered)
                .map_err(|error| JsError::plain(format!("unreadable registration: {error}")))
        })
    }
}

/// The three functions the host module is built on.
fn native<'js>(ctx: &Ctx<'js>, shared: &Rc<Shared>) -> rquickjs::Result<Object<'js>> {
    let native = Object::new(ctx.clone())?;
    let shared = shared.clone();
    native.set(
        "send",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, message: String| match shared
                .outbox_room
                .get()
                .checked_sub(message.len())
            {
                Some(room) => {
                    shared.outbox_room.set(room);
                    shared.outbox.borrow_mut().push(message);
                    Ok(())
                }
                None => {
                    shared.out_of_memory.set(true);
                    Err(Exception::throw_range(
                        &ctx,
                        "too much sent to the host at once",
                    ))
                }
            },
        )?,
    )?;
    native.set(
        "utf8Encode",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, text: String| {
            TypedArray::<u8>::new(ctx, text.into_bytes())
        })?,
    )?;
    native.set(
        "utf8Decode",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, bytes: TypedArray<'js, u8>, fatal: bool| {
                // SAFETY: the slice is copied into a `String` before any script
                // can run again, so nothing can move or free the buffer under it.
                let bytes = unsafe { bytes.as_bytes() }.unwrap_or_default();
                let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
                match std::str::from_utf8(bytes) {
                    Ok(text) => Ok(text.to_string()),
                    Err(_) if fatal => Err(Exception::throw_type(
                        &ctx,
                        "The encoded data was not valid utf-8",
                    )),
                    Err(_) => Ok(String::from_utf8_lossy(bytes).into_owned()),
                }
            },
        )?,
    )?;
    Ok(native)
}

/// Turn an rquickjs error into the exception behind it.
fn caught(ctx: &Ctx<'_>, error: rquickjs::Error) -> JsError {
    match error {
        rquickjs::Error::Exception => describe(ctx, ctx.catch()),
        other => JsError::plain(other.to_string()),
    }
}

fn describe<'js>(ctx: &Ctx<'js>, thrown: Value<'js>) -> JsError {
    let text = |value: Value<'js>| {
        value
            .get::<Coerced<String>>()
            .map(|text| text.0)
            .unwrap_or_default()
    };
    match thrown
        .as_object()
        .filter(|object| object.contains_key("message").unwrap_or(false))
    {
        Some(error) => JsError {
            name: error
                .get("name")
                .map(text)
                .unwrap_or_else(|_| "Error".into()),
            message: error.get("message").map(text).unwrap_or_default(),
            stack: error
                .get::<_, Option<String>>("stack")
                .ok()
                .flatten()
                .unwrap_or_default(),
        },
        None => {
            // Reading the value may itself throw; that must not stay pending.
            let shown = text(thrown);
            let _ = ctx.catch();
            JsError::plain(format!("threw {shown}"))
        }
    }
}

struct PluginResolver {
    dir: PathBuf,
}

impl Resolver for PluginResolver {
    fn resolve<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if name == HOST_MODULE {
            return Ok(name.to_string());
        }
        if !(name.starts_with("./") || name.starts_with("../")) {
            return Err(Exception::throw_reference(
                ctx,
                &format!(
                    "Cannot import \"{name}\" (from {base}). A plugin can import \"{HOST_MODULE}\" ({}) and its own files by relative path (\"./util.js\"). There is no Node, npm or require here.",
                    EXPORTS.join(", ")
                ),
            ));
        }
        let mut parts: Vec<&str> = base.split('/').collect();
        parts.pop();
        for part in name.split('/') {
            match part {
                "." | "" => {}
                ".." => {
                    if parts.pop().is_none() {
                        return Err(Exception::throw_reference(
                            ctx,
                            &format!(
                                "Cannot import \"{name}\" (from {base}): it is outside the plugin's directory"
                            ),
                        ));
                    }
                }
                part => parts.push(part),
            }
        }
        let resolved = parts.join("/");
        if !self.dir.join(&resolved).is_file() {
            return Err(Exception::throw_reference(
                ctx,
                &format!(
                    "Cannot import \"{name}\" (from {base}): there is no {resolved} in the plugin's directory. It has: {}.",
                    modules_in(&self.dir).join(", ")
                ),
            ));
        }
        Ok(resolved)
    }
}

struct PluginLoader {
    dir: PathBuf,
}

impl Loader for PluginLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        let source =
            read_module(&self.dir, name).map_err(|why| Exception::throw_reference(ctx, &why))?;
        Module::declare(ctx.clone(), name, source)
    }
}

/// Read a module by its path relative to the plugin directory.
fn read_module(dir: &Path, name: &str) -> Result<String, String> {
    let inside = Path::new(name)
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    if !inside {
        return Err(format!("\"{name}\" is outside the plugin's directory"));
    }
    std::fs::read_to_string(dir.join(name)).map_err(|error| format!("Cannot read {name}: {error}"))
}

/// The JavaScript files at the top of a plugin's directory, for the message
/// about one that isn't there.
fn modules_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".js") || name.ends_with(".mjs"))
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(stack: &str) -> JsError {
        JsError {
            name: "TypeError".into(),
            message: "boom".into(),
            stack: stack.into(),
        }
    }

    #[test]
    fn the_location_is_the_first_frame_in_the_plugins_files() {
        let thrown = error(
            "    at call (diffui:42:17)\n    at fetch (diffui:500:9)\n    at api (lib/api.js:7:21)\n    at <anonymous> (main.js:3:1)\n",
        );
        assert_eq!(thrown.location(), Some("lib/api.js:7:21"));
        assert_eq!(thrown.to_string(), "lib/api.js:7:21: TypeError: boom");
        assert_eq!(
            error("    at main.js:2:5\n").location(),
            Some("main.js:2:5")
        );
        assert_eq!(
            error("    at map (native)\n    at run (main.js:9:3)\n").location(),
            Some("main.js:9:3")
        );
        assert_eq!(error("").to_string(), "TypeError: boom");
    }
}

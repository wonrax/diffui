//! Diagnostics plumbing: the `tracing` subscriber, the log file it writes, and
//! the panic hook.
//!
//! The core logs through `tracing`, but nothing used to install a subscriber,
//! so every actor warning vanished. [`init`] installs one that always writes
//! to a size-capped log file — a GUI launch has no terminal to read — and also
//! to stderr when there is a terminal attached.
//!
//! The file lives at `~/Library/Logs/diffui/diffui.log` on macOS (where
//! Console.app looks) and `$XDG_STATE_HOME/diffui/diffui.log` (or
//! `~/.local/state/diffui/diffui.log`) elsewhere. Paths are built from `$HOME`
//! rather than the account database, so a sandboxed `HOME` gets its own log.

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Once the file passes this size it is rotated to `diffui.log.1` (replacing
/// the previous one), so the two together stay under twice this.
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

/// Our own crates at `info`, everything else (iced, wgpu, jj-lib) at `warn`.
const DEFAULT_FILTER: &str = "warn,diffui=info,diffui_core=info";

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Where the log file is written, whether or not [`init`] managed to open it.
pub(crate) fn log_path() -> Option<PathBuf> {
    if let Some(path) = LOG_PATH.get() {
        return Some(path.clone());
    }
    default_log_path()
}

fn default_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return Some(home?.join("Library/Logs/diffui/diffui.log"));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".local/state")))?;
    Some(state.join("diffui/diffui.log"))
}

/// Install the subscriber and the panic hook. Call once, first thing in
/// `main`. Never fails the launch: without a writable log directory the app
/// still runs, logging to stderr only.
pub(crate) fn init() {
    let filter = filter_from_env();
    let file = default_log_path().and_then(|path| match RotatingFile::open(&path) {
        Ok(file) => {
            let _ = LOG_PATH.set(path);
            Some(file)
        }
        Err(error) => {
            eprintln!("diffui: cannot open log file {}: {error}", path.display());
            None
        }
    });
    let file_layer = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_thread_names(true)
            .with_writer(Mutex::new(file))
    });
    let terminal = io::stderr().is_terminal();
    let stderr_layer = (terminal || file_layer.is_none()).then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(terminal)
            .with_writer(io::stderr)
    });
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
    install_panic_hook();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        log = %log_path().map(|path| path.display().to_string()).unwrap_or_default(),
        "diffui starting"
    );
}

/// `DIFFUI_LOG`, then `RUST_LOG`, then [`DEFAULT_FILTER`]. A directive that
/// doesn't parse falls back to the default rather than silencing everything.
fn filter_from_env() -> EnvFilter {
    let requested = ["DIFFUI_LOG", "RUST_LOG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
    if let Some(directives) = requested {
        match EnvFilter::try_new(&directives) {
            Ok(filter) => return filter,
            Err(error) => eprintln!("diffui: ignoring log filter {directives:?}: {error}"),
        }
    }
    EnvFilter::new(DEFAULT_FILTER)
}

/// Log every panic with its backtrace before the default hook prints it. A
/// panic an actor catches still passes through here, which is the only place
/// its backtrace is ever seen.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            thread = thread.name().unwrap_or("<unnamed>"),
            "panic: {info}\n{backtrace}"
        );
        previous(info);
    }));
}

/// An append-only log file that rotates itself to `<name>.1` once it passes
/// [`MAX_LOG_BYTES`], checked at open and on every write, so a long session
/// that logs in a loop can't fill the disk.
struct RotatingFile {
    path: PathBuf,
    file: File,
    len: u64,
}

impl RotatingFile {
    fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let len = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        let mut file = Self {
            path: path.to_owned(),
            file: Self::append(path)?,
            len,
        };
        if len >= MAX_LOG_BYTES {
            file.rotate()?;
        }
        Ok(file)
    }

    fn append(path: &Path) -> io::Result<File> {
        OpenOptions::new().create(true).append(true).open(path)
    }

    fn rotate(&mut self) -> io::Result<()> {
        let mut rotated = self.path.clone().into_os_string();
        rotated.push(".1");
        fs::rename(&self.path, rotated)?;
        self.file = Self::append(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.len >= MAX_LOG_BYTES {
            // Keep logging into the current file if the rename fails; a log
            // that grows is better than one that stops.
            let _ = self.rotate();
        }
        let written = self.file.write(buf)?;
        self.len += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Show the log file in the platform's file manager: selected in Finder on
/// macOS, its directory opened elsewhere.
pub(crate) fn reveal_log_file() {
    let Some(path) = log_path() else {
        tracing::warn!("no log file location: HOME is not set");
        return;
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        if path.exists() {
            command.arg("-R").arg(&path);
        } else {
            command.arg(path.parent().unwrap_or(&path));
        }
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = {
        let mut command = std::process::Command::new(if cfg!(windows) {
            "explorer"
        } else {
            "xdg-open"
        });
        command.arg(path.parent().unwrap_or(&path));
        command
    };
    if let Err(error) = command.spawn() {
        tracing::warn!(%error, path = %path.display(), "failed to reveal the log file");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_file_rotates_past_its_cap() {
        let dir = std::env::temp_dir().join(format!("diffui-log-rotate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("diffui.log");
        let mut file = RotatingFile::open(&path).expect("open the log");
        file.len = MAX_LOG_BYTES;
        file.write_all(b"after\n").expect("write past the cap");
        let mut rotated = path.clone().into_os_string();
        rotated.push(".1");
        assert!(Path::new(&rotated).exists(), "the full file moved aside");
        assert_eq!(fs::read_to_string(&path).unwrap(), "after\n");
        let _ = fs::remove_dir_all(&dir);
    }
}

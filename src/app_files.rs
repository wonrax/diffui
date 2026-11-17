//! Where the app keeps its files, and how it writes them.
//!
//! Each kind keeps the root it has always had, so nothing moves under an
//! existing install: config and recents follow XDG everywhere (dotfile-managed
//! on macOS too), window state and logs take each OS's own place for them.

use std::env;
use std::path::{Path, PathBuf};

/// What a file is, which decides where it lives.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AppDir {
    /// Settings the user edits: `$XDG_CONFIG_HOME`, else `~/.config`.
    Config,
    /// History worth keeping (recents): `$XDG_DATA_HOME`, else
    /// `~/.local/share`.
    Data,
    /// Per-machine layout (window geometry, the open session): Application
    /// Support on macOS, `%LOCALAPPDATA%` on Windows, else the XDG state dir.
    State,
    /// The log: `~/Library/Logs` on macOS, else the XDG state dir.
    Logs,
}

/// The path of the app's file `name` of kind `dir`, or `None` when the home
/// directory it hangs off is unknown.
pub(crate) fn app_file(dir: AppDir, name: &str) -> Option<PathBuf> {
    let home = || {
        env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
    };
    let xdg = |var: &str, default: &str| {
        env::var_os(var)
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| Some(home()?.join(default)))
    };
    let root = match dir {
        AppDir::Config => xdg("XDG_CONFIG_HOME", ".config")?,
        AppDir::Data => xdg("XDG_DATA_HOME", ".local/share")?,
        // Non-roaming: window coordinates shouldn't follow the user to a
        // machine with another monitor layout.
        AppDir::State if cfg!(windows) => env::var_os("LOCALAPPDATA")
            .or_else(|| env::var_os("APPDATA"))
            .map(PathBuf::from)?,
        AppDir::State if cfg!(target_os = "macos") => home()?.join("Library/Application Support"),
        AppDir::Logs if cfg!(target_os = "macos") => home()?.join("Library/Logs"),
        AppDir::State | AppDir::Logs => xdg("XDG_STATE_HOME", ".local/state")?,
    };
    Some(root.join("diffui").join(name))
}

/// Write `bytes` to `path` whole or not at all, creating its directory. The
/// bytes go beside the file and are renamed over it, which is atomic on the
/// platforms we ship: a save that dies part-way (a quit mid-write, a full
/// disk) leaves the old file, not a truncated one that loads as nothing.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".new");
    let temp = PathBuf::from(temp);
    let written = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_atomic_write_replaces_the_file_and_leaves_nothing_beside_it() {
        let dir = env::temp_dir().join(format!("diffui-write-atomic-{}", std::process::id()));
        let path = dir.join("nested/recents.toml");
        write_atomic(&path, b"old").expect("the first write");
        write_atomic(&path, b"new").expect("the second write");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["recents.toml"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! The plugins diffui ships (`plugins/bundled/<id>/`, embedded when it is
//! built): what they are, where they say they live, and `diffui plugin
//! eject`, which copies one into the plugins folder to edit.
//!
//! A bundled plugin runs from memory with the permissions it declares; the
//! user never has to allow it. A plugin of the same id in the plugins
//! folder replaces it, under the folder's usual trust rules.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use diffui_plugins::MANIFEST_FILE;

use crate::config::display_path;

/// One bundled plugin: its id, and its files by path relative to its
/// directory.
pub(crate) struct Bundled {
    pub(crate) id: String,
    pub(crate) files: BTreeMap<String, String>,
}

/// The bundled plugins. A test puts its own under `~/.bundled-test` instead
/// (laid out like `plugins/bundled`), which replaces the built-in set.
pub(crate) fn bundled() -> Vec<Bundled> {
    #[cfg(test)]
    if let Some(home) = std::env::var_os("HOME") {
        let dir = Path::new(&home).join(".bundled-test");
        if dir.is_dir() {
            return from_dir(&dir);
        }
    }
    diffui_plugins::BUNDLED
        .iter()
        .map(|(id, files)| Bundled {
            id: (*id).to_owned(),
            files: files
                .iter()
                .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
fn from_dir(dir: &Path) -> Vec<Bundled> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, files);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                files.insert(name, text);
            }
        }
    }
    let mut plugins: Vec<Bundled> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let mut files = BTreeMap::new();
            walk(&entry.path(), &entry.path(), &mut files);
            Bundled {
                id: entry.file_name().to_string_lossy().into_owned(),
                files,
            }
        })
        .collect();
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    plugins
}

/// Where a bundled plugin says it is, in messages and Settings: nowhere on
/// disk.
pub(crate) fn place(id: &str) -> PathBuf {
    Path::new("(bundled)").join(id)
}

/// Copy the bundled plugin `id` into `folder` (the plugins folder) with the
/// API's types and an editor config, as `diffui plugin init` writes them.
/// The copy replaces the bundled plugin from then on, and is asked about
/// like any plugin in the folder.
pub(crate) fn eject(id: &str, folder: &Path) -> Result<String, String> {
    let all = bundled();
    let Some(plugin) = all.iter().find(|plugin| plugin.id == id) else {
        let known: Vec<&str> = all.iter().map(|plugin| plugin.id.as_str()).collect();
        return Err(if known.is_empty() {
            "This diffui has no bundled plugins".to_owned()
        } else {
            format!("No bundled plugin {id:?}; there are {}", known.join(", "))
        });
    };
    let dir = folder.join(id);
    if dir.join(MANIFEST_FILE).exists() {
        return Err(format!(
            "{} already holds a plugin; move it away to eject the bundled one again",
            display_path(&dir)
        ));
    }
    let ts = plugin.files.keys().any(|name| name.ends_with(".ts"));
    let (config_name, config) = super::cli::editor_config(ts);
    let mut files: Vec<(String, String)> = plugin
        .files
        .iter()
        .map(|(name, text)| (name.clone(), text.clone()))
        .collect();
    files.push(("diffui.d.ts".to_owned(), diffui_plugins::TYPES.to_owned()));
    files.push((config_name.to_owned(), config.to_owned()));
    let mut said = format!("Copied the bundled {id} plugin to {}\n", display_path(&dir));
    for (name, text) in files {
        let path = dir.join(&name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", display_path(parent)))?;
        }
        std::fs::write(&path, text).map_err(|error| format!("{}: {error}", display_path(&path)))?;
        let _ = writeln!(said, "  {name}");
    }
    said.push_str(
        "It replaces the bundled one from the next start (or `diffui ctl reload`), and diffui asks \
         before it runs it. Delete the folder to go back to the bundled one.\n",
    );
    Ok(said)
}

//! Embeds the plugins diffui ships: every `bundled/<id>/` directory's
//! manifest and modules become [`BUNDLED`](crate::BUNDLED).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("bundled");
    // Cargo scans a directory it is told to watch, files and all. Until there
    // is a `bundled` directory, the crate's own is watched for it to appear.
    let watched = if root.is_dir() {
        root.clone()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
    };
    println!("cargo::rerun-if-changed={}", watched.display());
    let mut plugins: Vec<PathBuf> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("plugin.toml").is_file())
        .collect();
    plugins.sort();
    let mut out = String::from("&[\n");
    for dir in plugins {
        let id = dir.file_name().unwrap().to_string_lossy().into_owned();
        let _ = writeln!(out, "    ({id:?}, &[");
        let mut files = Vec::new();
        collect(&dir, &dir, &mut files);
        files.sort();
        for (name, path) in files {
            let _ = writeln!(
                out,
                "        ({name:?}, include_str!({:?})),",
                path.display().to_string()
            );
        }
        out.push_str("    ]),\n");
    }
    out.push_str("]\n");
    let target = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("bundled.rs");
    std::fs::write(target, out).unwrap();
}

/// The manifest and the modules under `dir`, by path relative to `root`.
/// Type definitions and editor files stay out: `diffui plugin eject` writes
/// fresh ones.
fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect(root, &path, files);
        } else if name == "plugin.toml"
            || [".js", ".mjs", ".ts", ".mts"]
                .iter()
                .any(|ext| name.ends_with(ext) && !name.ends_with(".d.ts"))
        {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            files.push((relative, path));
        }
    }
}

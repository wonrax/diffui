//! Embeds the jj-lib version Cargo.lock resolved, for the app's check against
//! the `jj` CLI on PATH. The Lucide font the widgets embed is fetched by
//! widgets/build.rs.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    embed_jj_lib_version();
}

/// Expose the jj-lib version Cargo.lock resolved as `DIFFUI_JJ_LIB_VERSION`,
/// so the app can warn when the `jj` CLI on PATH is older than the library
/// writing to the same repository. Read from the lock rather than kept as a
/// constant so a bump can't leave it stale.
fn embed_jj_lib_version() {
    let lock =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo")).join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let version = fs::read_to_string(&lock)
        .ok()
        .and_then(|text| {
            let (_, rest) = text.split_once("name = \"jj-lib\"\n")?;
            let line = rest.lines().next()?;
            Some(
                line.strip_prefix("version = \"")?
                    .strip_suffix('"')?
                    .to_owned(),
            )
        })
        .unwrap_or_default();
    println!("cargo:rustc-env=DIFFUI_JJ_LIB_VERSION={version}");
}

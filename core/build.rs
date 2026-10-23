//! Compiles the `@pierre/highlights` module (see `src/highlights`) into the
//! crate: the wasm is fetched at build time, wabt's `wasm2c` translates it to
//! C, and `cc` builds that with the wasm2c runtime and our shim. The module
//! uses wasm SIMD, which wasm2c lowers through SIMDe's portable headers.
//!
//! The version is pinned and the bytes checksummed, because the language and
//! token ids in `src/highlights/tables.rs` are that version's ABI. The fetch is
//! cached in `OUT_DIR`, and `HIGHLIGHTS_WASM_PATH` points the build at a local
//! copy instead of the network (the Nix package does this).
//!
//! Needs wabt (`wasm2c`, its `include/` and `share/wabt/wasm2c/`) and SIMDe's
//! headers. The Nix shell sets `WABT_DIR` and `SIMDE_DIR` to their prefixes;
//! elsewhere wabt is found through `wasm2c` on `PATH` and SIMDe through
//! `SIMDE_DIR` or the compiler's default include path.

use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

// Pinned to @pierre/highlights 0.2.0. Bump all three together, and regenerate
// `src/highlights/tables.rs` from that version's dist/languages.js and
// dist/token-types.js.
const HIGHLIGHTS_VERSION: &str = "0.2.0";
const HIGHLIGHTS_SHA256: &str = "cf5fc4d83e7c1ddab19d163de096f787f87efe61504909aa469434f61ef51e2e";
const HIGHLIGHTS_SIZE: u64 = 183_180;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=highlights/shim.c");
    println!("cargo:rerun-if-env-changed=HIGHLIGHTS_WASM_PATH");
    println!("cargo:rerun-if-env-changed=WABT_DIR");
    println!("cargo:rerun-if-env-changed=SIMDE_DIR");

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let wasm = out.join("highlights.wasm");
    fetch_wasm(&wasm);

    let wabt = wabt_prefix();
    let translated = out.join("highlights.c");
    let status = Command::new(wabt.join("bin/wasm2c"))
        .arg(&wasm)
        .args(["-n", "highlights", "-o"])
        .arg(&translated)
        .status()
        .unwrap_or_else(|err| panic!("running {}: {err}", wabt.join("bin/wasm2c").display()));
    assert!(status.success(), "wasm2c failed on {}", wasm.display());

    let runtime = wabt.join("share/wabt/wasm2c");
    let mut build = cc::Build::new();
    build
        .file(&translated)
        .file("highlights/shim.c")
        .file(runtime.join("wasm-rt-impl.c"))
        .file(runtime.join("wasm-rt-mem-impl.c"))
        .file(runtime.join("wasm-rt-exceptions-impl.c"))
        .include(&out)
        .include(wabt.join("include"))
        .include(&runtime)
        // Explicit bounds checks instead of guard pages, so the runtime
        // installs no SIGSEGV handler or sigaltstack into the app. Linear
        // memory still comes from mmap, grown in place and handed back to the
        // OS when an instance is dropped.
        .define("WASM_RT_MEMCHECK_BOUNDS_CHECK", "1")
        .define("WASM_RT_MEMCHECK_GUARD_PAGES", "0")
        .define("WASM_RT_USE_MMAP", "1")
        .opt_level(2)
        // Generated code: its warnings aren't ours to fix. Nor is the Nix cc
        // wrapper's complaint about cc-rs's `--target`, which would otherwise
        // surface as a cargo warning on every build.
        .warnings(false)
        .cargo_warnings(false);
    if let Some(simde) = env::var_os("SIMDE_DIR") {
        build.include(Path::new(&simde).join("include"));
    }
    build.compile("diffui_highlights");
}

/// Put the pinned module at `dest`: from `HIGHLIGHTS_WASM_PATH`, a previous
/// build's copy, or the npm package through jsDelivr.
fn fetch_wasm(dest: &Path) {
    if let Some(local) = env::var_os("HIGHLIGHTS_WASM_PATH") {
        let bytes = fs::read(&local)
            .unwrap_or_else(|e| panic!("HIGHLIGHTS_WASM_PATH={local:?} could not be read: {e}"));
        verify(&bytes);
        fs::write(dest, &bytes).expect("write highlights.wasm into OUT_DIR");
        return;
    }
    if fs::read(dest).is_ok_and(|bytes| checksum_ok(&bytes)) {
        return;
    }

    let url = format!(
        "https://cdn.jsdelivr.net/npm/@pierre/highlights@{HIGHLIGHTS_VERSION}/dist/highlights.wasm"
    );
    let response = ureq::get(&url).call().unwrap_or_else(|e| {
        panic!(
            "failed to download the highlights module from {url}: {e}\n\
             If you're offline or behind a proxy, set HIGHLIGHTS_WASM_PATH to a local highlights.wasm."
        )
    });
    let mut bytes = Vec::with_capacity(HIGHLIGHTS_SIZE as usize);
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut bytes)
        .expect("read the highlights module response body");
    verify(&bytes);
    fs::write(dest, &bytes).expect("write highlights.wasm into OUT_DIR");
}

fn checksum_ok(bytes: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    if bytes.len() as u64 != HIGHLIGHTS_SIZE {
        return false;
    }
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex == HIGHLIGHTS_SHA256
}

fn verify(bytes: &[u8]) {
    assert!(
        checksum_ok(bytes),
        "highlights module integrity check failed (expected {HIGHLIGHTS_SIZE} bytes, sha256 \
         {HIGHLIGHTS_SHA256}). The pinned version may have been republished: update \
         HIGHLIGHTS_* in core/build.rs and src/highlights/tables.rs."
    );
}

fn wabt_prefix() -> PathBuf {
    if let Some(dir) = env::var_os("WABT_DIR") {
        return dir.into();
    }
    env::var_os("PATH")
        .and_then(|path| env::split_paths(&path).find(|dir| dir.join("wasm2c").is_file()))
        .and_then(|bin| bin.parent().map(Path::to_path_buf))
        .expect("wasm2c not found: install wabt or set WABT_DIR (the Nix shell provides both)")
}

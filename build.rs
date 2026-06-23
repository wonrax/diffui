//! Fetches the Lucide icon font at build time so the 837 KB binary stays out of
//! git. The TTF lands in `OUT_DIR` and is embedded by `src/icons.rs` via
//! `include_bytes!`, so there's no runtime file dependency.
//!
//! The version is pinned (and the bytes checksummed) so the embedded font always
//! matches the codepoint constants in `src/icons.rs`: a republished or wrong
//! file fails the build loudly rather than silently shifting glyphs.
//!
//! Network at build time is a real cost (CI / offline / sandboxed builds). Two
//! escape hatches: the download is cached in `OUT_DIR` (so only clean builds
//! refetch), and `LUCIDE_TTF_PATH` points the build at a local TTF instead of
//! the network.

use std::{env, fs, io::Read, path::PathBuf};

// Pinned to lucide-static 1.21.0. Bump all three together when updating, and
// regenerate the codepoints in `src/icons.rs` from that version's font/info.json.
const LUCIDE_VERSION: &str = "1.21.0";
const LUCIDE_SHA256: &str = "ebcd4c55d702f35fab102f8c34bc18c2338e17368470ed58aac49f8fb2a5d476";
const LUCIDE_SIZE: u64 = 837_320;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LUCIDE_TTF_PATH");

    let dest =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo")).join("lucide.ttf");

    // Offline / CI / vendored builds: copy a local TTF instead of fetching.
    if let Some(local) = env::var_os("LUCIDE_TTF_PATH") {
        let bytes = fs::read(&local)
            .unwrap_or_else(|e| panic!("LUCIDE_TTF_PATH={local:?} could not be read: {e}"));
        verify(&bytes);
        fs::write(&dest, &bytes).expect("write lucide.ttf into OUT_DIR");
        return;
    }

    // Cache hit: a previous build already produced a valid font in this OUT_DIR.
    if fs::read(&dest).is_ok_and(|b| checksum_ok(&b)) {
        return;
    }

    let url =
        format!("https://cdn.jsdelivr.net/npm/lucide-static@{LUCIDE_VERSION}/font/lucide.ttf");
    let bytes = download(&url);
    verify(&bytes);
    fs::write(&dest, &bytes).expect("write lucide.ttf into OUT_DIR");
}

fn download(url: &str) -> Vec<u8> {
    let response = ureq::get(url).call().unwrap_or_else(|e| {
        panic!(
            "failed to download the Lucide font from {url}: {e}\n\
             If you're offline or behind a proxy, set LUCIDE_TTF_PATH to a local lucide.ttf."
        )
    });
    let mut bytes = Vec::with_capacity(LUCIDE_SIZE as usize);
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .expect("read the Lucide font response body");
    bytes
}

fn checksum_ok(bytes: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    if bytes.len() as u64 != LUCIDE_SIZE {
        return false;
    }
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex == LUCIDE_SHA256
}

fn verify(bytes: &[u8]) {
    assert!(
        checksum_ok(bytes),
        "Lucide font integrity check failed (expected {LUCIDE_SIZE} bytes, sha256 {LUCIDE_SHA256}). \
         The pinned version may have been republished — update LUCIDE_* in build.rs and the \
         codepoints in src/icons.rs."
    );
}

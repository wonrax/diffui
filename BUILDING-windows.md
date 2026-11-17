# Building diffui on Windows

On macOS and Linux, `nix develop` gives you everything the build needs. Windows
has no Nix, so you set up the pieces below yourself.

## Requirements

- Rust with the MSVC target, plus the MSVC linker and the Windows SDK.
- wabt, for `wasm2c`. Its Windows release links against OpenSSL but doesn't
  include `libcrypto-3-x64.dll`, so put that DLL next to `wasm2c.exe` or on
  `PATH`.
- SIMDe headers.

```powershell
$env:WABT_DIR  = "C:\path\to\wabt"    # not needed if wasm2c.exe is on PATH
$env:SIMDE_DIR = "C:\path\to\simde"   # contains include\simde\...
cargo run -- .
```

## What's missing on Windows

Credential prompts (the askpass helper) and `diffui ctl` both talk over Unix
sockets, so they're disabled on Windows. Many tests set Unix file permissions
and only compile on Unix.

## jj

In a colocated repo checked out with `core.autocrlf=true`, every text file
shows up as modified in jj, because jj stores files byte for byte by default.
Run this before jj's first snapshot:

```powershell
jj config set --repo working-copy.eol-conversion input-output
```

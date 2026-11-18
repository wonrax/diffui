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

## Repositories in WSL

Open a repository that lives inside a WSL distribution through a host, not
through its `\\wsl.localhost\…` path: jj on Windows would read every file
over the network share, see none of the changes made inside, and rewrite
line endings and executable bits. With a host, `diffui-serve` runs inside
the distribution and does jj's work there.

Build the server inside the distribution, from a checkout at the same commit
as the app:

```sh
# With Nix:
nix build .#diffui-serve          # ./result/bin/diffui-serve

# Without: a C compiler, wabt (wasm2c) and SIMDe's headers, as for the app.
cargo install --path serve        # ~/.cargo/bin/diffui-serve
```

Each distribution is offered as a host by its name (the chip in the open
dialog, or `diffui Ubuntu:~/src/repo`). diffui starts
`wsl.exe --distribution <name> --exec /bin/sh -l` and looks for
`diffui-serve` on the `PATH` that gets from `/etc/profile` and
`~/.profile`, whatever your own shell is. A `PATH` only your interactive
shell sets (`.zshrc`, a fish or nushell config) isn't seen. Say where it is
otherwise, in Settings → Hosts (the pencil on the distribution's row, Server)
or in diffui's config.toml (`%XDG_CONFIG_HOME%\diffui\config.toml`, else
`%HOME%\.config\diffui\config.toml`, else
`%USERPROFILE%\.config\diffui\config.toml`):

```toml
[hosts.Ubuntu]
server = ["/home/me/src/diffui/result/bin/diffui-serve"]
```

`diffui skill remote` has the rest: every host setting, and what a remote
tab doesn't do.

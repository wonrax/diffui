# Repositories on other machines

diffui opens a repository that lives on another machine (a server over ssh,
a WSL distribution from Windows, a container) as a tab like any other. The
repository isn't mounted or copied: a small server, `diffui-serve`, runs
where the repository is and does jj's work there, and the app talks to it
over a byte stream.

## What has to be true

- **`diffui-serve` is on that machine.** diffui never installs, uploads or
  updates anything there. Build it from a diffui checkout on that machine
  (`cargo install --path serve`, or `nix build .#diffui-serve`); on macOS and
  Linux the app's own `diffui serve` does the same job. `diffui-serve
  --version` prints the protocol it speaks. It can also be built elsewhere
  and copied over by hand: a static musl build
  (`cargo zigbuild --release -p diffui-serve --target aarch64-unknown-linux-musl`,
  with `WABT_DIR` and `SIMDE_DIR` set as for the app) runs on any Linux of
  that architecture, NixOS included.
- **Both ends speak the same protocol.** The version is a number that moves
  only when the two can no longer talk, not with every release. When they
  differ, the tab says which is which: update the older end.
- **Push and fetch run over there**, with that machine's git credentials and
  ssh agent. Nothing prompts in the app.

## Hosts

A host is a table in config.toml, or one Settings → Hosts made (saved to
settings.toml in the same shape). config.toml's keys win a key at a time, and
Settings shows them locked. After editing config.toml, `diffui ctl reload`.

```toml
[hosts.box]
transport = "ssh"                  # a transport plugin's id: "ssh" or "wsl" are bundled
destination = "me@box.example"     # the transport's own settings; ssh: destination, port, login_shell
server = ["~/.cargo/bin/diffui-serve"]
env = { JJ_CONFIG = "/home/me/.config/jj/work.toml" }

[hosts.dev]
command = ["docker", "exec", "-i", "dev", "diffui-serve"]   # instead of a transport: the whole command, run as written
```

| Key | Meaning |
|---|---|
| `transport` | The plugin that reaches the host. Its per-host settings are the table's other keys. |
| `command` | Instead of a transport: the whole command to start here, as words, ending in what runs `diffui-serve` there (`["docker", "exec", "-i", "dev", "diffui-serve"]`). It is run as written; its input and output are the connection. `{rev}` and `{version}` work in it. |
| `server` | With a transport: the server over there, as a word or a list of words. Default `diffui-serve`, found on the `PATH` a login shell gets. `{rev}` and `{version}` are the app's commit and version, so a command that fetches a build fetches the matching one: `["nix", "run", "github:OWNER/diffui/{rev}#diffui-serve", "--"]`. `{rev}` needs an app built knowing its commit (the flake's package does). |
| `env` | With a transport: environment for the server. With `command`: environment for the command itself, on this machine (`DOCKER_HOST`, `KUBECONFIG`); a variable for the far side goes in the command (`docker exec -e NAME=value …`). |

Transports also find hosts on their own: `wsl` lists the installed
distributions, `ssh` the `Host`s of `~/.ssh/config`. Such a host needs no
table. A table with its name and no `transport` adds to it, which is how a
found host gets a `server`:

```toml
[hosts.Ubuntu]
server = ["nix", "run", "/home/me/src/diffui#diffui-serve", "--"]
```

## Opening one

- In the app: the open dialog (cmd+O) shows a chip with the machine once
  there is a host; pick one and browse its folders as on this machine.
- From a terminal: `diffui box:~/src/repo`, or `diffui ctl open box:/srv/repo`
  with the app running. A target is `host:` and a path starting `/` or `~`.
- On Windows a path inside a distribution (`\\wsl.localhost\Ubuntu\home\…`)
  opens on that distribution's host.

`diffui ctl state` gives a remote tab's `host`. `--repo <path>` matches
repositories on this machine only.

A connection is the app's, not the dialog's or the tab's: one that is still
coming up (a server being fetched or built can take minutes) keeps going when
the dialog closes, and one nothing uses is kept ten minutes before it is hung
up. Settings → Hosts and the open dialog's host list show how each stands;
a host's page has Stop and Disconnect.

## What a remote tab doesn't do

The working tree isn't on this disk, so what reads or runs something in it
is off, with "The repository is on another machine" as the reason:
`[[commands]]` rows with `exec` or `agent` (a `run` row is jj and works),
plugin commands and a plugin's repository reads and events, review notes,
the external merge tool, and jj aliases in the command line's completion.
Pull request badges work: the server reads the git remotes.

## When it doesn't connect

The tab, or the open dialog's status line, says what the transport's program
said. The host's page in Settings (Details on the tab's strip opens it) has
the rest under Output: the exact command that was run and the end of what it
wrote, with a button that copies all of it. The log
(`~/Library/Logs/diffui/diffui.log`, or `~/.local/state/diffui/diffui.log`)
has the command, the exit code and the whole output of a start that failed.
To try a host by hand, run that command in a terminal: a server that starts
prints `diffui-serve` and waits. The usual failures:

- "… has no diffui-serve to run": it isn't installed there, or isn't on the
  `PATH` of a login shell. Set the host's `server` to its full path.
- "can't talk to the diffui-serve … over there": the protocols differ.
- ssh "couldn't sign in … without asking": diffui never shows a password or
  passphrase prompt. Add the key to the agent (`ssh-add`), or accept the host
  key once in a terminal.
- A lost connection puts a strip with Reconnect over the diff; the graph
  stays.

## Writing a transport

A transport is a plugin (`diffui skill plugin`) with `[provides.transport]`
that calls `transport.register`. It never carries the data: `connect` answers
with the program to start on this machine, and diffui starts it and uses its
stdin and stdout. That program has to end up running the host's server on
the other side and pass bytes through untouched. Anything that ends in a
byte stream fits, a bridge to a socket or an HTTP tunnel included.

| Method | Answers |
|---|---|
| `connect` | `{ argv, env }` for `host` (`{ name, options, server: { argv, env } }`). Required. Held to the manifest's `[permissions] exec` and `env`, like `exec`. Throw to say a setting is missing. |
| `hosts` | The machines found here without a table, each `{ name, options }`. |
| `explain` | Why the program ended before a server answered (`{ code, stderr }`), in a sentence the user can act on, or nothing. |
| `locate` | The `{ host, path }` a path on this machine really is, for paths under the manifest's `local_roots`. |

A host's own settings are the plugin's `[config.schema]` fields with
`per_host = true`; they arrive as `host.options` over their defaults. The
bundled `ssh` and `wsl` plugins are the reference: `diffui plugin eject ssh`
copies one out to read.

# Writing a plugin

A plugin is a folder, `~/.config/diffui/plugins/<id>/`, with a `plugin.toml`
and a JavaScript or TypeScript module that diffui runs in a sandbox of its
own (QuickJS, not Node).

Use a `[[commands]]` row (`diffui skill commands`) when the job is "run this
program, or this jj line, with these values filled in": it needs no code and
nothing has to be allowed. Write a plugin when the job needs logic: several
steps with decisions between them, HTTP, reading the repository, reacting to
what happens in the app, a status-bar item, an AI provider
(`diffui skill ai`), or a forge (`diffui skill forge-plugin`).

**The contract is `diffui.d.ts`.** `diffui plugin types` prints it, and
`init` writes it beside the module. Its header is the manifest reference
(every key, `[config.schema]` types, `[provides]`) and the runtime's rules;
the rest is the whole API with what each function does. Read it before you
write; this page is the map, not the contract.

## The loop

1. `diffui plugin init <id>` writes `plugin.toml`, `main.js`, `diffui.d.ts`
   and `jsconfig.json` into `~/.config/diffui/plugins/<id>`. `--ts` writes
   `main.ts` and `tsconfig.json` instead, `--forge <host>` adds a forge
   provider for that host, `--dir <dir>` writes somewhere else. What it
   writes loads as it is.
2. Edit `plugin.toml` and the module.
3. `diffui plugin check <dir>` (or `check <id>` for one in the plugins
   folder) loads the plugin without the app and prints its permissions, the
   commands, forge methods and events it registered, and what it logged. For
   a forge it names each optional method the script left out and what diffui
   does without it. Every problem is one line with its `file:line:col`
   (a manifest field, a when-clause, a syntax error, a handler the manifest
   declares and the script doesn't register), and the exit status is
   non-zero. Nothing answers repository or `ui` calls there, so handlers
   aren't run: `check` proves the plugin loads, not that it works. It
   doesn't check types either (see TypeScript below).
4. In the running app, `diffui ctl reload` reads the plugins folder again.
   The first time a plugin is seen, and whenever its manifest asks for more
   than before, diffui asks the user whether it may run: you can't answer
   for them, so tell them to look at the app. After that, saving a file in
   the plugin's folder reloads it.
5. `diffui ctl plugins` shows each plugin's state (`running`, `loading`,
   `not-allowed`, `off`, `failed` with the error and its `file:line:col`),
   what it registered and the end of its log (`--json` for all of it).
   `diffui ctl run <id>` runs one of its commands. `diffui plugin list`
   lists the plugins without the app: the bundled ones and the folder's.

diffui ships some plugins built in: the GitHub and GitLab forges (its only
support for either) and the four AI providers. They run without asking, with
the permissions they declare; `[plugins.<id>] enabled = false` in
settings.toml (or the Off toggle in Settings) turns one off. A plugin in the
plugins folder with a bundled plugin's id replaces it, and is asked about
like any other. To change a bundled plugin, `diffui plugin eject <id>`
copies it into the plugins folder with `diffui.d.ts`; edit the copy, and
delete the folder to go back to the built-in one.

## `plugin.toml`

```toml
id = "labels"                    # lowercase letters, digits, dashes; the folder's name
name = "Labels"                  # shown in Settings and as the palette heading
version = "0.1.0"
api = 0                          # the plugin API version; 0 through alpha
main = "main.js"                 # or main.ts

[permissions]                    # all optional; see below
http = ["codeberg.org"]
exec = [["git", "config"]]
jj = "read"

[[commands]]                     # the script must register a handler for each
id = "labels.sync"               # always "<plugin id>.<name>"
label = "Sync labels"
hint = "Fetch the labels again"  # optional
group = "Bookmarks"              # optional palette heading; default the plugin's name
when = "repo.kind == 'jj'"       # optional; the same when-clauses as [[commands]] rows
menu = ["palette", "revision"]   # palette | revision | bookmark | file; default palette
keys = ["cmd+shift+l"]           # optional

[config.schema]                  # optional: defaults for `config`, and Settings' controls
team = { type = "string", default = "core", label = "Team" }
```

Unlike a `[[commands]]` row in config.toml, a plugin's row has no `exec`,
`run`, `stdin`, `output`, `confirm` or prompts: the handler does all of
that in code. The user sets a plugin's config under `[plugins.<id>]` in
config.toml; Settings writes its changes to settings.toml, config.toml wins
per key, and the plugin restarts on a change.

## Permissions

A plugin can do nothing outside its sandbox that its manifest doesn't ask
for, and the user sees the list before it runs:

| Manifest | Lets the script |
|---|---|
| `http = ["host", "*.example.com", "localhost:3000"]` | `http.fetch` those hosts, and ask `auth.token` for their git credentials |
| `http = ["{forge.hosts}"]` | the same for every host the forge claims: its patterns, `hosts()`, the user's `hosts`, the hosts it has a token for |
| `http = ["{config.base_url}"]` | the host of the address the string setting `base_url` holds, so the user can point an API at a proxy or a local server |
| `exec = [["git", "config"]]` | `exec` an argv that starts with one of these prefixes; a word `"{config.program}"` is the string setting `program` (declare it in `[config.schema]`), so the user can point the plugin at a program |
| `env = ["GH_HOST"]` | set these variables for the programs `exec` runs |
| `read = ["${GH_CONFIG_DIR:-~/.config/gh}/hosts.yml"]` | `files.read` these files, named as written or by the path they expand to; `~/`, `$NAME` and `${NAME:-default}` expand when read. Read-only, and nothing else on disk |
| `jj = "read"` | read the repository: `repo.bookmarks`, `remotes`, `selection`, `rows`, `diff`, `readFile`, `repo.jj.query` |
| `jj = "write"` | all of that, and `repo.jj.run` |

`repo.root`, `repo.forgeRemote`, `kv`, `config`, `console` and `ui` need no
permission. Ask for the least that works: every addition makes diffui ask
the user again. A plugin that isn't allowed doesn't run, and its commands
are listed greyed with "Not allowed".

## The API

Everything is imported from `"diffui"`; `diffui.d.ts` has the signatures.

| Import | For |
|---|---|
| `commands` | `commands.register(id, handler)`: the handler gets `{ repo, changeId, commitId, bookmark, path, line }` |
| `forge` | `forge.register(provider)`: a forge (`diffui skill forge-plugin`) |
| `ForgeError` | `throw new ForgeError(kind, message, { host, what })` from a forge, so diffui says what to do (`gh auth login --hostname …`) |
| `ai` | `ai.register(provider)` for a plugin with `[provides.ai]`: `complete` (called with a `request`, `{ prompt, input, system, model, cwd }`, and `{ signal }`) returns the reply as text or an (async) iterable of text pieces (required), `models` (the list Settings' model picker shows), `account` (what it runs as, `{ label }`, for its page) |
| `transport` | `transport.register(provider)` for a plugin with `[provides.transport]`, which reaches repositories on other machines: see `diffui skill remote` |
| `AiError` | `throw new AiError(kind, message)` from an AI provider: diffui adds what to do ("Add an API key in Settings → AI → Anthropic") |
| `events` | `events.on(name, listener)` for `repoOpened`, `refsChanged`, `operationChanged` (the repository changed: a commit, a rebase, an undo; what a status item recomputes on), `selectionChanged`, `fetched`, `pushed`, `commandInvoked` |
| `http` | `http.fetch(url, init)`, shaped like `fetch`, to the allowed hosts |
| `exec` | `exec(argv, options)`: no shell, an allowed argv prefix, `env` for allowed variables; `exec.stream(argv, options)` for output read a piece at a time with `for await`, then `await out.ended` for the exit |
| `files` | `files.read(path)`: the text of a file `[permissions] read` lists, such as a CLI's own config |
| `auth` | `auth.token(host, { git })`: a token variable, the stored token, the user's `token_cmd`, else `git credential fill` (not with `git: false`); never prompts. `auth.secret(key, { host })`, `auth.hosts()` |
| `kv` | a small JSON store that survives restarts |
| `config` | the plugin's settings |
| `ui` | `ui.toast`, `ui.activity`, `ui.prompt.input` / `confirm` / `choose`, `ui.openUrl`, `ui.clipboard.write`, `ui.status.set` |

Every handler is handed a `repo`. It is pinned to the tab and the jj
operation that were on screen when the handler was called: once the
repository has moved on, or while another tab is in front, its methods
reject with an error named `StaleError`. Catch it and stop; the next event
brings a fresh handle. `repo.jj.query(argv)` runs a read-only jj command at
that operation (the d.ts lists which); `repo.jj.run(argv)` runs any jj
command the way `diffui ctl jj` does.

## TypeScript

diffui erases a `.ts` module's types and runs what is left. It doesn't
check them, and neither does `diffui plugin check`: a plugin with type
errors loads. Check them yourself in the plugin's folder with the config
`init` wrote:

```sh
npx -p typescript tsc --noEmit                    # main.ts, with tsconfig.json
npx -p typescript tsc --noEmit -p jsconfig.json   # main.js with // @ts-check
```

Both find the API's types in the `diffui.d.ts` beside the module, which is
how `import … from "diffui"` resolves; keep that file there.

## Pitfalls

- There is no Node here: no `require`, `fs`, `process`, `Buffer` or npm
  packages, and no global `fetch`. Import from `"diffui"` and from your own
  files by relative path (`"./util.js"`); anything else fails the load with
  a message that lists what exists.
- `http.fetch` reaches only the hosts under `[permissions] http`, redirects
  included. A new host is a manifest change, which the user is asked about.
- Registration happens while the main module loads, and must match the
  manifest: a `[[commands]]` row without `commands.register`, or
  `[provides] forge` without `forge.register`, fails the load.
- A handler that runs for more than half a second without awaiting, or a
  heap past 64 MB, stops the plugin until it is reloaded. Await your I/O.
- TypeScript is erased, not compiled: no `enum`, `namespace` or constructor
  parameter properties, and types are imported with `import type`.
- `ui.status.set` is one item per plugin for the whole window, not one per
  repository: it doesn't change when the user switches tabs.
- Plugins never draw. What they can show is in `ui`.
- `exec` and `http.fetch` hold their whole output (up to 8 MB). For
  anything bigger, a diff above all, use `exec.stream` or `stream: true`
  and hand pieces on as they come.
- `repo.jj.run` resolves when diffui has accepted the command, not when it
  has finished, and the handle is stale once the repository changed: one
  command per handler, or carry on from the next event.

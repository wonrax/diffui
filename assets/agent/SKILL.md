---
name: diffui
description: Drive the diffui app (a GUI for jj and git) from the terminal with `diffui ctl`, add commands to it with `[[commands]]` rows in its config.toml, and write plugins for it with `diffui plugin`. Use when the user runs diffui and wants you to show them something in it, run a jj command where they can watch it, teach it a new command or shortcut, or support a forge it doesn't know.
---

# diffui

diffui is a desktop app for reviewing and rewriting history in jj (and git)
repositories. You can drive the running app with `diffui ctl`, add commands
to it by writing `[[commands]]` rows in its `config.toml`, and give it code
of your own as a plugin (see "Writing a plugin").

## `diffui ctl`

`diffui ctl` talks to the running app over a private unix socket. It fails
with "diffui isn't running" when no app is open; ask the user to start it.
Every verb takes `--json` (print the app's answer as one JSON object) and
`--repo <path>` (act on the open tab whose repository holds that path, and
bring it forward; otherwise the active tab).

| Verb | Does |
|---|---|
| `diffui ctl state` | Open repositories (tab, root, kind `jj`/`git`/`pr`, which is active), the selected revision (change id, commit id, first line, bookmarks, whether it's `@`), the log's revset, and the active tab's ten latest activity rows (label, status, result, equivalent command). |
| `diffui ctl open <path>` | Open a repository, or a pull request (`https://github.com/o/r/pull/12`, `o/r#12`, a GitLab `https://host/group/name/-/merge_requests/7` or `group/name!7`), as a tab. Answers with `state`. |
| `diffui ctl select <revset>` | Show the revision `<revset>` names, as `jj show` would. |
| `diffui ctl commands` | Every command id, its label, whether it was declared in config (`declared`), and whether it can run now (`available`, with `reason` when not). |
| `diffui ctl run <id>` | Run a command by id, as its key would. Fails with the reason when it can't run now. |
| `diffui ctl jj <args…>` | Run a jj command line in the app, exactly as typing it in the palette would, except that a rebase or squash runs as written instead of waiting for a pick. A mutation gets the app's activity row, its queue and its confirm for immutable revisions; a view (`log`, `diff`, `show`, `evolog`, `op log`, `file annotate`) opens it; `describe`, `commit`, `split` and `resolve` open their editor; anything else runs as a jj subprocess whose output streams to an activity row. Answers once the line has planned, with what it does (`did`), or why it won't run. Watch `state`'s activity for the outcome. |
| `diffui ctl toast <text>` | Show a toast. |
| `diffui ctl reload` | Read `config.toml`'s `[keys]` and `[[commands]]` and the plugins folder again, and start every plugin afresh. Answers with what was wrong (`problems`, as `path:line:col: message`). |
| `diffui ctl plugins` | Every plugin found: its id, folder, state (`running`, `loading`, `not-allowed`, `off`, `failed` with the `error`), what it registered and the end of its log. |

Prefer `diffui ctl jj …` over running `jj` yourself when the user is
watching the app: the change shows in its activity and can be undone there.

## Pull requests

When a remote is on GitHub or GitLab, diffui shows each bookmark's pull
request (merge request on GitLab) and its checks, and can open and update
them through the `gh` or `glab` CLI the user is signed in to. These are
ordinary commands, so `diffui ctl run <id>` runs them on the selected
revision (select one first with `diffui ctl select`):

| Id | Does |
|---|---|
| `forge.create` | Open the create dialog for the revision's bookmark (or a generated one), pushing first. |
| `forge.update` | Open the dialog to change an open pull request's base, title or description. |
| `forge.submit-stack` | Push every bookmark from trunk to the revision and open or update a pull request for each, each based on the one below. |
| `forge.open` | Open the pull request in the browser. |
| `forge.open-tab` | Open the pull request as a tab. |
| `forge.copy-url` | Copy the pull request's URL. |
| `forge.refresh` | Ask the forge again for every bookmark's pull request and checks. |

The dialog waits for the user; `forge.submit-stack` doesn't. Watch `state`'s
activity for the outcome.

## Declaring commands

`config.toml` lives at `$XDG_CONFIG_HOME/diffui/config.toml`, else
`~/.config/diffui/config.toml`. The app never writes it. After editing it,
run `diffui ctl reload` and fix every problem it prints.

```toml
[[commands]]
id = "user.open-in-zed"          # letters, digits, . - _ ; unique
label = "Open in Zed"            # what the palette and menus show
hint = "Open the file in Zed"    # optional, the palette's second line
group = "Changes"                # optional palette heading, default "Commands"
when = "file"                    # optional when-clause, see below
menu = ["file", "palette"]       # where it's offered, default ["palette"]
keys = ["cmd+shift+e"]           # optional chords, bound when no dialog is open
exec = ["zed", "{root}/{path}:{line}"]
```

`group` is one of Repository, Revisions, Bookmarks, Changes, Conflicts,
Compare, View, Appearance, App, Commands.

A row has exactly one of these four:

- `exec = ["program", "arg", …]`: run a program. No shell is involved: each
  element is one argument, and a variable's value is never split, quoted or
  expanded again. The program runs in the repository's root with the user's
  login `PATH`.
- `run = "jj …"`: a jj command line, run like `diffui ctl jj`. It's split
  into words first, the way a shell would split it, then each word's
  variables are filled in, so a value is always one argument. Needs a jj
  repository.
- `agent = "<prompt>"`: run the user's agent CLI with the prompt as its last
  argument: `[ai] command` from config.toml, else `claude -p`, else
  `codex exec`.
- `edit = "<path>"`: open a path (relative to the root) in the user's editor
  at `{line}`: `[editor] command`, else `$VISUAL` / `$EDITOR` when it's a GUI
  editor (code, cursor, zed, subl, idea, mate, …), else the system opener.
  Only files in the working copy exist on disk; the command is off for others.

Optional fields:

| Field | Meaning |
|---|---|
| `stdin = "<template>"` | Text fed to the program's standard input (`exec` and `agent` only). |
| `output = "…"` | Where the program's standard output goes (`exec` and `agent` only): `activity` (default; the activity row and its output panel), `toast` (its first line once it exits), `clipboard`, `description` (written into the description editor as it arrives, opening it on the revision; undoable), `open-url` (its first line, an `http(s)` link, opened in the browser) or `none`. |
| `confirm = "<template>"` | Ask before running; the text is the question. Can't use `{diff}` or `{recent_descriptions}`. |
| `override = true` | Replace the built-in or shipped command with the same `id`. Without it, a row taking an existing id is left out. |
| `timeout = 300` | Seconds the program may run before it's stopped (default 300). |

A non-zero exit or a missing program shows a toast and an error on the
activity row with the program's last error line. The output panel's Stop
cancels a running program.

### Prompts

Each `[[commands.prompts]]` table under a command asks one question before it
runs, in order. The answer is `{prompt.<name>}`.

```toml
[[commands]]
id = "user.rebase-onto"
label = "Rebase onto a release branch"
when = "repo.mutate && revision.mutable"
menu = ["revision", "palette"]
run = "jj rebase -r {change_id} -o {prompt.target}"
confirm = "Rebase {change_id}?"

[[commands.prompts]]
name = "target"                   # letters, digits, - _
kind = "choose"                   # input (default) | confirm | choose
label = "Rebase onto"
choices = ["main", "release/1.0"] # choose only
```

`input` asks for a line of text, `confirm` asks yes or no (no stops the run;
yes is `true`), `choose` offers its `choices` in the palette.

### Template variables

Write `{name}` in any template; `{{` and `}}` are literal braces. A command
that names a variable with no value in the current context is listed greyed
with the reason.

| Variable | Value |
|---|---|
| `{root}` | the repository's root directory |
| `{change_id}` | the revision's full change id |
| `{commit_id}` | the revision's full commit id |
| `{description}` | the revision's description |
| `{bookmark}` | the bookmark in context: the one picked, else the revision's first |
| `{path}` | the file in context, relative to the root: the one picked, else the diff's selected file |
| `{line}` | a line in that file: line-select mode's cursor, else its first changed line, else 1 |
| `{diff}` | the revision's diff in git format, read when the command runs (use it in `stdin`; as an argument it must be under 64 KiB) |
| `{recent_descriptions}` | the ten latest non-empty descriptions below the working copy, each followed by a line of `---` |
| `{prompt.<name>}` | a prompt's answer |

"The revision" is the one the command was picked for (a menu row, a palette
result), else the selected one.

### When-clauses

`when` decides where a command applies. It's a small expression over these
keys: `&&`, `||`, `!`, parentheses, `==`, `!=`, `>=`, `<=`, `>`, `<`, strings
in single or double quotes, numbers, `true` and `false`. `&&` binds tighter
than `||`. A key alone is true when its value is `true`, a non-zero number or
a non-empty string. `==` and `!=` compare like values; the ordering operators
compare numbers. Comparing different kinds of value is false (and `!=` true);
a key that doesn't exist reads as false and `reload` reports it.

| Key | Value |
|---|---|
| `repo.kind` | `"jj"`, `"git"` or `"pr"` (a pull-request tab) |
| `repo.mutate` | the repository can be rewritten (jj) |
| `repo.forge` | `"github"`, `"gitlab"` or a forge plugin's id: the forge one of the repository's remotes is on |
| `revision.selected` | a revision is in context and in the log |
| `revision.mutable` | the revision in context isn't immutable |
| `revision.is_working_copy` | the revision in context is `@` |
| `selection.count` | how many rows are marked in the log |
| `bookmark` | a bookmark is in context: the one picked, else the revision's first |
| `bookmark.has_remote` | the bookmark in context exists on a remote |
| `bookmark.pr` | the state of the bookmark in context's pull request: `"open"`, `"draft"`, `"merged"` or `"closed"` |
| `file` | a file is in context: the one picked, else the diff's selected file |
| `diff.line` | a diff line is in context (line-select mode has a cursor) |

Examples: `repo.kind == "jj" && revision.mutable`, `selection.count >= 2`,
`bookmark && !bookmark.has_remote`, `bookmark.pr == "open"`.

In menus, a command whose when-clause doesn't hold isn't shown; the palette
lists it greyed with the clause.

### Menus and keys

`menu` takes `palette` (the palette's top level), `revision` (the revision
context menu and a revision's actions in the palette), `bookmark` (the
revision menu of a row with a bookmark, and a bookmark's actions in the
palette) and `file` (the file context menu and a file's actions in the
palette).

`keys` take chords like `cmd+shift+e`, `alt+r`, `ctrl+enter`, `f5`: modifiers
`cmd` (⌘ on macOS, Ctrl elsewhere), `ctrl`, `alt`, `shift`, then one
character or a key name (`enter`, `esc`, `tab`, `space`, `backspace`,
`delete`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`,
`pagedown`, `f1` to `f12`). A declared key wins over a built-in one it
collides with; `reload` says so. `[keys]` rebinds any command by id, and
keeps an id it doesn't know yet for when a reload declares it:

```toml
[keys]
"user.open-in-zed" = ["cmd+shift+e"]

[keys.base]
"file.next" = "n"
```

### Shipped commands

diffui ships two commands written this way. Replace either with a row of the
same id and `override = true`.

- `file.open-in-editor`: `edit = "{path}"`, offered in the file menu and the
  palette.
- `ai.describe` ("Generate description", also the description editor's
  Generate button): `agent` with a prompt that includes
  `{recent_descriptions}`, `stdin = "{diff}"` and `output = "description"`.

```toml
[ai]
command = ["claude", "-p"]   # the agent CLI; the prompt is added last

[editor]
command = ["zed"]            # the path and line are added the way it takes them
```

## Writing a plugin

A plugin is a folder, `~/.config/diffui/plugins/<id>/`, with a `plugin.toml`
and a JavaScript or TypeScript module that diffui runs in a sandbox of its
own (QuickJS, not Node).

Use a `[[commands]]` row when the job is "run this program, or this jj
line, with these values filled in": it needs no code and nothing has to be
allowed. Write a plugin when the job needs logic: several steps with
decisions between them, HTTP, reading the repository, reacting to what
happens in the app, a status-bar item, or a forge (pull-request badges and
dialogs for a host diffui doesn't know).

### The loop

1. `diffui plugin init <id>` writes `plugin.toml`, `main.js`, `diffui.d.ts`
   and `jsconfig.json` into `~/.config/diffui/plugins/<id>`. `--ts` writes
   `main.ts` and `tsconfig.json` instead, `--forge <host>` adds a forge
   provider for that host, `--dir <dir>` writes somewhere else. What it
   writes loads as it is.
2. Edit `plugin.toml` and the module. `diffui.d.ts` beside them is the whole
   API with its types and what each function does; read it before you write.
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
   lists the folder without the app.

### `plugin.toml`

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

[provides]                       # optional: this plugin is a forge
forge = { hosts = ["codeberg.org"], term = "pull request" }

[[commands]]                     # the script must register a handler for each
id = "labels.sync"               # always "<plugin id>.<name>"
label = "Sync labels"
hint = "Fetch the labels again"  # optional
when = "repo.kind == 'jj'"       # optional; the same when-clauses as above
menu = ["palette", "revision"]   # palette | revision | bookmark | file; default palette
keys = ["cmd+shift+l"]           # optional

[config.schema]                  # optional: defaults for `config`
team = { type = "string", default = "core", label = "Team" }
```

Unlike a `[[commands]]` row in config.toml, a plugin's row has no `exec`,
`run`, `stdin`, `output`, `confirm` or prompts: the handler does all of
that in code. `term` is `"pull request"` (the default) or `"merge request"`,
whichever the forge says. The user sets a plugin's config under
`[plugins.<id>]` in config.toml; `token_cmd` there is an argv that prints
the token `auth.token` returns.

### Permissions

A plugin can do nothing outside its sandbox that its manifest doesn't ask
for, and the user sees the list before it runs:

| Manifest | Lets the script |
|---|---|
| `http = ["host", "*.example.com", "localhost:3000"]` | `http.fetch` those hosts, and ask `auth.token` for their git credentials |
| `exec = [["git", "config"]]` | `exec` an argv that starts with one of these prefixes |
| `jj = "read"` | read the repository: `repo.bookmarks`, `remotes`, `selection`, `rows`, `diff`, `readFile`, `repo.jj.query` |
| `jj = "write"` | all of that, and `repo.jj.run` |

`repo.root`, `repo.forgeRemote`, `kv`, `config`, `console` and `ui` need no
permission. Ask for the least that works: every addition makes diffui ask
the user again. A plugin that isn't allowed doesn't run, and its commands
are listed greyed with "Not allowed".

A forge names hosts in two places, and they are separate lists.
`[provides] forge.hosts` is what the plugin claims: a repository with a
remote on one of those hosts gets this forge. `[permissions] http` is what
the script may fetch, and `auth.token(host)` only answers for a host that
is in `http`. A forge host doesn't have to be in `http`. When the API lives
on another host (`api.bitbucket.org` for remotes on `bitbucket.org`), put
the API's host in `http`; if the plugin also wants the token git keeps for
the remote, which is stored under the remote's host, put that host in
`http` as well and ask `auth.token(repo.forgeRemote.host)`:

```toml
[permissions]
http = ["api.bitbucket.org", "bitbucket.org"]   # fetch the first, token for the second

[provides]
forge = { hosts = ["bitbucket.org"] }
```

### The API

Everything is imported from `"diffui"`; `diffui.d.ts` has the signatures.

| Import | For |
|---|---|
| `commands` | `commands.register(id, handler)`: the handler gets `{ repo, changeId, commitId, bookmark, path, line }` |
| `forge` | `forge.register(provider)`, a provider being `pullRequests`, `checks`, `createPullRequest` and `updatePullRequest`; only `pullRequests` is required, and diffui hides what a missing method would serve |
| `events` | `events.on(name, listener)` for `repoOpened`, `refsChanged`, `operationChanged` (the repository changed: a commit, a rebase, an undo; what a status item recomputes on), `selectionChanged`, `fetched`, `pushed`, `commandInvoked` |
| `http` | `http.fetch(url, init)`, shaped like `fetch`, to the allowed hosts |
| `exec` | `exec(argv, options)`: no shell, an allowed argv prefix |
| `auth` | `auth.token(host)`: the user's `token_cmd`, else `git credential fill`; never prompts |
| `kv` | a small JSON store that survives restarts |
| `config` | the plugin's settings |
| `ui` | `ui.toast`, `ui.activity`, `ui.prompt.input` / `confirm` / `choose`, `ui.openUrl`, `ui.clipboard.write`, `ui.status.set` |

Every handler is handed a `repo`. It is pinned to the tab and the jj
operation that were on screen when the handler was called: once the
repository has moved on, or while another tab is in front, its methods
reject with an error named `StaleError`. Catch it and stop; the next event
brings a fresh handle.

`repo.jj.query(argv)` runs a read-only jj command at that operation and
resolves with its stdout. The argv starts with one of `log`, `show`,
`diff`, `interdiff`, `evolog`, `status`, `file show`, `file list`,
`file annotate`, `bookmark list`, `tag list`, `op log`, `op show`,
`config get`, `git remote list`, `workspace list`, and can't carry `-R`,
`--at-op`, `--ignore-working-copy`, `--tool` or `--config`.
`repo.jj.run(argv)` runs any jj command the way `diffui ctl jj` does.

### TypeScript

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

### The Forgejo plugin, in full

The reference plugin: Forgejo and Gitea pull requests. `plugin.toml`:

```toml
# Forgejo and Gitea pull requests for diffui: badges on bookmarks, checks,
# create and update. The reference plugin: to use it, run
# `diffui plugin init forgejo` and replace the plugin.toml and main.js it
# writes with these two. The diffui.d.ts it writes is the one main.js names.
#
# For a self-hosted instance, add its host to both lists below. The token
# comes from `git credential fill`, or from `token_cmd` under
# [plugins.forgejo] in config.toml.
id = "forgejo"
name = "Forgejo"
version = "0.1.0"
api = 0
main = "main.js"

[permissions]
http = ["codeberg.org"]

[provides]
forge = { hosts = ["codeberg.org"] }

[[commands]]
id = "forgejo.open-in-browser"
label = "Open pull request in browser"
when = "bookmark.pr"
menu = ["bookmark", "palette"]
```

`main.js`:

```js
// @ts-check
/// <reference path="./diffui.d.ts" />
import { auth, commands, forge, http, ui } from "diffui";

/**
 * Ask the Forgejo API about the repository's project.
 *
 * @param {import("diffui").Repo} repo
 * @param {string} path Below `/api/v1/repos/<owner>/<name>`.
 * @param {import("diffui").RequestInit} [init]
 * @returns {Promise<any>}
 */
async function api(repo, path, init = {}) {
  if (!repo.forgeRemote) throw new Error("This repository has no Forgejo remote");
  const { host, owner, name, webUrl } = repo.forgeRemote;
  const res = await http.fetch(`${new URL(webUrl).origin}/api/v1/repos/${owner}/${name}${path}`, {
    ...init,
    headers: {
      accept: "application/json",
      authorization: `token ${await auth.token(host)}`,
      "content-type": "application/json",
    },
  });
  if (!res.ok) throw new Error(`Forgejo ${res.status}: ${await said(res)}`);
  return res.json();
}

/**
 * What a failed response says: the API's `message`, else its text.
 *
 * @param {import("diffui").Response} res
 */
async function said(res) {
  const text = await res.text();
  try {
    return JSON.parse(text).message ?? text;
  } catch {
    return text.trim().slice(0, 200) || res.statusText;
  }
}

/**
 * @param {any} pr A pull request as the API returns it.
 * @returns {import("diffui").PullRequest}
 */
const toPullRequest = (pr) => ({
  number: pr.number,
  url: pr.html_url,
  title: pr.title,
  head: pr.head.ref,
  base: pr.base.ref,
  headSha: pr.head.sha,
  state: pr.merged ? "merged" : pr.state === "closed" ? "closed" : pr.draft ? "draft" : "open",
});

/** @type {Record<string, import("diffui").Checks["state"]>} */
const CHECK_STATES = { success: "success", warning: "success", pending: "pending", failure: "failure", error: "failure" };

/** @type {import("diffui").ForgeProvider} */
const provider = {
  async pullRequests(repo, { branches }) {
    const recent = await api(repo, "/pulls?state=all&sort=recentupdate&limit=50");
    return recent
      .filter((/** @type {any} */ pr) => pr.head.repo_id === pr.base.repo_id && branches.includes(pr.head.ref))
      .map(toPullRequest);
  },

  async checks(repo, sha) {
    const combined = await api(repo, `/commits/${sha}/status`);
    return {
      state: combined.total_count ? (CHECK_STATES[combined.state] ?? "none") : "none",
      url: combined.statuses?.[0]?.target_url ?? null,
    };
  },

  async createPullRequest(repo, { head, base, title, body, draft }) {
    // Forgejo has no draft flag; a "WIP:" title is how it marks one.
    const request = { head, base, title: draft ? `WIP: ${title}` : title, body };
    return toPullRequest(await api(repo, "/pulls", { method: "POST", body: JSON.stringify(request) }));
  },

  async updatePullRequest(repo, number, patch) {
    return toPullRequest(await api(repo, `/pulls/${number}`, { method: "PATCH", body: JSON.stringify(patch) }));
  },
};

forge.register(provider);

commands.register("forgejo.open-in-browser", async ({ repo, bookmark }) => {
  if (!bookmark) return;
  const [pullRequest] = await provider.pullRequests(repo, { branches: [bookmark] });
  if (pullRequest) {
    await ui.openUrl(pullRequest.url);
  } else {
    await ui.toast(`No pull request for ${bookmark}`);
  }
});
```

### Pitfalls

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
- TypeScript is erased, not compiled and not checked: no `enum`,
  `namespace` or constructor parameter properties, types are imported with
  `import type`, and a type error doesn't stop the plugin loading.
- `ui.status.set` is one item per plugin for the whole window, not one per
  repository: it doesn't change when the user switches tabs.
- Plugins never draw. What they can show is in `ui`.
- `repo.jj.run` resolves when diffui has accepted the command, not when it
  has finished, and the handle is stale once the repository changed: one
  command per handler, or carry on from the next event.

## Checking your work

- `diffui ctl reload` lists every problem with a `path:line:col`.
- `diffui ctl commands --json` shows your command, `declared: true`, and
  whether it can run on what's selected now (with the reason when not).
- `diffui ctl run <id>` runs it the way the user would.

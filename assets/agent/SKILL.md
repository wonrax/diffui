---
name: diffui
description: Drive the diffui app (a GUI for jj and git) from the terminal with `diffui ctl`, leave review notes on your changes and read the user's replies with `diffui notes`, add commands to it with `[[commands]]` rows in its config.toml, and write plugins for it with `diffui plugin`. Use when the user reviews your jj changes in diffui, when the user runs diffui and wants you to show them something in it, run a jj command where they can watch it, teach it a new command or shortcut, or support a forge it doesn't know.
---

# diffui

diffui is a desktop app for reviewing and rewriting history in jj (and git)
repositories. You can leave the user review notes on your changes with
`diffui notes` (see "Review notes"), drive the running app with `diffui ctl`, add commands
to it by writing `[[commands]]` rows in its `config.toml`, and give it code
of your own as a plugin (see "Writing a plugin").

## Review notes

When you change code in a jj repository the user reviews in diffui, leave
notes on your changes: the user reads them inline in the diff, beside the
lines they're about, and replies where something should change. `diffui
notes` works whether diffui is open or not; an open app shows new notes and
replies at once.

**What deserves a note.** What the user can't see in the diff: a design
decision and the alternative you didn't take, a trade-off, something you're
unsure of, a spot that needs their eyes (a lock order, a migration, a
behaviour change), a follow-up you left out on purpose. Not a narration of
the diff: "added a function" is noise. A few good notes beat many. A note
the user doesn't reply to is fine; there is nothing to approve.

**Where.** A note is on a change (`-r`, default `@`), and optionally on one
of its files (`--file`), and optionally on lines of it as the change has them
(`--line 40` or `--line 40-52`; add `--old` for lines the change removed).
Notes follow the change through amends and rebases: they're anchored to its
change id and the text of the lines, not to a commit. When those lines are
rewritten, the note stays on its file, marked outdated.

**The user writes notes too.** Reviewing, the user starts notes of their
own on lines, files or the change: feedback for you. They wait on you like
a reply does (status `replied`, the first message's role `user`), so `wait`
and `list --waiting` return them. Treat each as a request: fix it, then
reply under it. The user may also paste notes into your chat as Markdown
(what `diffui notes export` prints); the ids aren't in the paste, so find
them with `list --waiting` before replying.

**The loop.**

1. Finish the change, then leave notes:
   `diffui notes add --as <you> --file src/lib.rs --line 40-52 "Took the lock before the read so a writer can't slip in between; costs one extra lock per call."`
   Use `-` as the text to read Markdown from stdin (a heredoc). Pass the same
   `--as` name every time (your tool's name, or a session name when several
   of you work in one repository) so the user can tell you apart.
2. Wait for replies, in the background while the user reviews:
   `diffui notes wait` sleeps until the user replies to one of your notes,
   then prints the notes waiting on you and exits 0; it returns at once when
   some already are. It exits 124 after `--timeout` (default `30m`; `0`
   waits forever). Add `-r <revset>` to wait on some changes only, `--new` to
   ignore replies already waiting, `--json` for the threads as JSON.
3. For each reply, change the code (amend the change; the notes follow),
   then answer under the note: `diffui notes reply <id> --as <you> "Done:
   switched to an IndexMap."`, with `--resolve` when nothing is left to do.
   Reply without resolving when you have a question back. `diffui notes
   resolve <id>…` resolves without a word.
4. Wait again until the user is done.

| Verb | Does |
|---|---|
| `diffui notes add [-r REV] [--file PATH [--line N[-M]] [--old]] --as NAME TEXT` | Leave a note; prints it with its id. |
| `diffui notes reply ID --as NAME [--resolve] TEXT` | Answer under a note. |
| `diffui notes resolve ID…` / `diffui notes reopen ID…` | Resolve, or open again. |
| `diffui notes list [-r REVSET] [--waiting] [--resolved] [--by NAME]` | Notes grouped by change, newest activity first: open ones, or with `--resolved` all; `--waiting` only those the user wrote in last (replied to, or started). |
| `diffui notes show ID` | One note and its replies. |
| `diffui notes wait [-r REVSET] [--timeout 30m] [--new]` | Block until the user replies (see above). |
| `diffui notes export [ID…] [-r REVSET] [--resolved]` | Notes as Markdown (`--format md`, the default and only one): each change's id and title, each note's `path:lines`, the lines in a fence, and every message. What the app's Copy writes. |
| `diffui notes delete ID…` | Delete notes. |
| `diffui notes prune [--resolved] [--archived] [-r REVSET] [--dry-run]` | Delete in bulk: resolved notes, archived ones (their change was abandoned), or every note on some changes. |

Every verb takes `--repo <path>` (default: the current directory) and
`--json`. In the output a note's status is `open` (no reply), `replied` (the
user wrote last, a reply or a note of theirs: your turn) or `resolved`; `outdated` means its lines are gone
from the change, `archived` that the change was abandoned. The user's
messages have the role `user`.

## `diffui ctl`

`diffui ctl` talks to the running app over a private unix socket. It fails
with "diffui isn't running" when no app is open; ask the user to start it.
Every verb takes `--json` (print the app's answer as one JSON object) and
`--repo <path>` (act on the open tab whose repository holds that path, and
bring it forward; otherwise the active tab).

| Verb | Does |
|---|---|
| `diffui ctl state` | Open tabs (tab, root, name, kind `jj`/`git`/`pr`/`patch`/`compare`, which is active), the selected revision (change id, commit id, first line, bookmarks, whether it's `@`), the log's revset, and the active tab's ten latest activity rows (label, status, result, equivalent command). |
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

## Patches and comparisons

diffui also shows a diff that has no repository behind it. With the app
running, each of these opens as a tab there; without it, the app starts.

- `diffui -` reads a diff from stdin: `git diff | diffui -`.
- `diffui fix.patch` opens a patch file: any unified diff (`git diff`,
  `diff -u`, `diff -ruN`, svn, hg), or `git format-patch` emails, one tab
  per file with each email of a series as a step. A path is a patch when it
  ends in `.patch`, `.diff`, `.mbox` or `.eml`, or when it's a file no
  repository holds and it reads as a diff; any other path opens its
  repository, as before.
- `diffui diff <left> <right>` compares two files or two folders, and waits
  until its tab closes. That makes it a diff tool for jj and git, which
  delete the folders they compare once the tool exits.

jj (`~/.config/jj/config.toml`), then `jj diff --tool diffui`:

```toml
[merge-tools.diffui]
program = "diffui"
diff-args = ["diff", "$left", "$right"]
diff-invocation-mode = "dir"
```

git (`~/.gitconfig`), then `git difftool --dir-diff --tool diffui`:

```ini
[difftool "diffui"]
  cmd = diffui diff "$LOCAL" "$REMOTE"
```

The user can leave notes on these tabs too, but they live in the tab
only: `diffui notes` doesn't see them, and they're gone when the tab
closes. Ask the user to paste them (Copy) if you need them.

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
| `forge.submit-stack` | Preview a pull request for every bookmark from trunk to the revision, each based on the one below, then push and open or update them once the user confirms. |
| `forge.open` | Open the pull request in the browser. |
| `forge.open-tab` | Open the pull request as a tab. |
| `forge.copy-url` | Copy the pull request's URL. |
| `forge.refresh` | Ask the forge again for every bookmark's pull request and checks. |

Both wait for the user: nothing is pushed or sent to the forge until they
confirm (Submit stack only reads the stack and asks the forge which pull
requests exist). Watch `state`'s activity for the outcome.

GitHub and GitLab are bundled plugins (`github`, `gitlab`; see Plugins),
not built into the app: turning one off leaves that forge without badges or
pull request tabs, `diffui plugin eject <id>` copies one out to change, and
a folder plugin with the same id replaces it. `[plugins.github] program =
"/path/to/gh"` in config.toml points one at another CLI.

Each signs in to a host through its CLI (`gh`, `glab`) or with a personal
access token. `auth = "auto"` (the default) uses the CLI where it is signed
in to the host, else a token, else says how to sign in; `"cli"` or
`"token"` forces one, for every host or per host under
`[plugins.github.per_host."github.acme.com"]`. Tokens live in the macOS
Keychain (service `diffui`, account `github@<host>`), never in a config
file; `GH_TOKEN` / `GH_ENTERPRISE_TOKEN` / `GITLAB_TOKEN` are read the way
the CLIs read them. Settings → Plugins → the plugin's pencil has an
Account row per host. For scripts and agents:

```sh
diffui plugin auth github --status                     # how github.com signs in, as whom
printf %s "$TOKEN" | diffui plugin auth github --host github.acme.com --token-stdin
diffui plugin auth gitlab --host gitlab.acme.org --sign-out
```

`--token-stdin` keeps the token only once the forge says whose it is
(`GET /user`). GitHub needs the `repo` scope (fine-grained: metadata,
contents, commit statuses and checks read; pull requests read and write),
GitLab `api`.

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
- `agent = "<prompt>"`: send the prompt (and `stdin`, which a provider gets
  as what the prompt is about) to the AI provider: the one Settings → AI (or
  `[ai] provider`) picks, see "AI providers" below.
- `edit = "<path>"`: open a path (relative to the root) in the user's editor
  at `{line}`: `[editor] command`, else `$VISUAL` / `$EDITOR` when it's a GUI
  editor (code, cursor, zed, subl, idea, mate, …), else the system opener.
  Only files in the working copy exist on disk; the command is off for others.

Optional fields:

| Field | Meaning |
|---|---|
| `stdin = "<template>"` | Text fed to the program's standard input (`exec` and `agent` only). |
| `output = "…"` | Where the program's standard output (an agent's reply) goes (`exec` and `agent` only): `activity` (default; the activity row and its output panel), `toast` (its first line once it exits), `clipboard`, `description` (written into the description editor as it arrives, opening it on the revision; undoable), `pr` (the pull request dialog's title, the first line, and body, as it arrives, opening the dialog for the revision), `bookmark` (the bookmark name prompt, its first line made a name such as `fix-parser-crash`, opening the prompt at the revision), `panel` (the output panel, opened as it starts, for text to read: rendered as Markdown, with a Rendered | Raw toggle), `notes` (the panel as it streams, then each list item that starts with a location such as `` `src/lib.rs:40-52` `` filed as a review note on that line of the revision, from the provider; the panel stays when no item cites a line), `open-url` (its first line, an `http(s)` link, opened in the browser) or `none`. A failure puts back what a field held. |
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
| `{stack_diff}` | the diff of `trunk()..` the revision, its stack, in git format (jj only) |
| `{stack_descriptions}` | the descriptions of the stack's revisions, oldest first, each followed by a line of `---` (jj only) |

`{diff}` and `{stack_diff}` stop at 200 KB: the rest is left out with a line
that says so, and diffui tells the user once the command has run (the
output panel says it first).
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
| `ai` | an AI provider is picked (or found automatically) and set up; the shipped AI rows name it |
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

diffui ships these commands written this way. Replace any with a row of the
same id and `override = true`.

- `file.open-in-editor`: `edit = "{path}"`, offered in the file menu and the
  palette.
- `ai.describe` ("Generate description", also the description editor's
  Generate button): `agent` with a prompt that includes
  `{recent_descriptions}`, `stdin = "{diff}"` and `output = "description"`.
- `ai.pr-description` ("Generate pull request title and description", also
  the pull request dialog's Generate button): `stdin` the stack's
  descriptions and diff, `output = "pr"`.
- `ai.bookmark-name` ("Suggest bookmark name", also the new bookmark
  prompt's Suggest button): the revision's description and diff,
  `output = "bookmark"`.
- `ai.explain` ("Explain this change"): the revision's description and
  diff, `output = "panel"`.
- `ai.review` ("Review this change"): the same, `output = "notes"`: each
  finding that cites a line becomes a review note from the provider.

### AI providers

An `agent` row goes to one AI provider. diffui bundles four, each a plugin
with its own page in Settings → AI (the pencil), where Test sends a short
prompt and shows the reply:

- `claude-code`: runs `claude -p <prompt>` in the repository, `stdin` on its
  standard input. Settings: `program`, `model` (passed as `--model`).
- `codex`: runs `codex exec --skip-git-repo-check <prompt>`, the same way.
  Settings: `program`, `model`.
- `anthropic`: the Messages API, streamed. Settings: `model` (default
  `claude-opus-5-5`, chosen from the API's list), `base_url` (for a proxy),
  and the `api_key`, kept in the Keychain or read from `ANTHROPIC_API_KEY`.
- `openai`: any OpenAI-compatible chat completions API (OpenAI, OpenRouter,
  Ollama, LM Studio): `base_url` (default `https://api.openai.com/v1`), an
  optional `api_key` (or `OPENAI_API_KEY`), a `model` chosen from the
  server's list, which it needs to be set up, and a `name` diffui calls it
  by ("Ollama").

`[ai] command` in config.toml is a fifth, "Custom command": a program that
takes the prompt as its last argument and `stdin` on its standard input.

The shipped AI rows' `when` starts with `ai`: without a provider set up they
are left out of menus and greyed in the palette with what to do. In the
revision menu every `agent` row (yours too) sits in a submenu named after the
provider in use, with sparkles before each label; the pull request rows sit in
one named after the repository's forge.

Which one runs is `[ai] provider`: Settings → AI writes it to settings.toml;
in config.toml it wins and locks the setting. `"auto"` (the default) takes
the custom command when there is one, else the first of `claude-code` (its
program is on the `PATH`), `codex` (likewise), `anthropic` (a key is saved or
set), `openai` (it has a model). A plugin of your own is used only when
named. A named provider has to be set up too; until it is, the AI commands
say what's missing.
Every AI command sends the diff to that provider: the palette row and the
Generate buttons say which.

```toml
[ai]
provider = "anthropic"       # or "auto", "custom", "claude-code", "codex", "openai"
command = ["llm", "-m", "x"] # the custom command; the prompt is added last

[plugins.anthropic]
model = "claude-opus-5-5"

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
   lists the plugins without the app: the bundled ones and the folder's.

diffui ships some plugins built in (its GitHub and GitLab forges, its only
support for either). They run without asking, with the permissions they
declare; `[plugins.<id>] enabled = false` in settings.toml (or the Off
toggle in Settings) turns one off. A plugin in the plugins folder with a
bundled plugin's id replaces it, and is asked about like any other. To change a bundled plugin,
`diffui plugin eject <id>` copies it into the plugins folder with
`diffui.d.ts`; edit the copy, and delete the folder to go back to the
built-in one.

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

[config.schema]                  # optional: defaults for `config`, and Settings' controls
team = { type = "string", default = "core", label = "Team" }
mode = { type = "enum", values = ["fast", "safe"], default = "safe", per_host = true }
program = { type = "path", default = "tool" }   # a field with Browse; "not found" shown inline
api_key = { type = "secret", label = "API key" } # write-only; read with auth.secret("api_key")
```

Types are `string`, `boolean`, `number`, `enum` (a segmented control),
`path` (a field with Browse) and `secret` (kept in the Keychain, never in a
file; `per_host = true` for one per host, and a forge's per-host `token`
secret is what `auth.token(host)` reads, with `page`, `scopes` and `env`
saying where to make one and which variables hold one). `per_host = true`
values sit under `[plugins.<id>.per_host."<host>"]`; the script reads
`config.per_host?.[host]?.mode ?? config.mode`. Settings writes values to
settings.toml, config.toml wins per key, and the plugin restarts on a
change.

Unlike a `[[commands]]` row in config.toml, a plugin's row has no `exec`,
`run`, `stdin`, `output`, `confirm` or prompts: the handler does all of
that in code. `term` is `"pull request"` (the default) or `"merge request"`,
whichever the forge says. The user sets a plugin's config under
`[plugins.<id>]` in config.toml; `token_cmd` there is an argv that prints
the token `auth.token` returns, and `hosts` adds hosts to a forge's claim.

A forge can say more about itself, so diffui names it, opens its links and
explains its errors without running the script (all optional):

```toml
[provides.forge]
hosts = ["github.com", "github.*", "*.github.*"]   # here "*" is any label or more
term = "pull request"
sigil = "#"                     # references read owner/name#12 ("!" for a merge request)
short = "PR"                    # "MR" for a merge request
default_host = "github.com"     # the host a reference without one means
host_env = "GH_HOST"            # a variable that names that host instead, when set
cli = "gh"                      # the program errors tell the user to install and sign in with
pull_request_path = "{owner}/{name}/pull/{number}"   # a pull request's page after the host
subgroups = false               # true when an owner can be several segments (GitLab)
```

An AI provider says so, and names the settings the app reads (both
optional), then calls `ai.register` at load:

```toml
[provides.ai]
ready = "api_key"   # a path, secret or string setting: set up when the program
                    # is there, the secret is set, the string isn't empty
model = "model"     # the string setting Settings' Choose fills from models()
label = "name"      # a string setting with what to call it ("Ollama")
```

A remote goes to the first forge that claims its host: a folder plugin
that replaces a bundled one, then the bundled ones, then the rest, so a
plugin from the folder can't take a bundled forge's host unless that forge
is off or replaced.

### Permissions

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
| `forge` | `forge.register(provider)`, a provider being `pullRequests`, `checks`, `createPullRequest` and `updatePullRequest`, `hosts` (more hosts to claim, asked once after loading), and for a pull request in a tab of its own `view`, `commits`, `commitDiff` and `diff`, `reviewComments` (its review threads, shown read-only in its diff and on the revision whose bookmark has it), and `account` (who the user is on a host, or whose a pasted token is, for Settings' Account rows); only `pullRequests` is required, and diffui hides what a missing method would serve |
| `ai` | `ai.register(provider)` for a plugin with `[provides.ai]`: `complete` (called with a `request`, `{ prompt, input, system, model, cwd }`, and `{ signal }`) returns the reply as text or an (async) iterable of text pieces (required), `models` (the list Settings' model picker shows), `account` (what it runs as, `{ label }`, for its page) |
| `AiError` | `throw new AiError(kind, message)` from an AI provider, `kind` one of `missing-cli`, `auth`, `rate-limited`, `unreachable`, `failed`: diffui adds what to do ("Add an API key in Settings → AI → Anthropic") |
| `ForgeError` | `throw new ForgeError(kind, message, { host, what })` from a provider, `kind` one of `missing-cli`, `auth`, `not-found`, `rate-limited`, `unreachable`, `timeout`, `failed`: diffui then says what to do (`gh auth login --hostname …`) |
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
brings a fresh handle.

`repo.jj.query(argv)` runs a read-only jj command at that operation and
resolves with its stdout. The argv starts with one of `log`, `show`,
`diff`, `interdiff`, `evolog`, `status`, `file show`, `file list`,
`file annotate`, `bookmark list`, `tag list`, `op log`, `op show`,
`config get`, `git remote list`, `workspace list`, and can't carry `-R`,
`--at-op`, `--ignore-working-copy`, `--tool` or `--config`.
`repo.jj.run(argv)` runs any jj command the way `diffui ctl jj` does.

A pull request tab needs all four of `view(pr)`, `commits(pr)`,
`commitDiff(pr, oid, { signal })` and `diff(pr, { signal })`; `pr` is
`{ host, owner, name, number, webUrl }`. `view` gives the header over the
diff: fill its `body` with the description (Markdown) so the tab can show
it. `commits` lists the commits oldest first, each with its whole `message`
and, when the forge counts them, its `additions` and `deletions`. A diff is unified `git diff` text
or files (`{ path, oldPath, status, patch, additions, deletions }`), handed
over as it arrives: return a string, an array, or an (async) iterable of
parts, and never collect a big diff first. `diff` as an async generator
over `exec.stream` (or `http.fetch(url, { stream: true })`'s `body`) keeps
memory flat whatever the size; diffui shows each file once it is whole, and
`signal` aborts (killing the program) when the user closes the tab.

```js
async *diff(pr, { signal }) {
  const out = await exec.stream([config.program, "pr", "diff", String(pr.number)], {
    env: { GH_HOST: pr.host },
    signal,
  });
  yield* out;
  const { ok, stderr } = await out.ended;
  if (!ok) throw new ForgeError("failed", stderr);
}
```

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
  packages, and no global `fetch`. `files.read` reads only the files
  `[permissions] read` lists. Import from `"diffui"` and from your own
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
- `exec` and `http.fetch` hold their whole output (up to 8 MB). For
  anything bigger, a diff above all, use `exec.stream` or `stream: true`
  and hand pieces on as they come.
- `repo.jj.run` resolves when diffui has accepted the command, not when it
  has finished, and the handle is stale once the repository changed: one
  command per handler, or carry on from the next event.

## Checking your work

- `diffui ctl reload` lists every problem with a `path:line:col`.
- `diffui ctl commands --json` shows your command, `declared: true`, and
  whether it can run on what's selected now (with the reason when not).
- `diffui ctl run <id>` runs it the way the user would.

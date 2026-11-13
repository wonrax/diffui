---
name: diffui
description: Drive the diffui app (a GUI for jj and git) from the terminal with `diffui ctl`, and add commands to it with `[[commands]]` rows in its config.toml. Use when the user runs diffui and wants you to show them something in it, run a jj command where they can watch it, or teach it a new command or shortcut.
---

# diffui

diffui is a desktop app for reviewing and rewriting history in jj (and git)
repositories. You can drive the running app with `diffui ctl`, and add
commands to it by writing `[[commands]]` rows in its `config.toml`.

## `diffui ctl`

`diffui ctl` talks to the running app over a private unix socket. It fails
with "diffui isn't running" when no app is open; ask the user to start it.
Every verb takes `--json` (print the app's answer as one JSON object) and
`--repo <path>` (act on the open tab whose repository holds that path, and
bring it forward; otherwise the active tab).

| Verb | Does |
|---|---|
| `diffui ctl state` | Open repositories (tab, root, kind `jj`/`git`/`pr`, which is active), the selected revision (change id, commit id, first line, bookmarks, whether it's `@`), the log's revset, and the active tab's ten latest activity rows (label, status, result, equivalent command). |
| `diffui ctl open <path>` | Open a repository (or a GitHub PR reference) as a tab. Answers with `state`. |
| `diffui ctl select <revset>` | Show the revision `<revset>` names, as `jj show` would. |
| `diffui ctl commands` | Every command id, its label, whether it was declared in config (`declared`), and whether it can run now (`available`, with `reason` when not). |
| `diffui ctl run <id>` | Run a command by id, as its key would. Fails with the reason when it can't run now. |
| `diffui ctl jj <args…>` | Run a jj command line in the app, exactly as typing it in the palette would, except that a rebase or squash runs as written instead of waiting for a pick. A mutation gets the app's activity row, its queue and its confirm for immutable revisions; a view (`log`, `diff`, `show`, `evolog`, `op log`, `file annotate`) opens it; `describe`, `commit`, `split` and `resolve` open their editor; anything else runs as a jj subprocess whose output streams to an activity row. Answers once the line has planned, with what it does (`did`), or why it won't run. Watch `state`'s activity for the outcome. |
| `diffui ctl toast <text>` | Show a toast. |
| `diffui ctl reload` | Read `config.toml`'s `[keys]` and `[[commands]]` again. Answers with what was wrong (`problems`, as `path:line:col: message`). |

Prefer `diffui ctl jj …` over running `jj` yourself when the user is
watching the app: the change shows in its activity and can be undone there.

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
| `revision.selected` | a revision is in context and in the log |
| `revision.mutable` | the revision in context isn't immutable |
| `revision.is_working_copy` | the revision in context is `@` |
| `selection.count` | how many rows are marked in the log |
| `bookmark` | a bookmark is in context: the one picked, else the revision's first |
| `bookmark.has_remote` | the bookmark in context exists on a remote |
| `file` | a file is in context: the one picked, else the diff's selected file |
| `diff.line` | a diff line is in context (line-select mode has a cursor) |

Examples: `repo.kind == "jj" && revision.mutable`, `selection.count >= 2`,
`bookmark && !bookmark.has_remote`.

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

## Checking your work

- `diffui ctl reload` lists every problem with a `path:line:col`.
- `diffui ctl commands --json` shows your command, `declared: true`, and
  whether it can run on what's selected now (with the reason when not).
- `diffui ctl run <id>` runs it the way the user would.

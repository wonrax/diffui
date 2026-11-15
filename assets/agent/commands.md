# Declaring commands

A `[[commands]]` row in config.toml adds a command to diffui's palette,
menus and keys. It needs no code and nothing has to be allowed; when the job
needs logic, write a plugin instead (`diffui skill plugin`).

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
  as what the prompt is about) to the AI provider the user picked
  (`diffui skill ai`).
- `edit = "<path>"`: open a path (relative to the root) in the user's editor
  at `{line}`: `[editor] command`, else `$VISUAL` / `$EDITOR` when it's a GUI
  editor (code, cursor, zed, subl, idea, mate, …), else the system opener.
  Only files in the working copy exist on disk; the command is off for others.

Optional fields:

| Field | Meaning |
|---|---|
| `stdin = "<template>"` | Text fed to the program's standard input (`exec` and `agent` only). |
| `output = "…"` | Where the program's standard output (an agent's reply) goes (`exec` and `agent` only): `activity` (default; the activity row and its output panel), `toast` (its first line once it exits), `clipboard`, `description` (written into the description editor as it arrives, opening it on the revision; undoable), `pr` (the pull request dialog's title, the first line, and body, as it arrives, opening the dialog for the revision), `bookmark` (the bookmark name prompt, its first line made a name such as `fix-parser-crash`, opening the prompt at the revision), `panel` (the output panel, opened as it starts, for text to read: rendered as Markdown, with a Rendered / Raw toggle), `notes` (the panel as it streams, then each list item that starts with a location such as `` `src/lib.rs:40-52` `` filed as a review note on that line of the revision, from the provider; the panel stays when no item cites a line), `open-url` (its first line, an `http(s)` link, opened in the browser) or `none`. A failure puts back what a field held. |
| `confirm = "<template>"` | Ask before running; the text is the question. Can't use `{diff}` or `{recent_descriptions}`. |
| `override = true` | Replace the built-in or shipped command with the same `id`. Without it, a row taking an existing id is left out. |
| `timeout = 300` | Seconds the program may run before it's stopped (default 300). |

A non-zero exit or a missing program shows a toast and an error on the
activity row with the program's last error line. The output panel's Stop
cancels a running program.

## Prompts

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

## Template variables

Write `{name}` in any template; `{{` and `}}` are literal braces. A command
that names a variable with no value in the current context is listed greyed
with the reason. "The revision" is the one the command was picked for (a menu
row, a palette result), else the selected one.

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
| `{prompt.<name>}` | a prompt's answer |

`{diff}` and `{stack_diff}` stop at 200 KB: the rest is left out with a line
that says so, and diffui tells the user once the command has run (the
output panel says it first).

## When-clauses

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
| `ai` | an AI provider is picked (or found automatically) and set up |
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

## Menus and keys

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
collides with; `reload` says so. `[keys]` rebinds any command by id (find
ids with `diffui ctl commands`), and keeps an id it doesn't know yet for
when a reload declares it:

```toml
[keys]
"user.open-in-zed" = ["cmd+shift+e"]

[keys.base]
"file.next" = "n"
```

## Shipped commands

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

The `ai.*` rows' `when` starts with `ai`: without a provider set up they are
left out of menus and greyed in the palette with what to do.

## Checking your work

- `diffui ctl reload` lists every problem with a `path:line:col`.
- `diffui ctl commands --json` shows your command, `declared: true`, and
  whether it can run on what's selected now (with the reason when not).
- `diffui ctl run <id>` runs it the way the user would.

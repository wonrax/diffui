# `diffui ctl`

`diffui ctl` talks to the running app over a private unix socket. It fails
with "diffui isn't running" when no app is open; ask the user to start it.
Every verb takes `--json` (print the app's answer as one JSON object) and
`--repo <path>` (act on the open tab whose repository holds that path, and
bring it forward; otherwise the active tab).

| Verb | Does |
|---|---|
| `diffui ctl state` | Open tabs (tab, root, name, kind `jj`/`git`/`pr`/`patch`/`compare`, which is active), the selected revision (change id, commit id, first line, bookmarks, whether it's `@`), the log's revset, and the active tab's ten latest activity rows (label, status, result, equivalent command). |
| `diffui ctl open <path>` | Open a repository, or a pull request (`https://github.com/o/r/pull/12`, `o/r#12`, a GitLab `https://host/group/name/-/merge_requests/7` or `group/name!7`), as a tab. A patch file opens as a patch, by the same rule as `diffui <file>` (`diffui skill patches`). Answers with `state`. |
| `diffui ctl select <revset>` | Show the revision `<revset>` names, as `jj show` would. |
| `diffui ctl commands` | Every command id, its label, whether it was declared in config (`declared`), and whether it can run now (`available`, with `reason` when not). |
| `diffui ctl run <id>` | Run a command by id, as its key would, on the selected revision (select one first). Fails with the reason when it can't run now. |
| `diffui ctl jj <args…>` | Run a jj command line in the app (see below). |
| `diffui ctl toast <text>` | Show a toast. |
| `diffui ctl reload` | Read `config.toml` and the plugins folder again, and start every plugin afresh. Answers with what was wrong (`problems`, as `path:line:col: message`). |
| `diffui ctl plugins` | Every plugin found: its id, folder, state (`running`, `loading`, `not-allowed`, `off`, `failed` with the `error`), what it registered and the end of its log. |

## `diffui ctl jj`

Prefer `diffui ctl jj …` over running `jj` yourself when the user is
watching the app: the change shows in its activity and can be undone there.
It runs the line exactly as typing it in the palette would, except that a
rebase or squash runs as written instead of waiting for a pick:

- a mutation gets the app's activity row, its queue and its confirm for
  immutable revisions;
- a view (`log`, `diff`, `interdiff`, `show`, `evolog`, `op log`,
  `file annotate`) opens it;
- `describe`, `commit` and `split` without `-m` (and `split` without paths),
  and `resolve`, open their editor for the user;
- anything else runs as a jj subprocess whose output streams to an activity
  row.

It answers once the line has planned, with what it does (`did`), or why it
won't run; it doesn't wait for the outcome. Watch `state`'s activity for
that.

# Review notes

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
rewritten, the note stays on its file, marked outdated. Notes are kept per
repository outside it (nothing is committed or pushed), so a note left from
another `jj workspace` of it reaches the user too.

**The user writes notes too.** Reviewing, the user starts notes of their
own on lines, files or the change: feedback for you. They wait on you like
a reply does (status `replied`, the first message's role `user`), so `wait`
and `list --waiting` return them. Treat each as a request: fix it, then
reply under it. The user may also paste notes into your chat as Markdown
(what `diffui notes export` prints); the ids aren't in the paste, so find
them with `list --waiting` before replying.

## The loop

1. Finish the change, then leave notes:
   `diffui notes add --as <you> --file src/lib.rs --line 40-52 "Took the lock before the read so a writer can't slip in between; costs one extra lock per call."`
   Use `-` as the text to read Markdown from stdin (a heredoc). Pass the same
   `--as` name every time (your tool's name, or a session name when several
   of you work in one repository) so the user can tell you apart.
2. Wait for replies, in the background while the user reviews:
   `diffui notes wait` sleeps until the user replies to one of your notes,
   then prints the notes waiting on you and exits 0; it returns at once when
   some already are (`--new` ignores those). It exits 124 after `--timeout`
   (default `30m`; `0` waits forever).
3. For each reply, change the code (amend the change; the notes follow),
   then answer under the note: `diffui notes reply <id> --as <you> "Done:
   switched to an IndexMap."`, with `--resolve` when nothing is left to do.
   Reply without resolving when you have a question back. `diffui notes
   resolve <id>…` resolves without a word.
4. Wait again until the user is done.

| Verb | Does |
|---|---|
| `diffui notes add` | Leave a note; prints it with its id. |
| `diffui notes reply ID` | Answer under a note (`--resolve` to close it too). |
| `diffui notes resolve ID…` / `diffui notes reopen ID…` | Resolve, or open again. |
| `diffui notes list` | Notes grouped by change, newest activity first: open ones, or with `--resolved` all; `--waiting` only those the user wrote in last; `--by NAME` only yours. |
| `diffui notes show ID` | One note and its replies. |
| `diffui notes wait` | Block until the user replies (see above). |
| `diffui notes export` | Notes as Markdown: each change's id and title, each note's `path:lines`, the lines in a fence, and every message. What the app's Copy writes. |
| `diffui notes delete ID…` | Delete notes. |
| `diffui notes prune` | Delete in bulk: resolved notes, archived ones (their change was abandoned), or every note on some changes; `--dry-run` first. |

Every verb takes `--repo <path>` (default: the current directory) and
`--json`. In the output a note's status is `open` (no reply), `replied` (the
user wrote last, a reply or a note of theirs: your turn) or `resolved`;
`outdated` means its lines are gone from the change, `archived` that the
change was abandoned. The user's messages have the role `user`.

## What `diffui notes` doesn't see

- **Pull request review comments.** On a revision whose bookmark has a pull
  request, and in a pull request's tab, diffui shows the forge's review
  threads with the same cards as notes. They're read-only and never stored:
  read and answer them on the forge (`gh`, `glab`), not here.
- **Notes on a patch or comparison tab** live in that tab only and are gone
  when it closes. Ask the user to paste them (Copy).
- `ai.review` files its findings as notes under the AI provider's name; they
  show in `list`, and `--by <you>` leaves them out.

# Patches and comparisons

diffui also shows a diff that has no repository behind it. With the app
running, each of these opens as a tab there; without it, the app starts.

- `diffui -` reads a diff from stdin: `git diff | diffui -`.
- `diffui fix.patch` opens a patch file: any unified diff (`git diff`,
  `diff -u`, `diff -ruN`, svn, hg), or `git format-patch` emails, one tab
  per file with each email of a series as a step. A path is a patch when it
  ends in `.patch`, `.diff`, `.mbox` or `.eml`, or when it's a file no
  repository holds and it reads as a diff; any other path opens its
  repository.
- `diffui diff <left> <right>` compares two files or two folders, and waits
  until its tab closes. That makes it a diff tool for jj and git, which
  delete the folders they compare once the tool exits. Don't run it in the
  foreground of a session you need back: it blocks until the user closes the
  tab.

To show the user one of your own jj changes, don't make a patch: open the
repository and select it (`diffui skill ctl`), so notes and the log work.

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

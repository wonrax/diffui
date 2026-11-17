---
name: diffui
description: Leave review notes on your jj changes in diffui and read the user's replies (`diffui notes`), drive the running diffui app from the terminal (`diffui ctl`), add commands and shortcuts to it, and write plugins for it. Use when the user reviews your jj changes in diffui, wants you to show them something in it or run a jj command they can watch, wants a diffui command, AI provider, forge or remote host set up, or asks for a diffui plugin. `diffui skill` prints this index; it says which `diffui skill <topic>` to read.
---

# diffui

diffui is a desktop app where the user reviews and rewrites jj (and git)
history. An agent uses it to leave the user review notes on its changes and
answer their replies, to drive the running app from the terminal, to teach it
commands, and to extend it with plugins.

This is the index. Each topic below is a reference printed by
`diffui skill <topic>`. Read one only when its trigger holds; most tasks need
one topic or none.

## What holds everywhere

- **The app may or may not be running.** `diffui notes`, `diffui plugin` and
  `diffui skill` work without it. `diffui ctl` needs it and fails with
  "diffui isn't running": ask the user to start it. `diffui <path>` and
  `diffui diff` open a window, starting the app when needed, so run them only
  when the user wants to look.
- **Tabs.** Each tab holds one thing: a jj or git repository, a pull request,
  a patch or a comparison. `diffui ctl` acts on the active tab, which the user
  may have switched; `--repo <path>` picks the tab whose repository holds
  that path.
- **Change ids, not commit ids.** A jj change id survives amends and rebases;
  a commit id doesn't. Notes are anchored to change ids, so name revisions by
  change id or revset in everything you hand diffui.
- **Notes are how you tell the user about your code.** When the user reviews
  your jj changes in diffui, put what they need to know on the lines it's
  about with `diffui notes`, then wait for their replies there, rather than
  summarising in chat. Notes need a jj repository (colocated with git is
  fine).
- **config.toml is the user's.** `$XDG_CONFIG_HOME/diffui/config.toml`, else
  `~/.config/diffui/config.toml`. diffui never writes it, and a key set there
  wins over Settings and locks its control. `settings.toml` beside it is what
  Settings rewrites whole: don't edit it. After editing config.toml or a
  plugin, run `diffui ctl reload` and fix every `path:line:col` it prints.
- **A repository can be on another machine.** A tab opened as `host:/path`
  is served by `diffui-serve` (or `diffui serve`) running there; what reads
  or runs something in the working tree is off for it.
- **Settings has sections**: General, Accounts (forge sign-ins), Hosts
  (other machines), AI and Plugins. Name the section when you send the user
  there.
- **`diffui jj <args>` is the jj CLI diffui links (0.47.1).** Use it when the
  `jj` on the `PATH` is a different version and refuses the repository.

Every flag is in the tools' own `--help` (`diffui notes --help`,
`diffui ctl --help`, `diffui plugin --help`, `diffui diff --help`); the
topics don't repeat them.

## Topics

| `diffui skill <topic>` | Read it when |
|---|---|
| `diffui skill notes` | You changed code in a jj repository the user reviews in diffui, or the user mentions diffui notes, replies, or pastes notes as Markdown. |
| `diffui skill ctl` | diffui is open and the user wants you to show them a revision or repository there, run a jj command they can watch and undo, or run one of its commands. |
| `diffui skill pull-requests` | The user wants a pull or merge request created, updated or stacked from diffui, or diffui shows no pull requests for a GitHub or GitLab remote (sign-in, tokens, a second account on one host, Enterprise and self-hosted hosts). |
| `diffui skill commands` | The user wants a new command, menu item or shortcut in diffui, or a shipped one changed: `[[commands]]` rows, template variables, when-clauses, keys. |
| `diffui skill ai` | The user asks which AI diffui's Generate and Review use, wants another provider or model, or an AI command is greyed out. |
| `diffui skill plugin` | A `[[commands]]` row can't do the job (it needs logic, HTTP, events or a status item), you're writing or fixing a plugin, or one fails to load. |
| `diffui skill forge-plugin` | diffui should show pull requests for a forge it doesn't know (Forgejo, Gitea, Bitbucket), or a forge plugin needs pull request tabs, review comments or sign-in. Read `plugin` first. |
| `diffui skill remote` | The repository is on another machine (a server over ssh, WSL from Windows, a container): setting up a host (Settings → Hosts, or config.toml), where `diffui-serve` comes from, why a remote tab doesn't connect and where its output is, or writing a transport plugin. |
| `diffui skill patches` | You want to show the user a diff that isn't a jj change (a patch, `git diff` output, two files or folders), or make diffui jj's or git's diff tool. |

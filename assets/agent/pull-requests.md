# Pull requests

When a remote is on GitHub or GitLab, diffui shows each bookmark's pull
request (merge request on GitLab) and its checks, and can open and update
them. These are ordinary commands, so `diffui ctl run <id>` runs them on the
selected revision (select one first with `diffui ctl select`):

| Id | Does |
|---|---|
| `forge.create` | Open the create dialog for the revision's bookmark (or a generated one), pushing first. |
| `forge.update` | Open the dialog to change an open pull request's base, title or description. |
| `forge.submit-stack` | Preview a pull request for every bookmark from trunk to the revision, each based on the one below, then push and open or update them once the user confirms. |
| `forge.open` | Open the pull request in the browser. |
| `forge.open-tab` | Open the pull request as a tab. |
| `forge.copy-url` | Copy the pull request's URL. |
| `forge.refresh` | Ask the forge again for every bookmark's pull request and checks. |

Create, update and submit stack wait for the user: nothing is pushed or sent
to the forge until they confirm (Submit stack only reads the stack and asks
the forge which pull requests exist). The dialogs' Submit and Cancel are
commands too (`forge.dialog-submit`, `forge.stack-confirm`, …): they're the
user's answer, so don't run them yourself. Watch `diffui ctl state`'s
activity for the outcome.

## The forges are plugins

GitHub and GitLab are bundled plugins (`github`, `gitlab`), not built into
the app: turning one off leaves that forge without badges or pull request
tabs, `diffui plugin eject <id>` copies one out to change, and a folder
plugin with the same id replaces it. GitHub claims `github.com` and any host
with a `github` label (`github.acme.com`), GitLab any host with a `gitlab`
label, and each also the hosts its CLI is signed in to. Another host needs `hosts = ["git.acme.org"]` under
`[plugins.github]` (or `[plugins.gitlab]`) in config.toml. A forge diffui
doesn't know needs a plugin: `diffui skill forge-plugin`.

## Signing in

Each signs in to a host through its CLI (`gh`, `glab`) or with a personal
access token. `auth = "auto"` (the default) uses the CLI where it is signed
in to the host, else a token, else says how to sign in; `"cli"` or
`"token"` forces one, for every host or per host under
`[plugins.github.per_host."github.acme.com"]`. `program` points one at
another CLI binary, and `api_url` (per host) at a server whose API isn't at
`https://<host>/api/v3` (GitHub) or `/api/v4` (GitLab). Tokens live in the
macOS Keychain, never in a config file; `GH_TOKEN` / `GITHUB_TOKEN` /
`GH_ENTERPRISE_TOKEN` / `GITLAB_TOKEN` are read the way the CLIs read them.
Settings → Accounts → the forge's pencil has a row per host. For scripts and
agents, `diffui plugin auth <id>`:

```sh
diffui plugin auth github --status                     # how github.com signs in, as whom
printf %s "$TOKEN" | diffui plugin auth github --host github.acme.com --token-stdin
diffui plugin auth gitlab --host gitlab.acme.org --sign-out
```

A host can have more accounts than its own sign-in: a work account beside a
personal one. "Another account" on that page names one ("work") and takes
its token; from a terminal it is `--account`:

```sh
printf %s "$WORK_TOKEN" | diffui plugin auth github --account work --token-stdin
diffui plugin auth github --account work --status
```

Nothing says which account a repository belongs to, and nothing needs to:
the forge asks for the repository with the host's own sign-in first, then
with each other account, and uses the first that can see it (remembered per
repository). So a repository in an organisation the personal account can't
see is reached with the work one, whoever owns it. When a repository shows
no pull requests and the right account isn't set up, add it this way.

`--token-stdin` keeps the token only once the forge says whose it is
(`GET /user`). GitHub needs the `repo` scope (fine-grained: metadata,
contents, commit statuses and checks read; pull requests read and write),
GitLab `api`. Never put a token on a command line or in a file; ask the user
to pipe it in.

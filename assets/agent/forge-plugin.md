# Writing a forge plugin

A forge plugin gives diffui pull requests for a host it doesn't know:
badges and checks on bookmarks, the create and update dialogs, and
optionally pull requests in a tab of their own. Read `diffui skill plugin`
first; this page adds what is forge-specific. `diffui plugin init <id>
--forge <host>` writes a working start (Forgejo's API shape), and the
`ForgeProvider` interface in `diffui plugin types` is the contract.

## The manifest

`[provides] forge = { hosts = ["codeberg.org"] }` makes the plugin a forge,
and the script must then call `forge.register`. `[provides.forge]` can say
more (its `term`, `sigil`, `short`, `default_host`, `host_env`, `cli`,
`pull_request_path`, `subgroups`) so diffui names it, opens its links and
explains its errors without running the script: the d.ts header has each
key. With `pull_request_path`, `diffui ctl open` and the open dialog take the
forge's pull request links and `owner/name#12` references.

A remote goes to the first forge that claims its host: a folder plugin
that replaces a bundled one, then the bundled ones, then the rest, so a
plugin from the folder can't take a bundled forge's host unless that forge
is off or replaced.

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

The user can add hosts to the claim with `hosts` under `[plugins.<id>]` in
config.toml, and give a token command there with `token_cmd`, an argv that
prints the token `auth.token` returns. A `secret` setting named `token` with
`per_host = true` is the token `auth.token(host)` reads and Settings' Account
rows set; its `page`, `scopes` and `env` say where to make one and which
variables hold one.

## The provider

`forge.register(provider)` takes:

- `pullRequests` (the only required method): the pull requests whose head
  is one of the bookmarks asked about, leaving out forks;
- `checks`, `createPullRequest` and `updatePullRequest`: the badges' CI
  state and the two dialogs;
- `hosts`: more hosts to claim, asked once after loading;
- `view`, `commits`, `commitDiff` and `diff`: a pull request in a tab of its
  own. The tab needs all four;
- `reviewComments`: its review threads, shown read-only in its diff and on
  the revision whose bookmark has it;
- `account`: who the user is on a host, or whose a pasted token is, for
  Settings' Account rows.

diffui hides what a missing method would serve; `diffui plugin check` names
each one left out. Throw `ForgeError` with its kind (`auth`, `not-found`,
`rate-limited`, …) so diffui tells the user what to do.

For a pull request tab, `pr` is `{ host, owner, name, number, webUrl }`.
`view` gives the header over the diff: fill its `body` with the description
(Markdown) so the tab can show it. `commits` lists the commits oldest first,
each with its whole `message` and, when the forge counts them, its
`additions` and `deletions`. A diff is unified `git diff` text or files
(`{ path, oldPath, status, patch, additions, deletions }`), handed over as
it arrives: return a string, an array, or an (async) iterable of parts, and
never collect a big diff first. `diff` as an async generator over
`exec.stream` (or `http.fetch(url, { stream: true })`'s `body`) keeps memory
flat whatever the size; diffui shows each file once it is whole, and
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

## The Forgejo plugin, in full

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

// @ts-check
/// <reference path="../../assets/diffui.d.ts" />
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

// GitLab, through the `glab` CLI's `glab api` (on the PATH and signed in to
// the host: gitlab.com, or a self-hosted GitLab glab is configured for).
// Projects are addressed by their URL-encoded path, so a project in a
// subgroup works like any other.

import {
  exec,
  forge,
  type Checks,
  type FilePatch,
  type ForgeRemote,
  type PrRef,
  type PullRequest,
} from "diffui";
import { api, apiStreamed } from "./glab.ts";

/** GitLab's largest page. */
const PAGE_SIZE = 100;

/** A merge request, as the API returns one. */
interface Mr {
  iid: number;
  web_url: string;
  title: string;
  source_branch: string;
  target_branch: string;
  sha?: string | null;
  state: string;
  draft?: boolean;
  work_in_progress?: boolean;
  source_project_id?: number | null;
  target_project_id?: number | null;
  author?: { username: string } | null;
  changes_count?: string | null;
}

/** A commit, from the commits endpoints. */
interface Commit {
  id: string;
  title?: string;
  message?: string | null;
  author_name?: string;
  author_email?: string;
  authored_date?: string | null;
  stats?: { additions: number; deletions: number } | null;
  last_pipeline?: { status: string; web_url?: string | null } | null;
}

/** One file of a diff listing. */
interface MrDiff {
  old_path: string;
  new_path: string;
  new_file?: boolean;
  renamed_file?: boolean;
  deleted_file?: boolean;
  diff?: string;
}

/** `projects/<path>`, the project's path URL-encoded as the API wants it. */
export function project(owner: string, name: string): string {
  let encoded = "";
  for (const byte of new TextEncoder().encode(`${owner}/${name}`)) {
    const char = String.fromCharCode(byte);
    encoded += /[A-Za-z0-9\-_.~]/.test(char) ? char : `%${byte.toString(16).toUpperCase().padStart(2, "0")}`;
  }
  return `projects/${encoded}`;
}

function pullRequest(mr: Mr): PullRequest {
  return {
    number: mr.iid,
    url: mr.web_url,
    title: mr.title,
    head: mr.source_branch,
    base: mr.target_branch,
    headSha: mr.sha ?? "",
    state: mrState(mr),
    review: null,
  };
}

function mrState(mr: Mr): PullRequest["state"] {
  switch (mr.state) {
    case "merged":
      return "merged";
    case "closed":
    case "locked":
      return "closed";
    default:
      return mr.draft || mr.work_in_progress ? "draft" : "open";
  }
}

const isOpen = (pr: PullRequest) => pr.state === "open" || pr.state === "draft";

/**
 * For each of `branches`, its pull request among `prs`: the open one when
 * there is one, else the latest. In `branches`' order.
 */
function latestPerBranch(prs: PullRequest[], branches: string[]): PullRequest[] {
  const best: (PullRequest | undefined)[] = branches.map(() => undefined);
  for (const pr of prs) {
    const slot = branches.indexOf(pr.head);
    if (slot < 0) continue;
    const held = best[slot];
    const better =
      held === undefined ||
      (isOpen(pr) !== isOpen(held) ? isOpen(pr) : pr.number > held.number);
    if (better) best[slot] = pr;
  }
  return best.filter((pr) => pr !== undefined);
}

function filePatch(file: MrDiff): FilePatch {
  const status = file.new_file
    ? "added"
    : file.deleted_file
      ? "deleted"
      : file.renamed_file
        ? "renamed"
        : "modified";
  return {
    path: file.new_path,
    oldPath: file.renamed_file ? file.old_path : null,
    status,
    patch: file.diff ?? "",
  };
}

/**
 * Every page of a per-file diff listing at `path` (which takes `per_page` /
 * `page`), handed over a file at a time.
 */
async function* diffs(pr: PrRef, path: string, signal: AbortSignal): AsyncGenerator<FilePatch> {
  for (let page = 1; ; page++) {
    const files = await apiStreamed<MrDiff[]>(pr.host, [`${path}?per_page=${PAGE_SIZE}&page=${page}`], signal);
    for (const file of files) yield filePatch(file);
    if (files.length < PAGE_SIZE) return;
  }
}

/** The hosts glab has been signed in to, from its config file's `hosts`. */
async function configuredHosts(): Promise<string[]> {
  const out = await exec([
    "sh",
    "-c",
    'cat "${GLAB_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/glab-cli}/config.yml"',
  ]);
  return out.ok ? yamlHosts(out.stdout) : [];
}

/**
 * The host keys of glab's `config.yml`, one level under `hosts:`. Host names
 * only; nothing else is read.
 */
export function yamlHosts(text: string): string[] {
  const hosts: string[] = [];
  let inHosts = false;
  let indent: number | undefined;
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (trimmed === "" || trimmed.startsWith("#")) continue;
    const depth = line.length - line.trimStart().length;
    if (depth === 0) {
      inHosts = line.trimEnd() === "hosts:";
      indent = undefined;
      continue;
    }
    if (!inHosts) continue;
    indent ??= depth;
    if (depth !== indent || !trimmed.endsWith(":")) continue;
    const key = trimmed.slice(0, -1).replace(/^["']+|["']+$/g, "");
    if (key.includes(".") && !key.startsWith(".") && !/[~ ]/.test(key)) hosts.push(key.toLowerCase());
  }
  return hosts;
}

const remoteOf = (remote: ForgeRemote | null): ForgeRemote => {
  // diffui asks about a repository only through the remote this forge claimed.
  if (remote === null) throw new Error("No GitLab remote");
  return remote;
};

/** Write a merge request: `fields` as `-f key=value`. */
async function writeMr(
  remote: ForgeRemote,
  method: string,
  path: string,
  fields: [string, string][],
): Promise<PullRequest> {
  const args = [path, "-X", method];
  for (const [key, value] of fields) args.push("-f", `${key}=${value}`);
  return pullRequest(await api<Mr>(remote.host, args));
}

const prProject = (pr: PrRef) => project(pr.owner, pr.name);

forge.register({
  hosts: configuredHosts,

  async pullRequests(repo, { branches }) {
    const remote = remoteOf(repo.forgeRemote);
    const path = `${project(remote.owner, remote.name)}/merge_requests?state=all&order_by=updated_at&per_page=${PAGE_SIZE}`;
    const listed = await api<Mr[]>(remote.host, [path]);
    const prs = listed
      // A fork's merge request names the fork's branch.
      .filter((mr) => (mr.source_project_id ?? null) === (mr.target_project_id ?? null))
      .map(pullRequest);
    return latestPerBranch(prs, branches);
  },

  async checks(repo, sha): Promise<Checks> {
    const remote = remoteOf(repo.forgeRemote);
    const path = `${project(remote.owner, remote.name)}/repository/commits/${sha}`;
    const commit = await api<Commit>(remote.host, [path]);
    const pipeline = commit.last_pipeline;
    if (pipeline == null) return { state: "none", url: null };
    let state: Checks["state"];
    switch (pipeline.status) {
      case "success":
        state = "success";
        break;
      case "failed":
      case "canceled":
        state = "failure";
        break;
      case "created":
      case "waiting_for_resource":
      case "preparing":
      case "pending":
      case "running":
      case "scheduled":
        state = "pending";
        break;
      default:
        state = "none";
    }
    return { state, url: pipeline.web_url ?? null };
  },

  async createPullRequest(repo, request) {
    const remote = remoteOf(repo.forgeRemote);
    const path = `${project(remote.owner, remote.name)}/merge_requests`;
    // GitLab marks a draft by its title.
    const title = request.draft ? `Draft: ${request.title}` : request.title;
    return writeMr(remote, "POST", path, [
      ["source_branch", request.head],
      ["target_branch", request.base],
      ["title", title],
      ["description", request.body],
    ]);
  },

  async updatePullRequest(repo, number, patch) {
    const remote = remoteOf(repo.forgeRemote);
    const path = `${project(remote.owner, remote.name)}/merge_requests/${number}`;
    const fields: [string, string | undefined][] = [
      ["target_branch", patch.base],
      ["title", patch.title],
      ["description", patch.body],
    ];
    return writeMr(
      remote,
      "PUT",
      path,
      fields.filter((field): field is [string, string] => field[1] !== undefined),
    );
  },

  async view(pr) {
    const mr = await api<Mr>(pr.host, [`${prProject(pr)}/merge_requests/${pr.number}`]);
    const changed = parseInt((mr.changes_count ?? "").replace(/\+$/, ""), 10);
    return {
      number: mr.iid,
      author: mr.author?.username ?? "",
      state: mrState(mr),
      base: mr.target_branch,
      head: mr.source_branch,
      changedFiles: Number.isNaN(changed) ? 0 : changed,
      url: mr.web_url,
      title: mr.title,
    };
  },

  async commits(pr) {
    const commits = [];
    for (let page = 1; ; page++) {
      const path = `${prProject(pr)}/merge_requests/${pr.number}/commits?per_page=${PAGE_SIZE}&page=${page}`;
      const listed = await api<Commit[]>(pr.host, [path]);
      for (const commit of listed) {
        commits.push({
          oid: commit.id,
          author: commit.author_name ?? "",
          headline: commit.title ?? "",
          authoredAt: commit.authored_date ?? null,
        });
      }
      if (listed.length < PAGE_SIZE) break;
    }
    // GitLab lists newest first; the tab wants the forge's oldest first,
    // like GitHub's.
    return commits.reverse();
  },

  async commitDiff(pr, oid, { signal }) {
    const base = `${prProject(pr)}/repository/commits/${oid}`;
    const commit = await api<Commit>(pr.host, [base], signal);
    return {
      diff: diffs(pr, `${base}/diff`, signal),
      // Without the commit's own counts, diffui sums the files'.
      additions: commit.stats?.additions ?? null,
      deletions: commit.stats?.deletions ?? null,
      details: {
        message: commit.message ?? commit.title ?? "",
        author: commit.author_name ?? "",
        email: commit.author_email ?? "",
        date: commit.authored_date ?? null,
      },
    };
  },

  diff(pr, { signal }) {
    return diffs(pr, `${prProject(pr)}/merge_requests/${pr.number}/diffs`, signal);
  },
});

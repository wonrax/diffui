// GitLab (gitlab.com, or a self-hosted GitLab), through the `glab` CLI's
// `glab api` or a token. Projects are addressed by their URL-encoded path,
// so a project in a subgroup works like any other.
//
// Each host is reached one of two ways, the `auth` setting's (per host, or
// for all): `cli` runs glab, signed in to the host; `token` calls the REST
// API with a token (`rest.ts`), on the same paths. `auto`, the default,
// takes glab when glab's config has the host, else a token when there is
// one, else glab, whose errors say how to sign in.

import {
  ForgeError,
  auth,
  config,
  files,
  forge,
  kv,
  type Account,
  type Checks,
  type FilePatch,
  type ForgeRemote,
  type PrRef,
  type PullRequest,
} from "diffui";
import * as glab from "./glab.ts";
import * as rest from "./rest.ts";

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
  description?: string | null;
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
    const files = await getBig<MrDiff[]>(pr.host, `${path}?per_page=${PAGE_SIZE}&page=${page}`, pr.webUrl, signal);
    for (const file of files) yield filePatch(file);
    if (files.length < PAGE_SIZE) return;
  }
}

/** The hosts glab has been signed in to, from its config file's `hosts`, read once per load. */
let glabHosts: Promise<string[]> | null = null;
function configuredHosts(): Promise<string[]> {
  // No file (glab never configured): no hosts beyond the manifest's.
  glabHosts ??= files
    .read("${GLAB_CONFIG_DIR:-${XDG_CONFIG_HOME:-~/.config}/glab-cli}/config.yml")
    .catch(() => "")
    .then(yamlHosts);
  return glabHosts;
}

/** The `auth` setting for `host`. */
const mode = (host: string): string => config.per_host?.[host.toLowerCase()]?.auth ?? config.auth ?? "auto";

/** How `host` is reached: the `auth` setting, with `auto` decided once per load. */
const chosen = new Map<string, Promise<"cli" | "token">>();
function methodFor(host: string): Promise<"cli" | "token"> {
  const set = mode(host);
  if (set === "cli" || set === "token") return Promise.resolve(set);
  let method = chosen.get(host);
  if (method === undefined) {
    method = (async () => {
      if ((await configuredHosts()).includes(host.toLowerCase().replace(/:\d+$/, ""))) return "cli";
      const token = await auth.token(host, { git: false }).catch(() => null);
      return token === null ? "cli" : "token";
    })();
    chosen.set(host, method);
  }
  return method;
}

/**
 * Which of the user's accounts reaches the project at `webUrl`: `undefined` for the host's own sign-in, else
 * the name of another account there. Being able to see a project has nothing to do with owning it, so the
 * project itself is asked for: with the host's sign-in first, then with each other account. The first that
 * sees it is used, and kept across restarts when it is another account.
 */
const reaching = new Map<string, Promise<string | undefined>>();
function accountFor(host: string, webUrl?: string): Promise<string | undefined> {
  const project = webUrl === undefined ? undefined : /^[a-z]+:\/\/[^/]+\/(.+?)\/?$/i.exec(webUrl)?.[1];
  if (project === undefined) return Promise.resolve(undefined);
  const key = `${host}/${project}`.toLowerCase();
  let found = reaching.get(key);
  if (found === undefined) {
    found = findAccount(host, project, webUrl, key);
    reaching.set(key, found);
  }
  return found;
}

async function findAccount(host: string, project: string, webUrl: string | undefined, key: string) {
  const others = await auth.accounts(host).catch(() => [] as string[]);
  if (others.length === 0) return undefined;
  const kept: unknown = await kv.get(`account:${key}`).catch(() => undefined);
  if (typeof kept === "string" && others.includes(kept)) return kept;
  if (await sees(host, project, webUrl, undefined)) return undefined;
  for (const other of others) {
    if (await sees(host, project, webUrl, other)) {
      await kv.set(`account:${key}`, other).catch(() => {});
      return other;
    }
  }
  // Nobody sees it: the host's sign-in is the one to say so.
  return undefined;
}

/** Whether `account` (the host's own sign-in when `undefined`) can see `project`. */
async function sees(host: string, project: string, webUrl: string | undefined, account?: string) {
  const path = `projects/${encodeURIComponent(project)}`;
  try {
    if (account !== undefined || (await methodFor(host)) === "token") {
      await rest.call(await apiFor(host, webUrl, account), "GET", path);
    } else {
      await glab.api(host, [path]);
    }
    return true;
  } catch {
    return false;
  }
}

/** The API of `host` with a token: `account`'s when one is named, else the host's own. The `token` mode also takes git's credential. */
async function apiFor(host: string, webUrl?: string, account?: string): Promise<rest.Api> {
  const git = mode(host) === "token" && account === undefined;
  const token = await auth.token(host, { git, account }).catch(() => null);
  if (token === null) throw new ForgeError("auth", `No GitLab token for ${host}`, { host });
  return rest.apiOf(host, token, webUrl);
}

/** The API, when the project at `webUrl` on `host` is reached with a token; `null` for glab. */
async function tokenApi(host: string, webUrl?: string): Promise<rest.Api | null> {
  const account = await accountFor(host, webUrl);
  if (account !== undefined) return apiFor(host, webUrl, account);
  return (await methodFor(host)) === "token" ? apiFor(host, webUrl) : null;
}

/** `GET path` on `host`'s API, through glab or with a token. */
async function get<T>(host: string, path: string, webUrl: string, signal?: AbortSignal): Promise<T> {
  const via = await tokenApi(host, webUrl);
  return via !== null ? rest.call<T>(via, "GET", path, { signal }) : glab.api<T>(host, [path], signal);
}

/** `get` for an answer that can be big, read a piece at a time. */
async function getBig<T>(host: string, path: string, webUrl: string, signal: AbortSignal): Promise<T> {
  const via = await tokenApi(host, webUrl);
  return via !== null ? rest.streamed<T>(via, path, signal) : glab.apiStreamed<T>(host, [path], signal);
}

/** Who `host` knows the user as, and through what. */
async function account(host: string, token?: string, other?: string): Promise<Account> {
  if (token !== undefined) return { method: "token", login: await rest.login(rest.apiOf(host, token)) };
  // Another account is its token's, or nothing.
  if (other !== undefined) {
    if (!(await auth.accounts(host)).includes(other.toLowerCase())) return { method: null, login: null };
    return { method: "token", login: await rest.login(await apiFor(host, undefined, other)) };
  }
  const method = await methodFor(host);
  try {
    if (method === "token") return { method, login: await rest.login(await apiFor(host)) };
    return { method, login: (await glab.api<{ username: string }>(host, ["user"])).username };
  } catch (error) {
    if (error instanceof ForgeError && (error.kind === "auth" || error.kind === "missing-cli")) {
      return { method: null, login: null };
    }
    throw error;
  }
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

/** Write a merge request: `fields` as `-f key=value`, or a JSON body with a token. */
async function writeMr(
  remote: ForgeRemote,
  method: string,
  path: string,
  fields: [string, string][],
): Promise<PullRequest> {
  const via = await tokenApi(remote.host, remote.webUrl);
  if (via !== null) return pullRequest(await rest.call<Mr>(via, method, path, { fields }));
  const args = [path, "-X", method];
  for (const [key, value] of fields) args.push("-f", `${key}=${value}`);
  return pullRequest(await glab.api<Mr>(remote.host, args));
}

const prProject = (pr: PrRef) => project(pr.owner, pr.name);

forge.register({
  async hosts() {
    // And the hosts the user gave diffui a token for.
    const [cli, tokens] = await Promise.all([configuredHosts(), auth.hosts()]);
    return [...new Set([...cli, ...tokens])];
  },

  async pullRequests(repo, { branches }) {
    const remote = remoteOf(repo.forgeRemote);
    const path = `${project(remote.owner, remote.name)}/merge_requests?state=all&order_by=updated_at&per_page=${PAGE_SIZE}`;
    const listed = await get<Mr[]>(remote.host, path, remote.webUrl);
    const prs = listed
      // A fork's merge request names the fork's branch.
      .filter((mr) => (mr.source_project_id ?? null) === (mr.target_project_id ?? null))
      .map(pullRequest);
    return latestPerBranch(prs, branches);
  },

  async checks(repo, sha): Promise<Checks> {
    const remote = remoteOf(repo.forgeRemote);
    const path = `${project(remote.owner, remote.name)}/repository/commits/${sha}`;
    const commit = await get<Commit>(remote.host, path, remote.webUrl);
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
    const mr = await get<Mr>(pr.host, `${prProject(pr)}/merge_requests/${pr.number}`, pr.webUrl);
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
      body: mr.description ?? "",
    };
  },

  async commits(pr) {
    const commits = [];
    for (let page = 1; ; page++) {
      const path = `${prProject(pr)}/merge_requests/${pr.number}/commits?per_page=${PAGE_SIZE}&page=${page}`;
      const listed = await get<Commit[]>(pr.host, path, pr.webUrl);
      for (const commit of listed) {
        commits.push({
          oid: commit.id,
          author: commit.author_name ?? "",
          headline: commit.title ?? "",
          message: commit.message ?? commit.title ?? "",
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
    const commit = await get<Commit>(pr.host, base, pr.webUrl, signal);
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

  account(host, { token, account: other }) {
    return account(host, token, other);
  },
});

// GitHub (github.com or a GitHub Enterprise Server), through the `gh` CLI
// or a token.
//
// Each host is reached one of two ways, the `auth` setting's (per host, or
// for all): `cli` runs gh, signed in to the host; `token` calls the REST and
// GraphQL APIs with a token (`rest.ts`). `auto`, the default, takes gh when
// gh's own list has the host, else a token when there is one, else gh, whose
// errors say how to sign in.
//
// A pull request's diff **streams**: `gh pr diff` stdout (or the API's diff)
// is handed to diffui a piece at a time as it arrives, and diffui shows each
// file once its last hunk is in, so a huge PR paints progressively while the
// download is still running.
import { ForgeError, auth, config, exec, files, forge, kv } from "diffui";
import type {
  Account,
  Checks,
  FilePatch,
  ForgeRemote,
  PrCommit,
  PrInfo,
  PrRef,
  PullRequest,
  Repo,
  ReviewThread,
} from "diffui";
import * as rest from "./rest.ts";

/**
 * GitHub's REST API caps `per_page` at 100; we always request the max and
 * page until a short page signals the end.
 */
const PAGE_SIZE = 100;

/**
 * How many of a project's most recent pull requests a refresh looks through
 * for its branches.
 */
const LIST_LIMIT = "200";

/**
 * How long one request may take to answer; for `gh pr diff`, how long the
 * stream may go without sending anything.
 */
const NETWORK_TIMEOUT_MS = 60_000;

const program: string = config.program;

/** The program, by its file name, for messages. */
const display = program.split("/").pop() || "gh";

/** `host/owner/name`, the form `gh --repo` takes. */
const repoArg = (host: string, slug: string) => `${host}/${slug}`;

const slugOf = (project: { owner: string; name: string }) => `${project.owner}/${project.name}`;

/** The project a repository call is about. */
function projectOf(repo: Repo): ForgeRemote {
  if (!repo.forgeRemote) throw new ForgeError("failed", "This repository has no GitHub remote");
  return repo.forgeRemote;
}

/** Couldn't start it, or it stopped answering. */
function waitError(host: string, error: unknown): unknown {
  if (!(error instanceof Error)) return error;
  switch (error.name) {
    case "NotFoundError":
      return new ForgeError("missing-cli", error.message);
    case "TimeoutError":
      return new ForgeError("timeout", error.message, { host });
    // A cancel: diffui stopped waiting already.
    case "AbortError":
      return error;
    default:
      return new ForgeError("failed", `couldn't run ${display}: ${error.message}`);
  }
}

/**
 * A run that exited non-zero. gh relays the API's own messages, so this
 * keys on the phrases it uses for the cases that have a fix.
 */
function failure(host: string, stderr: string, stdout: string): ForgeError {
  const text = `${stderr}\n${stdout}`;
  const lower = text.toLowerCase();
  const says = (...phrases: string[]) => phrases.some((phrase) => lower.includes(phrase));
  if (
    says(
      "auth login",
      "not logged in",
      "not logged into",
      "authentication required",
      "bad credentials",
      "401 unauthorized",
      "http 401",
      "has not been authenticated",
      "no token found",
    )
  ) {
    return new ForgeError("auth", text.trim(), { host });
  }
  if (says("rate limit")) return new ForgeError("rate-limited", text.trim(), { host });
  if (says("could not resolve to a", "http 404", "404 not found", "404 project not found")) {
    // diffui names what is missing: the pull request, or the project.
    return new ForgeError("not-found", text.trim(), { host });
  }
  if (says("error connecting to", "no such host", "could not resolve host", "dial tcp")) {
    return new ForgeError("unreachable", text.trim(), { host });
  }
  const detail = text
    .trim()
    .split("\n")
    .filter((line) => line.trim() !== "")
    .slice(0, 6)
    .join("\n");
  return new ForgeError(
    "failed",
    detail === "" ? `${display} failed without saying why` : `${display} failed: ${detail}`,
  );
}

/**
 * Run gh with `args` against `host` and return stdout. One request, so it
 * gets `NETWORK_TIMEOUT_MS` to answer.
 */
async function run(host: string, args: string[], signal?: AbortSignal): Promise<string> {
  let out;
  try {
    out = await exec([program, ...args], {
      env: { GH_HOST: host },
      signal,
      timeoutMs: NETWORK_TIMEOUT_MS,
    });
  } catch (error) {
    throw waitError(host, error);
  }
  if (!out.ok) {
    // gh api prints the API's error body on stdout.
    throw failure(host, out.stderr, out.stdout);
  }
  return out.stdout;
}

/** `run`, with stdout parsed as JSON. */
async function json<T>(host: string, args: string[]): Promise<T> {
  const stdout = await run(host, args);
  try {
    return JSON.parse(stdout) as T;
  } catch (error) {
    throw new ForgeError("failed", `${display} returned something diffui couldn't read: ${String(error)}`);
  }
}

/** A whole number, or `null` when `text` isn't one. */
function count(text: string): number | null {
  return /^\d+$/.test(text) ? Number(text) : null;
}

/** One row of `gh pr list --json`. */
interface ListedPr {
  number: number;
  url: string;
  title: string;
  headRefName: string;
  baseRefName: string;
  headRefOid?: string;
  state: string;
  isDraft?: boolean;
  reviewDecision?: string | null;
  isCrossRepository?: boolean;
}

function listedPr(pr: ListedPr): PullRequest {
  const state = pr.state === "MERGED" ? "merged" : pr.state === "CLOSED" ? "closed" : pr.isDraft ? "draft" : "open";
  const reviews = {
    APPROVED: "approved",
    CHANGES_REQUESTED: "changesRequested",
    REVIEW_REQUIRED: "reviewRequired",
  } as const;
  return {
    number: pr.number,
    url: pr.url,
    title: pr.title,
    head: pr.headRefName,
    base: pr.baseRefName,
    headSha: pr.headRefOid ?? "",
    state,
    review: reviews[pr.reviewDecision as keyof typeof reviews] ?? null,
  };
}

/** A pull request as the REST API answers a create or an update. */
interface RestPr {
  number: number;
  html_url: string;
  title: string;
  state: string;
  draft?: boolean;
  merged_at?: string | null;
  head: { ref: string; sha?: string };
  base: { ref: string; sha?: string };
}

function restPr(pr: RestPr): PullRequest {
  const state = pr.merged_at != null ? "merged" : pr.state === "closed" ? "closed" : pr.draft ? "draft" : "open";
  return {
    number: pr.number,
    url: pr.html_url,
    title: pr.title,
    head: pr.head.ref,
    base: pr.base.ref,
    headSha: pr.head.sha ?? "",
    state,
    review: null,
  };
}

/**
 * `gh api` with `fields` as `-f key=value` (strings) and `typed` as
 * `-F key=value` (booleans), answered with a pull request.
 */
async function writePr(
  remote: ForgeRemote,
  method: string,
  path: string,
  fields: [string, string][],
  typed: [string, string][],
): Promise<PullRequest> {
  const args = ["api", "-X", method, path];
  for (const [flag, pairs] of [
    ["-f", fields],
    ["-F", typed],
  ] as const) {
    for (const [key, value] of pairs) args.push(flag, `${key}=${value}`);
  }
  return restPr(await json<RestPr>(remote.host, args));
}

/**
 * One file from the REST "List pull request files" endpoint. `patch` is
 * absent for binary or oversized files.
 */
interface ApiPrFile {
  filename: string;
  previous_filename?: string | null;
  status: string;
  additions?: number;
  deletions?: number;
  patch?: string | null;
}

function filePatch(file: ApiPrFile): FilePatch {
  // The REST vocabulary also has "copied", which diffui shows as a rename:
  // its source path is the closest fit there is.
  const statuses = { added: "added", removed: "deleted", renamed: "renamed", copied: "copied" } as const;
  return {
    path: file.filename,
    oldPath: file.previous_filename ?? null,
    status: statuses[file.status as keyof typeof statuses] ?? "modified",
    patch: file.patch ?? null,
    // The API's counts cover the whole file even when the patch was
    // omitted; trust them over the parsed rows.
    additions: file.additions ?? 0,
    deletions: file.deletions ?? 0,
  };
}

/**
 * Wire shape of `GET /repos/{owner}/{repo}/commits/{sha}`: the same per-file
 * entries as the PR files endpoint, wrapped in a commit object.
 */
interface ApiCommit {
  files?: ApiPrFile[];
  stats?: { additions?: number; deletions?: number } | null;
  commit?: { message?: string; author?: { name?: string; email?: string; date?: string } | null } | null;
}

/**
 * `GET path` of the REST API for a pull request (`gh api path`, or the API
 * with a token), parsed; `what` names the response in an error.
 */
async function api<T>(pr: PrRef, path: string, what: string, signal: AbortSignal): Promise<T> {
  const viaToken = await tokenApi(pr.host, pr.webUrl);
  if (viaToken !== null) return rest.get<T>(viaToken, path, signal);
  const stdout = await run(pr.host, ["api", path], signal);
  try {
    return JSON.parse(stdout) as T;
  } catch (error) {
    throw new ForgeError("failed", `couldn't read the ${what} API response: ${String(error)}`);
  }
}

/** Page through `GET /repos/{owner}/{repo}/pulls/{n}/files`, 100 files a request. */
async function* filesApi(pr: PrRef, signal: AbortSignal): AsyncGenerator<FilePatch> {
  for (let page = 1; ; page++) {
    const path = `repos/${slugOf(pr)}/pulls/${pr.number}/files?per_page=${PAGE_SIZE}&page=${page}`;
    const files = await api<ApiPrFile[]>(pr, path, "pull-request files", signal);
    yield* files.map(filePatch);
    // A short (or empty) page is the last one; GitHub also simply stops
    // listing past its 3,000-file cap.
    if (files.length < PAGE_SIZE) return;
  }
}

/**
 * GitHub's "Sorry, the diff exceeded the maximum number of files" refusal
 * (HTTP 406 / `PullRequest.diff too_large`), as relayed through gh's stderr.
 */
function isDiffTooLarge(text: string): boolean {
  return text.includes("HTTP 406") || text.includes("too_large") || text.includes("exceeded the maximum");
}

/** gh's own list of the hosts it is signed in to, found where gh looks. */
const HOSTS_FILE = "${GH_CONFIG_DIR:-${XDG_CONFIG_HOME:-~/.config}/gh}/hosts.yml";

/** gh's hosts, read once per load. */
let ghHosts: Promise<string[]> | null = null;
function signedInHosts(): Promise<string[]> {
  // No file (gh never signed in): none.
  ghHosts ??= files
    .read(HOSTS_FILE)
    .catch(() => "")
    .then(yamlHosts);
  return ghHosts;
}

/** How `host` is reached: the `auth` setting, with `auto` decided once per load. */
const chosen = new Map<string, Promise<"cli" | "token">>();
function methodFor(host: string): Promise<"cli" | "token"> {
  const mode: string = config.per_host?.[host.toLowerCase()]?.auth ?? config.auth ?? "auto";
  if (mode === "cli" || mode === "token") return Promise.resolve(mode);
  let method = chosen.get(host);
  if (method === undefined) {
    method = (async () => {
      if ((await signedInHosts()).includes(host.toLowerCase().replace(/:\d+$/, ""))) return "cli";
      return (await storedToken(host)) === null ? "cli" : "token";
    })();
    chosen.set(host, method);
  }
  return method;
}

/** A token the user gave diffui for `host` (or set in GH_TOKEN), not git's. */
function storedToken(host: string): Promise<string | null> {
  return auth.token(host, { git: false }).catch(() => null);
}

/**
 * Which of the user's accounts reaches the project at `webUrl`: `undefined` for the host's own sign-in, else
 * the name of another account there (a work account beside a personal one). Nothing says which account a
 * repository belongs to, and being able to see one has nothing to do with owning it, so the project itself is
 * asked for: with the host's sign-in first, then with each other account. The first that sees it is used, and
 * kept across restarts when it is another account.
 */
const reaching = new Map<string, Promise<string | undefined>>();
function accountFor(host: string, webUrl?: string): Promise<string | undefined> {
  const project = webUrl === undefined ? undefined : /^[a-z]+:\/\/[^/]+\/([^/]+\/[^/]+)/i.exec(webUrl)?.[1];
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
  try {
    if (account !== undefined || (await methodFor(host)) === "token") {
      await rest.get(await apiFor(host, webUrl, account), `repos/${project}`);
    } else {
      await run(host, ["api", `repos/${project}`, "--jq", ".id"]);
    }
    return true;
  } catch {
    return false;
  }
}

/**
 * The APIs of `host` with a token: `account`'s when one is named, else the
 * host's own, for a host reached with one. The `token` mode also takes git's
 * credential for the host.
 */
async function apiFor(host: string, webUrl?: string, account?: string): Promise<rest.Api> {
  const explicit = (config.per_host?.[host.toLowerCase()]?.auth ?? config.auth) === "token";
  const token = await auth.token(host, { git: explicit && account === undefined, account }).catch(() => null);
  if (token === null) throw new ForgeError("auth", `No GitHub token for ${host}`, { host });
  return rest.apiOf(host, token, webUrl);
}

/** The API, when the project at `webUrl` on `host` is reached with a token; `null` for gh. */
async function tokenApi(host: string, webUrl?: string): Promise<rest.Api | null> {
  const account = await accountFor(host, webUrl);
  if (account !== undefined) return apiFor(host, webUrl, account);
  return (await methodFor(host)) === "token" ? apiFor(host, webUrl) : null;
}

/**
 * The host keys of gh's `hosts.yml`, each at the top level with its account
 * indented under it. What lets this forge claim a company host whose name
 * doesn't say it is GitHub. Host names only; nothing else is kept.
 */
function yamlHosts(text: string): string[] {
  return text
    .split("\n")
    .filter((line) => !/^[ \t#]/.test(line))
    .map((line) => line.trim().replace(/:$/, "").replace(/^["']+|["']+$/g, "").toLowerCase())
    .filter((key) => key.includes(".") && !key.startsWith(".") && !/[~ ]/.test(key));
}

/** The GraphQL query of a commit's check rollup. */
const CHECKS_QUERY =
  "query($owner:String!,$name:String!,$sha:GitObjectID!){" +
  "repository(owner:$owner,name:$name){object(oid:$sha){" +
  "... on Commit{statusCheckRollup{state}}}}}";

/** The GraphQL query `gh pr list --json` makes, a page of 100 at a time. */
const LIST_QUERY =
  "query($owner:String!,$name:String!,$after:String){repository(owner:$owner,name:$name){" +
  "pullRequests(first:100,after:$after,orderBy:{field:CREATED_AT,direction:DESC}){" +
  "nodes{number url title headRefName baseRefName headRefOid state isDraft reviewDecision isCrossRepository}" +
  "pageInfo{hasNextPage endCursor}}}}";

/**
 * A pull request's review threads, a page of 100 at a time, each with its
 * first 100 comments. GraphQL rather than REST: only it says which threads
 * are outdated or resolved.
 */
const THREADS_QUERY =
  "query($owner:String!,$name:String!,$number:Int!,$after:String){repository(owner:$owner,name:$name){" +
  "pullRequest(number:$number){reviewThreads(first:100,after:$after){" +
  "nodes{path line startLine originalLine diffSide isOutdated isResolved " +
  "comments(first:100){nodes{author{login} body createdAt url diffHunk}}}" +
  "pageInfo{hasNextPage endCursor}}}}}";

/** A pull request's commits, a page of 100 at a time, with their messages and line counts. */
const COMMITS_QUERY =
  "query($owner:String!,$name:String!,$number:Int!,$after:String){repository(owner:$owner,name:$name){" +
  "pullRequest(number:$number){commits(first:100,after:$after){" +
  "nodes{commit{oid message messageHeadline additions deletions authoredDate author{name user{login}}}}" +
  "pageInfo{hasNextPage endCursor}}}}}";

/** GitHub lists at most 250 commits of a pull request. */
const COMMIT_PAGES = 3;

type CommitsPage = {
  repository: {
    pullRequest: {
      commits: {
        nodes: {
          commit: {
            oid: string;
            message: string;
            messageHeadline: string;
            additions: number;
            deletions: number;
            authoredDate: string | null;
            author: { name: string | null; user: { login: string } | null } | null;
          };
        }[];
        pageInfo: { hasNextPage: boolean; endCursor: string | null };
      };
    } | null;
  } | null;
};

/** How many pages of threads a pull request is read to. */
const THREAD_PAGES = 5;

interface ApiThread {
  path: string;
  line: number | null;
  startLine: number | null;
  originalLine: number | null;
  diffSide: "LEFT" | "RIGHT";
  isOutdated: boolean;
  isResolved: boolean;
  comments: {
    nodes: { author: { login: string } | null; body: string; createdAt: string; url: string; diffHunk: string }[];
  };
}

type ThreadsPage = {
  repository: {
    pullRequest: {
      reviewThreads: { nodes: ApiThread[]; pageInfo: { hasNextPage: boolean; endCursor: string | null } };
    } | null;
  } | null;
};

/**
 * The text of the lines a comment is on: the end of its diff hunk, which
 * GitHub cuts at the commented line, on the thread's side.
 */
function hunkLines(hunk: string, side: "LEFT" | "RIGHT", count: number): string[] {
  const skip = side === "LEFT" ? "+" : "-";
  const lines = hunk
    .split("\n")
    .filter((line) => !line.startsWith("@@") && !line.startsWith(skip) && !line.startsWith("\\"))
    .map((line) => line.slice(1));
  return lines.slice(Math.max(0, lines.length - count));
}

function reviewThread(thread: ApiThread): ReviewThread {
  const comments = thread.comments.nodes;
  const first = comments[0];
  const end = thread.line ?? thread.originalLine;
  const count = end !== null && thread.startLine !== null ? end - thread.startLine + 1 : 1;
  return {
    path: thread.path,
    side: thread.diffSide === "LEFT" ? "old" : "new",
    line: thread.isOutdated ? null : thread.line,
    startLine: thread.isOutdated ? null : thread.startLine,
    originalLine: thread.originalLine,
    // A comment on a whole file has no lines, and no hunk to read.
    text: end === null || !first ? [] : hunkLines(first.diffHunk, thread.diffSide, count),
    outdated: thread.isOutdated,
    resolved: thread.isResolved,
    url: first?.url ?? "",
    comments: comments.map((comment) => ({
      author: comment.author?.login ?? "ghost",
      body: comment.body,
      createdAt: comment.createdAt,
    })),
  };
}

/** `gh pr list --limit 200`'s listing through the GraphQL API. */
async function listWithToken(remote: ForgeRemote, via: rest.Api): Promise<ListedPr[]> {
  const listed: ListedPr[] = [];
  let after: string | null = null;
  while (listed.length < Number(LIST_LIMIT)) {
    type Page = {
      repository: {
        pullRequests: { nodes: ListedPr[]; pageInfo: { hasNextPage: boolean; endCursor: string | null } };
      } | null;
    };
    const page: Page = await rest.graphql<Page>(via, LIST_QUERY, {
      owner: remote.owner,
      name: remote.name,
      after,
    });
    if (page.repository === null) {
      throw new ForgeError("not-found", `${slugOf(remote)} wasn't found`, { host: remote.host });
    }
    const prs = page.repository.pullRequests;
    listed.push(...prs.nodes);
    if (!prs.pageInfo.hasNextPage) break;
    after = prs.pageInfo.endCursor;
  }
  return listed;
}

/** The state the tab shows, from a REST pull request. */
function restState(pr: { state: string; draft?: boolean; merged_at?: string | null }): PrInfo["state"] {
  return pr.merged_at != null ? "merged" : pr.state === "closed" ? "closed" : pr.draft ? "draft" : "open";
}

/** Who `host` knows the user as, and through what. */
async function account(host: string, token?: string, other?: string): Promise<Account> {
  if (token !== undefined) {
    return { method: "token", login: await rest.login(rest.apiOf(host, token)) };
  }
  // Another account is its token's, or nothing.
  if (other !== undefined) {
    if (!(await auth.accounts(host)).includes(other.toLowerCase())) return { method: null, login: null };
    return { method: "token", login: await rest.login(await apiFor(host, undefined, other)) };
  }
  const signedOut = (error: unknown) =>
    error instanceof ForgeError && (error.kind === "auth" || error.kind === "missing-cli");
  const method = await methodFor(host);
  try {
    if (method === "token") return { method, login: await rest.login(await apiFor(host)) };
    const login = (await run(host, ["api", "user", "--jq", ".login"])).trim();
    return { method, login: login || null };
  } catch (error) {
    if (signedOut(error)) return { method: null, login: null };
    throw error;
  }
}

forge.register({
  async pullRequests(repo, { branches }) {
    const remote = projectOf(repo);
    const via = await tokenApi(remote.host, remote.webUrl);
    const listed =
      via !== null
        ? await listWithToken(remote, via)
        : await json<ListedPr[]>(remote.host, [
            "pr",
            "list",
            "--repo",
            repoArg(remote.host, slugOf(remote)),
            "--state",
            "all",
            "--limit",
            LIST_LIMIT,
            "--json",
            "number,url,title,headRefName,baseRefName,headRefOid,state,isDraft,reviewDecision,isCrossRepository",
          ]);
    // A fork's branch can have the same name as one of ours.
    return listed
      .filter((pr) => !pr.isCrossRepository && branches.includes(pr.headRefName))
      .map(listedPr);
  },

  async checks(repo, sha): Promise<Checks> {
    const remote = projectOf(repo);
    const via = await tokenApi(remote.host, remote.webUrl);
    let said: string;
    if (via !== null) {
      type Rollup = { repository: { object: { statusCheckRollup?: { state: string } | null } | null } | null };
      const data = await rest.graphql<Rollup>(via, CHECKS_QUERY, { owner: remote.owner, name: remote.name, sha });
      said = data.repository?.object?.statusCheckRollup?.state ?? "";
    } else {
      const stdout = await run(remote.host, [
        "api",
        "graphql",
        "-f",
        `query=${CHECKS_QUERY}`,
        "-f",
        `owner=${remote.owner}`,
        "-f",
        `name=${remote.name}`,
        "-f",
        `sha=${sha}`,
        "--jq",
        ".data.repository.object.statusCheckRollup.state",
      ]);
      said = stdout.trim();
    }
    const states = {
      SUCCESS: "success",
      FAILURE: "failure",
      ERROR: "failure",
      PENDING: "pending",
      EXPECTED: "pending",
    } as const;
    const state: Checks["state"] = said in states ? states[said as keyof typeof states] : "none";
    return { state, url: state === "none" ? null : `${remote.webUrl}/commit/${sha}` };
  },

  async createPullRequest(repo, request) {
    const remote = projectOf(repo);
    const path = `repos/${slugOf(remote)}/pulls`;
    const via = await tokenApi(remote.host, remote.webUrl);
    if (via !== null) {
      const { head, base, title, body, draft } = request;
      return restPr((await rest.rest<RestPr>(via, "POST", path, { body: { head, base, title, body, draft } })).data);
    }
    return writePr(
      remote,
      "POST",
      path,
      [
        ["head", request.head],
        ["base", request.base],
        ["title", request.title],
        ["body", request.body],
      ],
      [["draft", request.draft ? "true" : "false"]],
    );
  },

  async updatePullRequest(repo, number, patch) {
    const remote = projectOf(repo);
    const path = `repos/${slugOf(remote)}/pulls/${number}`;
    const fields: [string, string][] = [];
    for (const key of ["base", "title", "body"] as const) {
      const value = patch[key];
      if (value !== undefined) fields.push([key, value]);
    }
    const via = await tokenApi(remote.host, remote.webUrl);
    if (via !== null) {
      return restPr((await rest.rest<RestPr>(via, "PATCH", path, { body: Object.fromEntries(fields) })).data);
    }
    return writePr(remote, "PATCH", path, fields, []);
  },

  async hosts() {
    // Read offline, as gh keeps it: `gh auth status` would ask every host.
    // And the hosts the user gave diffui a token for.
    const [cli, tokens] = await Promise.all([signedInHosts(), auth.hosts()]);
    return [...new Set([...cli, ...tokens])];
  },

  async view(pr): Promise<PrInfo> {
    const via = await tokenApi(pr.host, pr.webUrl);
    if (via !== null) {
      type Pull = RestPr & {
        user?: { login?: string } | null;
        additions?: number;
        deletions?: number;
        changed_files?: number;
        body?: string | null;
      };
      const pull = await rest.get<Pull>(via, `repos/${slugOf(pr)}/pulls/${pr.number}`);
      const totals = typeof pull.additions === "number" && typeof pull.deletions === "number";
      return {
        number: pull.number,
        title: pull.title,
        author: pull.user?.login ?? "",
        state: restState(pull),
        base: pull.base.ref,
        head: pull.head.ref,
        additions: totals ? pull.additions : null,
        deletions: totals ? pull.deletions : null,
        changedFiles: pull.changed_files ?? 0,
        url: pull.html_url,
        body: pull.body ?? "",
      };
    }
    // `\u001f` (the unit separator) frames the fields, so titles with tabs
    // or quotes can't break them; titles can't hold newlines. The body,
    // which can, comes last and takes the rest.
    const jq =
      '[(.number|tostring), .title, .author.login, (if .isDraft and .state == "OPEN" then "DRAFT" else .state end), ' +
      ".baseRefName, .headRefName, (.additions|tostring), (.deletions|tostring), (.changedFiles|tostring), .url, " +
      '(.body // "")] | join("\\u001f")';
    const stdout = await run(pr.host, [
      "pr",
      "view",
      String(pr.number),
      "--repo",
      repoArg(pr.host, slugOf(pr)),
      "--json",
      "number,title,author,state,isDraft,baseRefName,headRefName,additions,deletions,changedFiles,url,body",
      "--jq",
      jq,
    ]);
    const parts = stdout.replace(/\n+$/, "").split("\u001f");
    const next = () => parts.shift() ?? "";
    const number = count(next()) ?? pr.number;
    const [title, author, state, base, head] = [next(), next(), next().toLowerCase(), next(), next()];
    const additions = count(next());
    const deletions = count(next());
    const totals = additions !== null && deletions !== null;
    return {
      number,
      title,
      author,
      state: state as PrInfo["state"],
      base,
      head,
      additions: totals ? additions : null,
      deletions: totals ? deletions : null,
      changedFiles: count(next()) ?? 0,
      url: next(),
      body: parts.join("\u001f"),
    };
  },

  /**
   * The commits through GraphQL on either path: only it counts each
   * commit's lines. A page of 100 at a time, to GitHub's 250.
   */
  async commits(pr): Promise<PrCommit[]> {
    const via = await tokenApi(pr.host, pr.webUrl);
    const commits: PrCommit[] = [];
    let after: string | null = null;
    for (let page = 0; page < COMMIT_PAGES; page++) {
      const variables = { owner: pr.owner, name: pr.name, number: pr.number, after };
      let answer: CommitsPage;
      if (via !== null) {
        answer = await rest.graphql<CommitsPage>(via, COMMITS_QUERY, variables);
      } else {
        const args = ["api", "graphql", "-f", `query=${COMMITS_QUERY}`, "-f", `owner=${pr.owner}`];
        args.push("-f", `name=${pr.name}`, "-F", `number=${pr.number}`);
        if (after !== null) args.push("-f", `after=${after}`);
        answer = (await json<{ data: CommitsPage }>(pr.host, args)).data;
      }
      const listed = answer.repository?.pullRequest?.commits;
      if (!listed) throw new ForgeError("not-found", `${slugOf(pr)}#${pr.number} wasn't found`, { host: pr.host });
      for (const { commit } of listed.nodes) {
        commits.push({
          oid: commit.oid,
          author: commit.author?.user?.login ?? commit.author?.name ?? "",
          headline: commit.messageHeadline,
          message: commit.message,
          authoredAt: commit.authoredDate || null,
          additions: commit.additions,
          deletions: commit.deletions,
        });
      }
      if (!listed.pageInfo.hasNextPage) break;
      after = listed.pageInfo.endCursor;
    }
    return commits;
  },

  /**
   * One commit's diff through the commits REST endpoint, paged; the
   * per-file entries match the files API's, so the same lowering applies.
   */
  async commitDiff(pr, oid, { signal }) {
    const page = (n: number) => `repos/${slugOf(pr)}/commits/${oid}?per_page=${PAGE_SIZE}&page=${n}`;
    const first = await api<ApiCommit>(pr, page(1), "commit", signal);
    async function* files(): AsyncGenerator<FilePatch> {
      let commit = first;
      for (let next = 2; ; next++) {
        const listed = commit.files ?? [];
        yield* listed.map(filePatch);
        if (listed.length < PAGE_SIZE) return;
        commit = await api<ApiCommit>(pr, page(next), "commit", signal);
      }
    }
    const meta = first.commit;
    const author = meta?.author ?? {};
    return {
      diff: files(),
      details: meta
        ? { message: meta.message ?? "", author: author.name ?? "", email: author.email ?? "", date: author.date || null }
        : null,
      // Prefer the commit's own stats over summed files: the API zeroes an
      // oversized blob's per-file counts.
      additions: first.stats ? (first.stats.additions ?? 0) : null,
      deletions: first.stats ? (first.stats.deletions ?? 0) : null,
    };
  },

  /**
   * Prefers the unified diff (`gh pr diff`, or the API's diff media type:
   * one request, true streaming); when GitHub refuses to serve it outright
   * (HTTP 406, more than 300 changed files), pages through the REST files
   * endpoint, which serves per-file patches up to GitHub's 3,000-file
   * listing cap.
   */
  async *diff(pr, { signal }) {
    const host = pr.host;
    const via = await tokenApi(host, pr.webUrl);
    if (via !== null) {
      const response = await rest.stream(
        via,
        `repos/${slugOf(pr)}/pulls/${pr.number}`,
        "application/vnd.github.diff",
        signal,
      );
      if (response.status === 406) {
        yield* filesApi(pr, signal);
        return;
      }
      if (response.body !== null) yield* response.body;
      return;
    }
    let ended;
    try {
      const out = await exec.stream([program, "pr", "diff", String(pr.number), "--repo", repoArg(host, slugOf(pr))], {
        env: { GH_HOST: host },
        signal,
        timeoutMs: NETWORK_TIMEOUT_MS,
      });
      yield* out;
      ended = await out.ended;
    } catch (error) {
      throw waitError(host, error);
    }
    if (ended.ok) return;
    // Only the too-large refusal reroutes: an auth or network failure would
    // fail the files API identically. It arrives before any diff bytes, so
    // nothing is sent twice.
    if (isDiffTooLarge(ended.stderr)) {
      yield* filesApi(pr, signal);
      return;
    }
    throw failure(host, ended.stderr, "");
  },

  async reviewComments(pr): Promise<ReviewThread[]> {
    const via = await tokenApi(pr.host, pr.webUrl);
    const threads: ApiThread[] = [];
    let after: string | null = null;
    for (let page = 0; page < THREAD_PAGES; page++) {
      const variables = { owner: pr.owner, name: pr.name, number: pr.number, after };
      let answer: ThreadsPage;
      if (via !== null) {
        answer = await rest.graphql<ThreadsPage>(via, THREADS_QUERY, variables);
      } else {
        const args = ["api", "graphql", "-f", `query=${THREADS_QUERY}`, "-f", `owner=${pr.owner}`];
        args.push("-f", `name=${pr.name}`, "-F", `number=${pr.number}`);
        if (after !== null) args.push("-f", `after=${after}`);
        answer = (await json<{ data: ThreadsPage }>(pr.host, args)).data;
      }
      const listed = answer.repository?.pullRequest?.reviewThreads;
      if (!listed) throw new ForgeError("not-found", `${slugOf(pr)}#${pr.number} wasn't found`, { host: pr.host });
      threads.push(...listed.nodes);
      if (!listed.pageInfo.hasNextPage) break;
      after = listed.pageInfo.endCursor;
    }
    return threads.filter((thread) => thread.comments.nodes.length > 0).map(reviewThread);
  },

  async account(host, { token, account: other }) {
    return account(host, token, other);
  },
});

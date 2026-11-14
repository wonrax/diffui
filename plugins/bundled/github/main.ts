// GitHub, through the `gh` CLI (on the PATH and signed in to the host:
// github.com or a GitHub Enterprise Server).
//
// A pull request's diff **streams**: `gh pr diff` stdout is handed to diffui
// a piece at a time as it arrives, and diffui shows each file once its last
// hunk is in, so a huge PR paints progressively while the download is still
// running. Lists and writes go through `gh pr list --json` and `gh api`.
import { ForgeError, config, exec, files, forge } from "diffui";
import type { Checks, FilePatch, ForgeRemote, PrCommit, PrInfo, PrRef, PullRequest, Repo } from "diffui";

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

/** `gh api path` for a pull request, parsed; `what` names the response in an error. */
async function api<T>(pr: PrRef, path: string, what: string, signal: AbortSignal): Promise<T> {
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

forge.register({
  async pullRequests(repo, { branches }) {
    const remote = projectOf(repo);
    const listed = await json<ListedPr[]>(remote.host, [
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
    const query =
      "query($owner:String!,$name:String!,$sha:GitObjectID!){" +
      "repository(owner:$owner,name:$name){object(oid:$sha){" +
      "... on Commit{statusCheckRollup{state}}}}}";
    const stdout = await run(remote.host, [
      "api",
      "graphql",
      "-f",
      `query=${query}`,
      "-f",
      `owner=${remote.owner}`,
      "-f",
      `name=${remote.name}`,
      "-f",
      `sha=${sha}`,
      "--jq",
      ".data.repository.object.statusCheckRollup.state",
    ]);
    const states = {
      SUCCESS: "success",
      FAILURE: "failure",
      ERROR: "failure",
      PENDING: "pending",
      EXPECTED: "pending",
    } as const;
    const said = stdout.trim();
    const state: Checks["state"] = said in states ? states[said as keyof typeof states] : "none";
    return { state, url: state === "none" ? null : `${remote.webUrl}/commit/${sha}` };
  },

  async createPullRequest(repo, request) {
    const remote = projectOf(repo);
    return writePr(
      remote,
      "POST",
      `repos/${slugOf(remote)}/pulls`,
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
    const fields: [string, string][] = [];
    for (const key of ["base", "title", "body"] as const) {
      const value = patch[key];
      if (value !== undefined) fields.push([key, value]);
    }
    return writePr(remote, "PATCH", `repos/${slugOf(remote)}/pulls/${number}`, fields, []);
  },

  async hosts() {
    // Read offline, as gh keeps it: `gh auth status` would ask every host.
    // No file (gh never signed in): no hosts beyond the manifest's.
    const text = await files.read(HOSTS_FILE).catch(() => "");
    return yamlHosts(text);
  },

  async view(pr): Promise<PrInfo> {
    // `\u001f` (the unit separator) frames the fields, so titles with tabs
    // or quotes can't break them; titles can't hold newlines.
    const jq =
      '[(.number|tostring), .title, .author.login, (if .isDraft and .state == "OPEN" then "DRAFT" else .state end), ' +
      ".baseRefName, .headRefName, (.additions|tostring), (.deletions|tostring), (.changedFiles|tostring), .url] " +
      '| join("\\u001f")';
    const stdout = await run(pr.host, [
      "pr",
      "view",
      String(pr.number),
      "--repo",
      repoArg(pr.host, slugOf(pr)),
      "--json",
      "number,title,author,state,isDraft,baseRefName,headRefName,additions,deletions,changedFiles,url",
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
    };
  },

  async commits(pr): Promise<PrCommit[]> {
    const jq =
      '.commits[] | [.oid, (.authors[0].login // ""), .messageHeadline, (.authoredDate // "")] | join("\\u001f")';
    const stdout = await run(pr.host, [
      "pr",
      "view",
      String(pr.number),
      "--repo",
      repoArg(pr.host, slugOf(pr)),
      "--json",
      "commits",
      "--jq",
      jq,
    ]);
    return stdout
      .split("\n")
      .filter((line) => line !== "")
      .map((line) => {
        const [oid = "", author = "", headline = "", authoredAt = ""] = line.split("\u001f");
        return { oid, author, headline, authoredAt: authoredAt || null };
      });
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
   * Prefers `gh pr diff` (one request, true streaming); when GitHub refuses
   * to serve the unified diff outright (HTTP 406, more than 300 changed
   * files), pages through the REST files endpoint, which serves per-file
   * patches up to GitHub's 3,000-file listing cap.
   */
  async *diff(pr, { signal }) {
    const host = pr.host;
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
});

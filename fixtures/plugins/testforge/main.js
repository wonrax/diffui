// @ts-check
// Every call runs the configured program with the host in TESTFORGE_HOST, as
// a gh-style forge would: JSON for answers, a unified diff streamed for the
// pull request, exit 4 when it isn't signed in.
import { ForgeError, config, exec, forge } from "diffui";

/** @param {{ code: number | null, stderr: string }} out */
function failure(out) {
  const said = out.stderr.trim();
  if (out.code === 4) return new ForgeError("auth", said);
  if (out.code === 5) return new ForgeError("not-found", said);
  return new ForgeError("failed", `testforge failed: ${said}`);
}

/** @param {unknown} error */
function missing(error) {
  if (error instanceof Error && error.name === "NotFoundError") {
    return new ForgeError("missing-cli", error.message);
  }
  return error;
}

/** @param {string} host @param {string[]} args @param {AbortSignal} [signal] */
async function run(host, args, signal) {
  let out;
  try {
    out = await exec([config.program, ...args], { env: { TESTFORGE_HOST: host }, signal });
  } catch (error) {
    throw missing(error);
  }
  if (!out.ok) throw failure(out);
  return JSON.parse(out.stdout);
}

/** @param {{ owner: string, name: string }} project */
const slug = (project) => `${project.owner}/${project.name}`;

forge.register({
  async pullRequests(repo, { branches }) {
    const remote = repo.forgeRemote;
    if (!remote) return [];
    /** @type {import("diffui").PullRequest[]} */
    const pulls = await run(remote.host, ["pulls", slug(remote)]);
    return pulls.filter((pr) => branches.includes(pr.head));
  },
  async checks() {
    return { state: "success" };
  },
  async hosts() {
    return run("", ["hosts"]);
  },
  view: (pr) => run(pr.host, ["view", slug(pr), String(pr.number)]),
  commits: (pr) => run(pr.host, ["commits", slug(pr), String(pr.number)]),
  async commitDiff(pr, oid, { signal }) {
    const commit = await run(pr.host, ["commit", slug(pr), oid], signal);
    return { diff: commit.files, details: commit.details };
  },
  async *diff(pr, { signal }) {
    let out;
    try {
      out = await exec.stream([config.program, "diff", slug(pr), String(pr.number)], {
        env: { TESTFORGE_HOST: pr.host },
        signal,
      });
    } catch (error) {
      throw missing(error);
    }
    yield* out;
    const end = await out.ended;
    if (!end.ok) throw failure(end);
  },
});

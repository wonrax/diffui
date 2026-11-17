// Running `glab` and turning how it failed into a ForgeError.

import { ForgeError, config, exec } from "diffui";

/** One request gets this long to answer. */
const NETWORK_TIMEOUT_MS = 60_000;

/** What errors call the program: its file name, as configured. */
function display(): string {
  const name = String(config.program).split("/").pop();
  return name || "glab";
}

/**
 * The host as glab names it: lowercase, without the port an http(s) remote
 * can carry.
 */
function hostname(host: string): string {
  return host.toLowerCase().replace(/:\d+$/, "");
}

/** `glab api` on `host` with `args`, its JSON answer parsed. */
export async function api<T>(host: string, args: string[], signal?: AbortSignal): Promise<T> {
  const name = hostname(host);
  return parse(await run(name, ["api", "--hostname", name, ...args], signal));
}

/**
 * `api`, with stdout read a piece at a time: for an answer that can be big
 * (a page of diffs), which `exec` would cap. `signal` kills it.
 */
export async function apiStreamed<T>(host: string, args: string[], signal: AbortSignal): Promise<T> {
  const name = hostname(host);
  let out;
  try {
    out = await exec.stream([config.program, "api", "--hostname", name, ...args], {
      env: { GITLAB_HOST: name },
      signal,
      timeoutMs: NETWORK_TIMEOUT_MS,
    });
  } catch (error) {
    throw waitError(error);
  }
  let stdout = "";
  try {
    for await (const piece of out) stdout += piece;
  } catch (error) {
    throw waitError(error);
  }
  const end = await out.ended;
  if (!end.ok) throw failure(end.stderr, stdout);
  return parse(stdout);
}

/**
 * Run with `args` against `host` and return stdout. One request, so it gets
 * NETWORK_TIMEOUT_MS to answer.
 */
async function run(host: string, args: string[], signal?: AbortSignal): Promise<string> {
  let out;
  try {
    out = await exec([config.program, ...args], {
      env: { GITLAB_HOST: host },
      signal,
      timeoutMs: NETWORK_TIMEOUT_MS,
    });
  } catch (error) {
    throw waitError(error);
  }
  if (!out.ok) throw failure(out.stderr, out.stdout);
  return out.stdout;
}

function parse<T>(stdout: string): T {
  try {
    return JSON.parse(stdout);
  } catch (error) {
    throw new ForgeError("failed", `${display()} returned something diffui couldn't read: ${(error as Error).message}`);
  }
}

/** Couldn't start it, or it stopped answering. */
function waitError(error: unknown): unknown {
  if (!(error instanceof Error)) return error;
  switch (error.name) {
    case "NotFoundError":
      return new ForgeError("missing-cli", error.message);
    case "TimeoutError":
      return new ForgeError("timeout", error.message);
    // A cancel is the app's own doing; it says nothing about it.
    case "AbortError":
      return error;
    default:
      return new ForgeError("failed", `couldn't run ${display()}: ${error.message}`);
  }
}

/**
 * A run that exited non-zero. glab relays the API's own messages, so this
 * keys on the phrases it uses for the cases that have a fix.
 */
export function failure(stderr: string, stdout: string): ForgeError {
  const text = `${stderr}\n${stdout}`;
  const lower = text.toLowerCase();
  const said = (...phrases: string[]) => phrases.some((phrase) => lower.includes(phrase));
  if (
    said(
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
    return new ForgeError("auth", text.trim());
  }
  if (said("rate limit")) return new ForgeError("rate-limited", text.trim());
  if (said("could not resolve to a", "http 404", "404 not found", "404 project not found")) {
    return new ForgeError("not-found", text.trim());
  }
  if (said("error connecting to", "no such host", "could not resolve host", "dial tcp")) {
    return new ForgeError("unreachable", text.trim());
  }
  const detail = text
    .trim()
    .split("\n")
    .filter((line) => line.trim() !== "")
    .slice(0, 6)
    .join("\n");
  return new ForgeError(
    "failed",
    detail === "" ? `${display()} failed without saying why` : `${display()} failed: ${detail}`,
  );
}

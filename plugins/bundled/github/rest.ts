// GitHub's REST and GraphQL APIs over `http.fetch`, with a token: what the
// plugin uses for a host where it signs in with a token rather than gh.
//
// Errors are typed from the status the way gh's are from its messages: 401
// and 403 are `auth`, unless the rate-limit headers say the limit was hit;
// 404 is `not-found`.

import { ForgeError, config, http } from "diffui";
import type { Response } from "diffui";

/** One request gets this long to answer; a stream this long per piece. */
const NETWORK_TIMEOUT_MS = 60_000;

/** Where a host's APIs are, and the token to send. */
export interface Api {
  host: string;
  /** REST, without a trailing slash: `https://api.github.com`, `https://<host>/api/v3`. */
  rest: string;
  graphql: string;
  token: string;
}

/**
 * The APIs of `host`. `webUrl` (a project's page) gives the scheme of a
 * company host; the per-host `api_url` setting overrides the rest.
 */
export function apiOf(host: string, token: string, webUrl?: string): Api {
  const override: string | undefined = config.per_host?.[host.toLowerCase()]?.api_url;
  const scheme = webUrl?.startsWith("http://") ? "http" : "https";
  let rest: string;
  if (override) rest = override.replace(/\/+$/, "");
  else if (host === "github.com") rest = "https://api.github.com";
  // GitHub Enterprise Cloud with data residency: api.<tenant>.ghe.com.
  else if (host.endsWith(".ghe.com")) rest = `https://api.${host}`;
  else rest = `${scheme}://${host}/api/v3`;
  const graphql = rest.endsWith("/api/v3") ? `${rest.slice(0, -"/v3".length)}/graphql` : `${rest}/graphql`;
  return { host, rest, graphql, token };
}

function headers(api: Api, accept = "application/vnd.github+json"): Record<string, string> {
  return {
    accept,
    authorization: `Bearer ${api.token}`,
    "x-github-api-version": "2022-11-28",
  };
}

/** A request that didn't get an answer. */
function waitError(host: string, error: unknown): unknown {
  if (!(error instanceof Error)) return error;
  switch (error.name) {
    case "NetworkError":
      return new ForgeError("unreachable", error.message, { host });
    case "TimeoutError":
      return new ForgeError("timeout", error.message, { host });
    default:
      // AbortError (diffui stopped waiting) and PermissionError pass as they are.
      return error;
  }
}

/** What GitHub said, from an error response's body. */
function said(body: string): string {
  try {
    const parsed = JSON.parse(body);
    if (typeof parsed?.message === "string") return parsed.message;
  } catch {
    // Not JSON: the text itself.
  }
  return body.trim().split("\n").slice(0, 6).join("\n");
}

/** A response that isn't 2xx, typed by its status and headers. */
export function failure(host: string, response: Response, body: string): ForgeError {
  const message = said(body);
  const limited =
    response.status === 429 ||
    response.headers.get("x-ratelimit-remaining") === "0" ||
    response.headers.has("retry-after") ||
    /rate limit/i.test(message);
  if ((response.status === 403 || response.status === 429) && limited) {
    return new ForgeError("rate-limited", message, { host });
  }
  if (response.status === 401 || response.status === 403) return new ForgeError("auth", message, { host });
  if (response.status === 404) return new ForgeError("not-found", message, { host });
  return new ForgeError(
    "failed",
    message === "" ? `GitHub answered ${response.status}` : `GitHub answered ${response.status}: ${message}`,
  );
}

async function send(
  api: Api,
  method: "GET" | "POST" | "PATCH",
  url: string,
  options: { body?: unknown; accept?: string; signal?: AbortSignal; stream?: boolean } = {},
): Promise<Response> {
  const init = {
    method,
    headers: {
      ...headers(api, options.accept),
      ...(options.body === undefined ? {} : { "content-type": "application/json" }),
    },
    body: options.body === undefined ? null : JSON.stringify(options.body),
    signal: options.signal,
    timeoutMs: NETWORK_TIMEOUT_MS,
    stream: options.stream ?? false,
  };
  try {
    return await http.fetch(url, init);
  } catch (error) {
    throw waitError(api.host, error);
  }
}

function parse<T>(text: string): T {
  try {
    return JSON.parse(text) as T;
  } catch (error) {
    throw new ForgeError("failed", `GitHub returned something diffui couldn't read: ${String(error)}`);
  }
}

/** `method path` on the REST API, its JSON answer and the next page's URL. */
export async function rest<T>(
  api: Api,
  method: "GET" | "POST" | "PATCH",
  path: string,
  options: { body?: unknown; signal?: AbortSignal } = {},
): Promise<{ data: T; next: string | null }> {
  const url = path.startsWith("http") ? path : `${api.rest}/${path.replace(/^\/+/, "")}`;
  const response = await send(api, method, url, options);
  const text = await response.text();
  if (!response.ok) throw failure(api.host, response, text);
  return { data: parse<T>(text), next: nextPage(response.headers.get("link")) };
}

/** `GET path`, its JSON answer. */
export async function get<T>(api: Api, path: string, signal?: AbortSignal): Promise<T> {
  return (await rest<T>(api, "GET", path, { signal })).data;
}

/** The `rel="next"` URL of a `Link` header. */
export function nextPage(link: string | null): string | null {
  for (const part of (link ?? "").split(",")) {
    const match = /<([^>]+)>\s*;\s*rel="?next"?/.exec(part);
    if (match) return match[1] ?? null;
  }
  return null;
}

/** A GraphQL query's `data`. GitHub answers 200 with `errors` for most failures. */
export async function graphql<T>(api: Api, query: string, variables: Record<string, unknown>): Promise<T> {
  const response = await send(api, "POST", api.graphql, { body: { query, variables } });
  const text = await response.text();
  if (!response.ok) throw failure(api.host, response, text);
  const answer = parse<{ data?: T; errors?: { type?: string; message?: string }[] }>(text);
  const error = answer.errors?.[0];
  if (error) {
    const message = error.message ?? "GraphQL error";
    if (error.type === "NOT_FOUND") throw new ForgeError("not-found", message, { host: api.host });
    if (error.type === "RATE_LIMITED") throw new ForgeError("rate-limited", message, { host: api.host });
    throw new ForgeError("failed", `GitHub said: ${message}`);
  }
  return answer.data as T;
}

/**
 * `GET path` with its body streamed, as `accept`: the response when it is
 * 2xx or 406 (the caller decides what a refusal means), else the typed
 * failure.
 */
export async function stream(api: Api, path: string, accept: string, signal: AbortSignal): Promise<Response> {
  const response = await send(api, "GET", `${api.rest}/${path}`, { accept, signal, stream: true });
  if (response.ok || response.status === 406) return response;
  throw failure(api.host, response, await response.text());
}

/** Who the token belongs to. */
export async function login(api: Api): Promise<string> {
  return (await get<{ login: string }>(api, "user")).login;
}

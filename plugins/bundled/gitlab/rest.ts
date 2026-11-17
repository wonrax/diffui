// GitLab's REST API over `http.fetch`, with a token: what the plugin uses for
// a host where it signs in with a token rather than glab. The paths are
// glab api's, so the two transports share every call in main.ts.
//
// Errors are typed from the status: 401 and 403 are `auth`, unless the
// rate-limit headers say the limit was hit; 404 is `not-found`.

import { ForgeError, config, http } from "diffui";
import type { Response } from "diffui";

/** One request gets this long to answer; a stream this long per piece. */
const NETWORK_TIMEOUT_MS = 60_000;

/** Where a host's API is, and the token to send. */
export interface Api {
  host: string;
  /** Without a trailing slash: `https://gitlab.com/api/v4`. */
  base: string;
  token: string;
}

/**
 * The API of `host`. `webUrl` (a project's page) gives the scheme; the
 * per-host `api_url` setting overrides it.
 */
export function apiOf(host: string, token: string, webUrl?: string): Api {
  const override: string | undefined = config.per_host?.[host.toLowerCase()]?.api_url;
  const scheme = webUrl?.startsWith("http://") ? "http" : "https";
  const base = override ? override.replace(/\/+$/, "") : `${scheme}://${host}/api/v4`;
  return { host, base, token };
}

function waitError(host: string, error: unknown): unknown {
  if (!(error instanceof Error)) return error;
  switch (error.name) {
    case "NetworkError":
      return new ForgeError("unreachable", error.message, { host });
    case "TimeoutError":
      return new ForgeError("timeout", error.message, { host });
    default:
      return error;
  }
}

function said(body: string): string {
  try {
    const parsed = JSON.parse(body);
    const message = parsed?.message ?? parsed?.error_description ?? parsed?.error;
    if (typeof message === "string") return message;
    if (message != null) return JSON.stringify(message);
  } catch {
    // Not JSON: the text itself.
  }
  return body.trim().split("\n").slice(0, 6).join("\n");
}

/** A response that isn't 2xx, typed by its status and headers. */
export function failure(host: string, response: Response, body: string): ForgeError {
  const message = said(body);
  const limited =
    response.status === 429 || response.headers.get("ratelimit-remaining") === "0" || /rate limit/i.test(message);
  if ((response.status === 403 || response.status === 429) && limited) {
    return new ForgeError("rate-limited", message, { host });
  }
  if (response.status === 401 || response.status === 403) return new ForgeError("auth", message, { host });
  if (response.status === 404) return new ForgeError("not-found", message, { host });
  return new ForgeError(
    "failed",
    message === "" ? `GitLab answered ${response.status}` : `GitLab answered ${response.status}: ${message}`,
  );
}

async function send(
  api: Api,
  method: string,
  path: string,
  options: { fields?: [string, string][]; signal?: AbortSignal; stream?: boolean } = {},
): Promise<Response> {
  const body = options.fields === undefined ? null : JSON.stringify(Object.fromEntries(options.fields));
  try {
    return await http.fetch(`${api.base}/${path.replace(/^\/+/, "")}`, {
      method: method as "GET",
      headers: {
        "private-token": api.token,
        accept: "application/json",
        ...(body === null ? {} : { "content-type": "application/json" }),
      },
      body,
      signal: options.signal,
      timeoutMs: NETWORK_TIMEOUT_MS,
      stream: options.stream ?? false,
    });
  } catch (error) {
    throw waitError(api.host, error);
  }
}

function parse<T>(text: string): T {
  try {
    return JSON.parse(text) as T;
  } catch (error) {
    throw new ForgeError("failed", `GitLab returned something diffui couldn't read: ${(error as Error).message}`);
  }
}

/** `method path`, its JSON answer; `fields` go as a JSON body. */
export async function call<T>(
  api: Api,
  method: string,
  path: string,
  options: { fields?: [string, string][]; signal?: AbortSignal } = {},
): Promise<T> {
  const response = await send(api, method, path, options);
  const text = await response.text();
  if (!response.ok) throw failure(api.host, response, text);
  return parse<T>(text);
}

/**
 * `GET path`, its body read a piece at a time: for an answer that can be
 * big (a page of diffs), which a whole read would cap at 8 MB.
 */
export async function streamed<T>(api: Api, path: string, signal: AbortSignal): Promise<T> {
  const response = await send(api, "GET", path, { signal, stream: true });
  if (!response.ok) throw failure(api.host, response, await response.text());
  let text = "";
  try {
    for await (const piece of response.body ?? []) text += piece;
  } catch (error) {
    throw waitError(api.host, error);
  }
  return parse<T>(text);
}

/** Who the token belongs to. */
export async function login(api: Api): Promise<string> {
  return (await call<{ username: string }>(api, "GET", "user")).username;
}

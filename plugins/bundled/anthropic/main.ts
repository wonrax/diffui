// Anthropic's Messages API as an AI provider: one streamed request per
// completion, the text deltas handed to diffui as they arrive.
//
// A reply that stops for any reason but the end of its turn (a refusal, the
// token limit) is an error, never a description cut short.
import { AiError, ai, auth, config, http } from "diffui";
import type { AiRequest, Response } from "diffui";

const base = String(config.base_url).replace(/\/+$/, "");
const VERSION = "2026-10-21";
const MAX_TOKENS = 64_000;
/** How long the API may take to answer, and then to send each piece. */
const TIMEOUT_MS = 120_000;

async function key(): Promise<string> {
  const value = await auth.secret("api_key");
  if (!value) throw new AiError("auth", "There is no Anthropic API key.");
  return value;
}

function headers(apiKey: string): Record<string, string> {
  return { "x-api-key": apiKey, "anthropic-version": VERSION, "content-type": "application/json" };
}

/** A request that got no answer. */
function sendError(error: unknown): unknown {
  if (error instanceof Error && (error.name === "NetworkError" || error.name === "TimeoutError")) {
    return new AiError("unreachable", `Couldn't reach ${base}: ${error.message}`);
  }
  return error;
}

/** An error the API named, in its body or in a stream's `error` event. */
function apiError(type: string | undefined, message: string, status?: number): AiError {
  if (status === 401 || status === 403 || type === "authentication_error" || type === "permission_error") {
    return new AiError("auth", `Anthropic refused the API key: ${message}`);
  }
  if (status === 429 || status === 529 || type === "rate_limit_error" || type === "overloaded_error") {
    return new AiError("rate-limited", `Anthropic is busy: ${message}`);
  }
  return new AiError("failed", `Anthropic: ${message}`);
}

async function failure(res: Response): Promise<AiError> {
  let type: string | undefined;
  let message = `${res.status} ${res.statusText}`.trim();
  try {
    const body = JSON.parse(await res.text());
    type = body?.error?.type;
    message = body?.error?.message ?? message;
  } catch {
    // Not JSON: the status says enough.
  }
  return apiError(type, message, res.status);
}

/** The events of a server-sent event stream, as `{ event, data }`. */
async function* events(body: AsyncIterable<string>): AsyncGenerator<{ event: string; data: string }> {
  let buffer = "";
  const parse = (block: string) => {
    let event = "message";
    const data: string[] = [];
    for (const line of block.split(/\r?\n/)) {
      if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
    }
    return { event, data: data.join("\n") };
  };
  for await (const piece of body) {
    buffer += piece;
    let end;
    while ((end = buffer.search(/\r?\n\r?\n/)) >= 0) {
      const block = buffer.slice(0, end);
      buffer = buffer.slice(end).replace(/^\r?\n\r?\n/, "");
      if (block.trim()) yield parse(block);
    }
  }
  if (buffer.trim()) yield parse(buffer);
}

ai.register({
  async *complete(req: AiRequest, { signal }) {
    const content = [];
    if (req.input) content.push({ type: "text", text: req.input });
    content.push({ type: "text", text: req.prompt });
    const body: Record<string, unknown> = {
      model: req.model ?? config.model,
      max_tokens: MAX_TOKENS,
      stream: true,
      messages: [{ role: "user", content }],
    };
    if (req.system) body.system = req.system;
    let res;
    try {
      res = await http.fetch(`${base}/v1/messages`, {
        method: "POST",
        headers: headers(await key()),
        body: JSON.stringify(body),
        stream: true,
        timeoutMs: TIMEOUT_MS,
        signal,
      });
    } catch (error) {
      throw sendError(error);
    }
    if (!res.ok) throw await failure(res);
    let stop: string | null = null;
    let ended = false;
    try {
      for await (const { data } of events(res.body!)) {
        let message;
        try {
          message = JSON.parse(data);
        } catch {
          continue;
        }
        switch (message.type) {
          case "content_block_delta":
            if (message.delta?.type === "text_delta" && message.delta.text) yield message.delta.text;
            break;
          case "message_delta":
            stop = message.delta?.stop_reason ?? stop;
            break;
          case "message_stop":
            ended = true;
            break;
          case "error":
            throw apiError(message.error?.type, message.error?.message ?? "the stream failed");
        }
      }
    } catch (error) {
      throw sendError(error);
    }
    if (stop === "refusal") throw new AiError("failed", "Claude declined to answer this request.");
    if (stop === "max_tokens") throw new AiError("failed", `The reply ran past ${MAX_TOKENS} tokens and was cut off.`);
    if (!ended) throw new AiError("unreachable", "The connection to Anthropic closed before the reply ended.");
  },

  async models() {
    const apiKey = await key();
    const models = [];
    let after = "";
    for (let page = 0; page < 10; page++) {
      let res;
      try {
        res = await http.fetch(`${base}/v1/models?limit=1000${after ? `&after_id=${encodeURIComponent(after)}` : ""}`, {
          headers: headers(apiKey),
          timeoutMs: TIMEOUT_MS,
        });
      } catch (error) {
        throw sendError(error);
      }
      if (!res.ok) throw await failure(res);
      const listed = await res.json();
      for (const model of listed.data ?? []) models.push({ id: model.id, name: model.display_name ?? null });
      if (!listed.has_more || !listed.last_id) break;
      after = listed.last_id;
    }
    return models;
  },

  async account() {
    // A model list of one says whether the key works.
    const apiKey = await key();
    let res;
    try {
      res = await http.fetch(`${base}/v1/models?limit=1`, { headers: headers(apiKey), timeoutMs: TIMEOUT_MS });
    } catch (error) {
      throw sendError(error);
    }
    if (!res.ok) throw await failure(res);
    return { label: `The API key works, at ${base}` };
  },
});

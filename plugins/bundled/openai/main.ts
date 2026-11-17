// An OpenAI-compatible chat completions API as an AI provider: one streamed
// request per completion, each `choices[0].delta.content` handed to diffui as
// it arrives. The key is optional (a local server needs none); the model
// isn't, and no model name is assumed: servers differ.
import { AiError, ai, auth, config, http } from "diffui";
import type { AiRequest, Response } from "diffui";

const base = String(config.base_url).replace(/\/+$/, "");
/** How long the server may take to answer, and then to send each piece. */
const TIMEOUT_MS = 120_000;

async function headers(): Promise<Record<string, string>> {
  const key = await auth.secret("api_key");
  const sent: Record<string, string> = { "content-type": "application/json" };
  if (key) sent.authorization = `Bearer ${key}`;
  return sent;
}

function sendError(error: unknown): unknown {
  if (error instanceof Error && (error.name === "NetworkError" || error.name === "TimeoutError")) {
    return new AiError("unreachable", `Couldn't reach ${base}: ${error.message}`);
  }
  return error;
}

function apiError(message: string, status?: number): AiError {
  if (status === 401 || status === 403) return new AiError("auth", `${base} refused the API key: ${message}`);
  if (status === 429) return new AiError("rate-limited", `${base} is limiting requests: ${message}`);
  return new AiError("failed", message);
}

async function failure(res: Response): Promise<AiError> {
  let message = `${res.status} ${res.statusText}`.trim();
  try {
    const body = JSON.parse(await res.text());
    message = body?.error?.message ?? body?.message ?? message;
  } catch {
    // Not JSON: the status says enough.
  }
  return apiError(message, res.status);
}

/** The `data:` payloads of a server-sent event stream. */
async function* payloads(body: AsyncIterable<string>): AsyncGenerator<string> {
  let buffer = "";
  const parse = (block: string) =>
    block
      .split(/\r?\n/)
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).replace(/^ /, ""))
      .join("\n");
  for await (const piece of body) {
    buffer += piece;
    let end;
    while ((end = buffer.search(/\r?\n\r?\n/)) >= 0) {
      const block = buffer.slice(0, end);
      buffer = buffer.slice(end).replace(/^\r?\n\r?\n/, "");
      const data = parse(block);
      if (data) yield data;
    }
  }
  const data = parse(buffer);
  if (data) yield data;
}

ai.register({
  async *complete(req: AiRequest, { signal }) {
    const model = req.model ?? config.model;
    if (!model) throw new AiError("failed", "No model is set. Pick one in Settings.");
    const messages = [];
    if (req.system) messages.push({ role: "system", content: req.system });
    messages.push({ role: "user", content: req.input ? `${req.input}\n\n${req.prompt}` : req.prompt });
    let res;
    try {
      res = await http.fetch(`${base}/chat/completions`, {
        method: "POST",
        headers: await headers(),
        body: JSON.stringify({ model, messages, stream: true }),
        stream: true,
        timeoutMs: TIMEOUT_MS,
        signal,
      });
    } catch (error) {
      throw sendError(error);
    }
    if (!res.ok) throw await failure(res);
    let finish: string | null = null;
    let done = false;
    try {
      for await (const data of payloads(res.body!)) {
        if (data.trim() === "[DONE]") {
          done = true;
          break;
        }
        let chunk;
        try {
          chunk = JSON.parse(data);
        } catch {
          continue;
        }
        if (chunk.error) throw apiError(chunk.error.message ?? "the stream failed", Number(chunk.error.code) || undefined);
        const choice = chunk.choices?.[0];
        if (choice?.delta?.content) yield choice.delta.content;
        if (choice?.finish_reason) finish = choice.finish_reason;
      }
    } catch (error) {
      throw sendError(error);
    }
    if (finish === "length") throw new AiError("failed", "The reply ran past the model's token limit and was cut off.");
    if (finish === "content_filter") throw new AiError("failed", "The server's content filter stopped the reply.");
    if (!done && !finish) throw new AiError("unreachable", `The connection to ${base} closed before the reply ended.`);
  },

  async models() {
    let res;
    try {
      res = await http.fetch(`${base}/models`, { headers: await headers(), timeoutMs: TIMEOUT_MS });
    } catch (error) {
      throw sendError(error);
    }
    if (!res.ok) throw await failure(res);
    const listed = await res.json();
    return (listed.data ?? listed.models ?? [])
      .map((model: { id?: string; name?: string }) => ({ id: model.id ?? model.name ?? "", name: null }))
      .filter((model: { id: string }) => model.id)
      .sort((a: { id: string }, b: { id: string }) => a.id.localeCompare(b.id));
  },

  async account() {
    await this.models!();
    return { label: `The server answers, at ${base}` };
  },
});

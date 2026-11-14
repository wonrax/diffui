// Claude Code as an AI provider: `claude -p <prompt>` in the repository,
// what it is about on stdin.
//
// It streams: `--output-format stream-json --verbose
// --include-partial-messages` makes claude print one JSON event a line as it
// writes, and each `stream_event` whose event is a `content_block_delta`
// with a `text_delta` is handed to diffui as it comes. The `result` line
// ends it, and says whether it failed. A claude too old for those flags
// refuses them; then the plain `-p` text runs instead, all of it at the end
// (the provider's page says so).
import { AiError, ai, config, exec } from "diffui";
import type { AiRequest, ExecStream } from "diffui";

const program: string = config.program;

/** The program, by its file name, for messages. */
const display = program.split("/").pop() || "claude";

/** diffui waits this long between pieces; a plain `-p` writes only at the end. */
const TIMEOUT_MS = 600_000;

const STREAMING = ["--output-format", "stream-json", "--verbose", "--include-partial-messages"];

/** Couldn't start it. */
function startError(error: unknown): unknown {
  if (error instanceof Error && error.name === "NotFoundError") {
    return new AiError("missing-cli", `${display} isn't installed, or isn't at ${program}.`);
  }
  return error;
}

/** It ran and failed: what it said, typed. */
function failure(said: string, code: number | null): AiError {
  const line = said.trim().split("\n").filter((line) => line.trim()).pop()?.trim() ?? "";
  const message = line || `${display} exited with ${code ?? "a signal"}.`;
  if (/not logged in|please run \/login|invalid api key|authentication|unauthori[sz]ed|oauth token/i.test(message)) {
    return new AiError("auth", `${display} isn't signed in: ${message}`);
  }
  if (/rate limit|usage limit|too many requests|overloaded/i.test(message)) return new AiError("rate-limited", message);
  if (/ENOTFOUND|ECONNREFUSED|ETIMEDOUT|unable to connect|network/i.test(message)) return new AiError("unreachable", message);
  return new AiError("failed", message);
}

/** claude refused a flag, as commander words it: an old claude. */
const refusedFlags = (stderr: string) => /unknown option|unexpected argument/i.test(stderr);

/** Whole lines of `out`, as they arrive. */
async function* lines(out: ExecStream): AsyncGenerator<string> {
  let buffer = "";
  for await (const piece of out) {
    buffer += piece;
    let end;
    while ((end = buffer.indexOf("\n")) >= 0) {
      yield buffer.slice(0, end);
      buffer = buffer.slice(end + 1);
    }
  }
  if (buffer) yield buffer;
}

function argv(req: AiRequest, streaming: boolean): string[] {
  const words = [program, "-p"];
  if (streaming) words.push(...STREAMING);
  const model: string = req.model ?? config.model ?? "";
  if (model) words.push("--model", model);
  if (req.system) words.push("--append-system-prompt", req.system);
  words.push(req.prompt);
  return words;
}

async function start(req: AiRequest, streaming: boolean, signal: AbortSignal): Promise<ExecStream> {
  try {
    return await exec.stream(argv(req, streaming), {
      stdin: req.input ?? undefined,
      cwd: req.cwd ?? undefined,
      signal,
      timeoutMs: TIMEOUT_MS,
    });
  } catch (error) {
    throw startError(error);
  }
}

/** The plain `-p` text, for a claude that doesn't stream. */
async function* plain(req: AiRequest, signal: AbortSignal): AsyncGenerator<string> {
  const out = await start(req, false, signal);
  let said = "";
  for await (const piece of out) {
    said = (said + piece).slice(-4096);
    yield piece;
  }
  const { ok, code, stderr } = await out.ended;
  if (!ok) throw failure(stderr.trim() ? stderr : said, code);
}

interface Result {
  is_error?: boolean;
  subtype?: string;
  result?: string;
}

ai.register({
  async *complete(req: AiRequest, { signal }) {
    const out = await start(req, true, signal);
    let wrote = false;
    let result: Result | null = null;
    let other = "";
    for await (const line of lines(out)) {
      let event;
      try {
        event = JSON.parse(line);
      } catch {
        // Not an event: a warning, or text from a claude that ignored the flags.
        other = (other + line + "\n").slice(-4096);
        continue;
      }
      const inner = event?.type === "stream_event" ? event.event : null;
      if (inner?.type === "content_block_delta" && inner.delta?.type === "text_delta" && inner.delta.text) {
        wrote = true;
        yield inner.delta.text;
      } else if (event?.type === "result") {
        result = event;
      }
    }
    const { ok, code, stderr } = await out.ended;
    if (!ok && !wrote && !result && refusedFlags(stderr)) {
      yield* plain(req, signal);
      return;
    }
    if (result && (result.is_error || result.subtype !== "success")) {
      throw failure(result.result || result.subtype || stderr, code);
    }
    if (!ok) throw failure(stderr.trim() ? stderr : other, code);
    // A result without deltas still answers.
    if (!wrote && result?.result) yield result.result;
  },

  async models() {
    // Claude Code's own names; a full model id works in the field too.
    return [
      { id: "opus", name: "The latest Opus" },
      { id: "sonnet", name: "The latest Sonnet" },
      { id: "haiku", name: "The latest Haiku" },
    ];
  },

  async account() {
    let version;
    let help;
    try {
      version = await exec([program, "--version"], { timeoutMs: 15_000 });
      help = await exec([program, "--help"], { timeoutMs: 15_000 });
    } catch (error) {
      throw startError(error);
    }
    if (!version.ok) throw failure(version.stderr || version.stdout, version.code);
    const label = `${display} ${version.stdout.trim()}`;
    const streams = help.stdout.includes("--include-partial-messages");
    return { label: streams ? label : `${label}: streaming needs a newer claude` };
  },
});

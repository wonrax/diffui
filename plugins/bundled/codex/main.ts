// Codex as an AI provider: `codex exec --json <prompt>` in the repository,
// what it is about on stdin.
//
// `--json` prints one event a line. The agent's reply is taken from them as
// it comes: `agent_message_delta` pieces where codex sends them, else each
// finished `agent_message` (codex 0.4x's `item.completed` events carry a
// whole message). `turn.failed` and `error` events end it with their
// message. A codex too old for `--json` refuses it; then plain `codex exec`
// runs instead and its stdout is the reply.
//
// `--skip-git-repo-check`: a jj repository that isn't colocated has no
// `.git` at its root, and codex refuses to run outside a git repository
// without it. Nothing here asks codex to change files.
import { AiError, ai, config, exec } from "diffui";
import type { AiRequest, ExecStream } from "diffui";

const program: string = config.program;

const display = program.split("/").pop() || "codex";

const TIMEOUT_MS = 600_000;

function startError(error: unknown): unknown {
  if (error instanceof Error && error.name === "NotFoundError") {
    return new AiError("missing-cli", `${display} isn't installed, or isn't at ${program}.`);
  }
  return error;
}

function failure(said: string, code: number | null): AiError {
  const line = said.trim().split("\n").filter((line) => line.trim()).pop()?.trim() ?? "";
  const message = line || `${display} exited with ${code ?? "a signal"}.`;
  if (/not logged in|codex login|unauthori[sz]ed|401|invalid api key|authentication/i.test(message)) {
    return new AiError("auth", `${display} isn't signed in: ${message}`);
  }
  if (/rate limit|usage limit|too many requests|429/i.test(message)) return new AiError("rate-limited", message);
  if (/stream disconnected|ENOTFOUND|ECONNREFUSED|unable to connect|network/i.test(message)) return new AiError("unreachable", message);
  return new AiError("failed", message);
}

const refusedFlags = (stderr: string) => /unexpected argument|unknown option/i.test(stderr);

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

function argv(req: AiRequest, json: boolean): string[] {
  const words = [program, "exec", "--skip-git-repo-check"];
  if (json) words.push("--json");
  const model: string = req.model ?? config.model ?? "";
  if (model) words.push("--model", model);
  // codex has no system prompt of its own here: it leads the prompt.
  words.push(req.system ? `${req.system}\n\n${req.prompt}` : req.prompt);
  return words;
}

async function start(req: AiRequest, json: boolean, signal: AbortSignal): Promise<ExecStream> {
  try {
    return await exec.stream(argv(req, json), {
      stdin: req.input ?? undefined,
      cwd: req.cwd ?? undefined,
      signal,
      timeoutMs: TIMEOUT_MS,
    });
  } catch (error) {
    throw startError(error);
  }
}

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

/** The event codex's two JSON shapes put the payload in. */
function payload(event: any): any {
  return event?.msg ?? event;
}

ai.register({
  async *complete(req: AiRequest, { signal }) {
    const out = await start(req, true, signal);
    let wrote = false;
    let streamed = false;
    let failed: string | null = null;
    let other = "";
    for await (const line of lines(out)) {
      let event;
      try {
        event = payload(JSON.parse(line));
      } catch {
        other = (other + line + "\n").slice(-4096);
        continue;
      }
      switch (event?.type) {
        case "agent_message_delta":
          if (event.delta) {
            streamed = wrote = true;
            yield event.delta;
          }
          break;
        case "agent_message":
          // The whole message, unless its pieces came already.
          if (!streamed && event.message) {
            yield (wrote ? "\n\n" : "") + event.message;
            wrote = true;
          }
          break;
        case "item.completed":
          if (event.item?.type === "agent_message" && event.item.text) {
            yield (wrote ? "\n\n" : "") + event.item.text;
            wrote = true;
          }
          break;
        case "turn.failed":
          failed = event.error?.message ?? "the turn failed";
          break;
        case "error":
          failed = event.message ?? "codex failed";
          break;
      }
    }
    const { ok, code, stderr } = await out.ended;
    if (!ok && !wrote && !failed && refusedFlags(stderr)) {
      yield* plain(req, signal);
      return;
    }
    if (failed) throw failure(failed, code);
    if (!ok) throw failure(stderr.trim() ? stderr : other, code);
  },

  async account() {
    let version;
    let help;
    try {
      version = await exec([program, "--version"], { timeoutMs: 15_000 });
      help = await exec([program, "exec", "--help"], { timeoutMs: 15_000 });
    } catch (error) {
      throw startError(error);
    }
    if (!version.ok) throw failure(version.stderr || version.stdout, version.code);
    const label = version.stdout.trim() || display;
    return { label: help.stdout.includes("--json") ? label : `${label}: streaming needs a newer codex` };
  },
});

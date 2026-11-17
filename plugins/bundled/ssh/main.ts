// ssh as a transport: `ssh -T <destination> <the server>`, its stdin and
// stdout the stream.
//
// ssh hands its command to the user's shell over there as one line, so the
// server's words are quoted for a shell. With `login_shell` (the default) the
// line runs in a login shell of the user's own, `exec $SHELL -lc '…'`: a
// command over ssh otherwise gets the bare PATH, without what a profile adds
// for cargo or nix. The quoting is POSIX's; fish reads it the same way.
//
// diffui has nowhere to show a password or passphrase prompt, so ssh runs
// with BatchMode: it signs in with a key or an agent, or fails saying so.
import { config, files, transport } from "diffui";
import type { FoundHost, Launch, TransportExit, TransportHost } from "diffui";

const program: string = config.program;

/** One word for a shell. A leading `~/` stays the home folder. */
function quote(word: string): string {
  if (word.startsWith("~/")) return `"$HOME"${quote(word.slice(1))}`;
  if (/^[A-Za-z0-9_\/.:=@%+,-]+$/.test(word)) return word;
  return `'${word.replaceAll("'", `'\\''`)}'`;
}

/** The server as a shell line that becomes it: `exec env K=V … argv…`. */
function serverLine(host: TransportHost): string {
  const env = Object.entries(host.server.env).map(([name, value]) => `${name}=${value}`);
  const words = env.length > 0 ? ["env", ...env, ...host.server.argv] : [...host.server.argv];
  return `exec ${words.map(quote).join(" ")}`;
}

function destination(host: TransportHost): string {
  return String(host.options.destination || host.name);
}

function connect(host: TransportHost): Launch {
  const line = serverLine(host);
  const remote = host.options.login_shell === false ? line : `exec "$SHELL" -lc ${quote(line)}`;
  const argv = [
    program,
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "Compression=yes",
    // A dead network is noticed in under a minute instead of at TCP's leisure.
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
  ];
  if (host.options.port) argv.push("-p", String(host.options.port));
  // `--`: a destination can't be read as an option.
  argv.push("--", destination(host), remote);
  return { argv };
}

/** The `Host`s of ~/.ssh/config that name one machine (no patterns). */
async function hosts(): Promise<FoundHost[]> {
  const text = await files.read("~/.ssh/config").catch(() => "");
  const found = new Set<string>();
  for (const line of text.split("\n")) {
    const match = /^\s*Host\s+(.+?)\s*$/i.exec(line);
    if (!match) continue;
    for (const name of match[1].split(/\s+/)) {
      if (!/[*?!]/.test(name)) found.add(name.replace(/^"(.*)"$/, "$1"));
    }
  }
  return [...found].map((name) => ({ name }));
}

function explain(host: TransportHost, exit: TransportExit): string | null {
  const said = exit.stderr;
  const to = destination(host);
  const server = host.server.argv[0] ?? "diffui-serve";
  if (exit.code === 127 || /command not found|: not found|No such file or directory/i.test(said)) {
    return `${host.name} has no ${server} to run. Install diffui-serve there, or set this host's server to where it is.`;
  }
  if (/Permission denied|Too many authentication failures/i.test(said)) {
    return `ssh couldn't sign in to ${to} without asking. diffui never prompts for a password or passphrase: add the key to your agent (ssh-add), then reconnect.`;
  }
  if (/Host key verification failed/i.test(said)) {
    return `ssh doesn't know ${to}'s host key. Connect once in a terminal (ssh ${to}) to accept it, then reconnect.`;
  }
  if (exit.code === 255) {
    const last = said.trim().split("\n").pop()?.trim();
    return last ? `Couldn't reach ${host.name}: ${last}` : `Couldn't reach ${host.name}.`;
  }
  return null;
}

transport.register({ connect, hosts, explain });

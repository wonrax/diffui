// WSL as a transport: `wsl.exe --distribution <name> --exec /bin/sh …`, its
// stdin and stdout the stream.
//
// `--` would hand the command to the user's own shell, which may be nushell
// or anything else that doesn't read sh. `--exec` starts /bin/sh directly,
// so the user's shell never parses a word of it. The server travels in an
// environment variable (WSLENV carries it across), quoted for sh, and sh
// runs `eval exec "$DIFFUI_SERVE"`: nothing needs quoting on the Windows
// side, where wsl.exe's rebuilding of a command line isn't to be leaned on.
//
// `sh -l` reads /etc/profile and ~/.profile, which is where nix and rustup
// put their PATH. A PATH only the user's interactive shell sets (.zshrc, a
// fish or nu config) isn't seen: set the host's server to a full path then.
import { config, exec, transport } from "diffui";
import type { FoundHost, Launch, Located, TransportExit, TransportHost } from "diffui";

const program: string = config.program;

/** One word for a shell. A leading `~/` stays the home folder. */
function quote(word: string): string {
  if (word.startsWith("~/")) return `"$HOME"${quote(word.slice(1))}`;
  if (/^[A-Za-z0-9_\/.:=@%+,-]+$/.test(word)) return word;
  return `'${word.replaceAll("'", `'\\''`)}'`;
}

function distribution(host: TransportHost): string {
  return String(host.options.distribution || host.name);
}

function connect(host: TransportHost): Launch {
  const env = Object.entries(host.server.env).map(([name, value]) => `${name}=${value}`);
  const words = env.length > 0 ? ["env", ...env, ...host.server.argv] : [...host.server.argv];
  const argv = [program, "--distribution", distribution(host)];
  if (host.options.user) argv.push("--user", String(host.options.user));
  const login = host.options.login_shell !== false;
  argv.push("--cd", "~", "--exec", "/bin/sh", login ? "-lc" : "-c", 'eval exec "$DIFFUI_SERVE"');
  return {
    argv,
    env: {
      DIFFUI_SERVE: words.map(quote).join(" "),
      // `/u`: from Windows into WSL only.
      WSLENV: "DIFFUI_SERVE/u",
      WSL_UTF8: "1",
    },
  };
}

/** wsl.exe writes UTF-16 unless WSL_UTF8 is honoured: drop what that leaves. */
function plain(text: string): string {
  return text.replaceAll("\0", "").replaceAll("\r", "");
}

/** The installed distributions. None off Windows, where there is no wsl.exe. */
async function hosts(): Promise<FoundHost[]> {
  const listed = await exec([program, "--list", "--quiet"], { env: { WSL_UTF8: "1" }, timeoutMs: 10_000 }).catch(
    () => null,
  );
  if (!listed?.ok) return [];
  return plain(listed.stdout)
    .split("\n")
    .map((name) => name.trim())
    .filter((name) => name && !name.startsWith("docker-desktop"))
    .map((name) => ({ name }));
}

function explain(host: TransportHost, exit: TransportExit): string | null {
  const said = plain(exit.stderr);
  const server = host.server.argv[0] ?? "diffui-serve";
  if (/WSL_E_DISTRO_NOT_FOUND|no distribution with the supplied name/i.test(said)) {
    return `WSL has no distribution named ${distribution(host)}. \`wsl --list\` shows the ones installed.`;
  }
  if (exit.code === 127 || /command not found|: not found|No such file or directory|Unknown command/i.test(said)) {
    return `${host.name} has no ${server} to run. Install diffui-serve inside it, or set this host's server to where it is.`;
  }
  return null;
}

/** `\\wsl.localhost\Ubuntu\home\me` is `/home/me` on `Ubuntu`. */
function locate(path: string): Located | null {
  const match = /^\\\\(?:wsl\.localhost|wsl\$)\\([^\\]+)(.*)$/i.exec(path);
  if (!match) return null;
  return { host: match[1], path: match[2].replaceAll("\\", "/") || "/" };
}

transport.register({ connect, hosts, explain, locate });

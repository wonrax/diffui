// WSL as a transport: `wsl.exe --distribution <name> -- <the server>`, its
// stdin and stdout the stream.
//
// wsl.exe hands what follows `--` to the user's shell inside as one line.
// How it puts that line back together from a Windows command line isn't
// something to lean on, so the server never goes on the command line: it
// travels in an environment variable (WSLENV carries it across), quoted for
// a shell, and the command line is only `eval exec $DIFFUI_SERVE`. Nothing
// in it needs quoting on the Windows side, and sh, bash, zsh and fish all
// read it the same way.
//
// `--shell-type login` makes that shell a login shell, for the PATH a
// profile adds (cargo, nix). It needs the WSL from the Microsoft Store
// (`wsl --update`); the one built into older Windows doesn't know the flag.
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
  if (host.options.login_shell !== false) argv.push("--shell-type", "login");
  argv.push("--cd", "~", "--", "eval", "exec", "$DIFFUI_SERVE");
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
  if (/shell-type|Invalid command line/i.test(said)) {
    return "This WSL is too old for diffui to start a login shell in. Run `wsl --update` in a terminal, or turn off the host's login shell setting.";
  }
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

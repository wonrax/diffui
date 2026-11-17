//! `diffui-serve`: what diffui talks to when a repository is on another
//! machine.
//!
//! diffui starts it over there through the host's transport (`ssh`,
//! `wsl.exe`, …) and speaks to it on stdin and stdout; it serves until stdin
//! closes. It is never installed by diffui: how it gets onto a machine, and
//! which one a host's `server` setting names, is the user's choice.

use std::io::IsTerminal as _;
use std::process::ExitCode;

use diffui_core::remote::serve;

const USAGE: &str = "\
diffui-serve: the far end of diffui's remote repositories.

diffui starts it on the machine a repository is on and talks to it on stdin
and stdout. Put it where that host's `server` setting in diffui says (the
default is `diffui-serve` on the PATH a login shell gets).

Usage:
  diffui-serve             serve on stdin and stdout until stdin closes
  diffui-serve --version   the version and protocol a diffui has to match
  diffui-serve jj <args>   the jj CLI built in (what typed jj commands run)";

fn main() -> ExitCode {
    // A typed jj command runs this binary as `jj` (see the core's
    // passthrough): be the jj CLI.
    if let Some(code) = diffui_core::cli::passthrough::jj_main() {
        return ExitCode::from(code);
    }
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        // `serve` is the app's own subcommand; the same word works here, so
        // a host's `server` can name either binary.
        None | Some("serve") => {}
        Some("--version" | "-V") => {
            println!("diffui-serve {}", serve::version());
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("diffui-serve: unknown argument `{other}`\n\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    if std::io::stdin().is_terminal() {
        // Someone ran it by hand: it would sit here waiting for frames.
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    match std::env::current_exe() {
        Ok(exe) => serve::serve_stdio(exe),
        Err(error) => {
            eprintln!("diffui-serve: can't find its own executable: {error}");
            ExitCode::FAILURE
        }
    }
}

//! Running the real jj CLI for the commands diffui doesn't run itself.
//!
//! The CLI is diffui's own binary: started with `argv[0]` = `jj` it runs
//! jj-cli 0.45.1's `CliRunner` (see [`jj_main`]), which links the same jj-lib
//! diffui embeds. So the CLI always matches the repo format diffui writes,
//! whatever `jj` is on the user's `PATH`, and it reads the user's jj config
//! the way the real CLI does.

use std::ffi::OsStr;
use std::path::Path;

/// The jj-cli version linked in, which `jj version` reports.
pub const JJ_VERSION: &str = "0.45.1";

/// Set in a passthrough child's environment: the CLI then writes plain text
/// (no colour, no pager) whatever the user's `ui.color` says.
const PASSTHROUGH_ENV: &str = "DIFFUI_JJ_PASSTHROUGH";

/// The app's first call in `main`, before anything else is set up.
///
/// - Started as `jj` (`argv[0]`'s file name): run jj-cli and return its exit
///   code. This is how passthrough children run.
/// - Started as `diffui jj <args…>`: re-execute as `jj <args…>`, so jj's
///   usage and error text say `jj`. Returns only if that fails.
/// - Otherwise `None`: carry on as the GUI.
pub fn jj_main() -> Option<u8> {
    let mut args = std::env::args_os();
    let argv0 = args.next()?;
    if Path::new(&argv0).file_name() == Some(OsStr::new("jj")) {
        return Some(run_cli());
    }
    if args.next().as_deref() != Some(OsStr::new("jj")) {
        return None;
    }
    Some(reexec_as_jj(args))
}

#[cfg(unix)]
fn reexec_as_jj(args: std::env::ArgsOs) -> u8 {
    use std::os::unix::process::CommandExt as _;
    let error = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe).arg0("jj").args(args).exec(),
        Err(error) => error,
    };
    eprintln!("diffui: failed to start jj: {error}");
    1
}

#[cfg(not(unix))]
fn reexec_as_jj(_args: std::env::ArgsOs) -> u8 {
    eprintln!("diffui: `diffui jj` needs a unix platform");
    1
}

fn run_cli() -> u8 {
    let runner = jj_cli::cli_util::CliRunner::init().version(JJ_VERSION);
    let runner = if std::env::var_os(PASSTHROUGH_ENV).is_some() {
        runner.add_dispatch_hook(async |ui, helper, dispatch| {
            // Plain output for the GUI, without putting `--color=never
            // --no-pager` into the args jj records in the op log.
            let mut config = helper.settings().config().clone();
            let mut layer =
                jj_lib::config::ConfigLayer::empty(jj_lib::config::ConfigSource::CommandArg);
            layer
                .set_value("ui.color", "never")
                .expect("a plain key takes a string");
            layer
                .set_value("ui.paginate", "never")
                .expect("a plain key takes a string");
            config.add_layer(layer);
            ui.reset(&config)?;
            dispatch.call(ui, helper).await
        })
    } else {
        runner
    };
    runner.run()
}

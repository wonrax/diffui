//! The transport kind, and the two transports diffui ships, from memory as
//! the app runs them. What `connect` answers is started here the way the app
//! starts it, against scripts standing in for `ssh` and `wsl.exe` that do
//! what the real ones do with their command line: hand it to a shell. So the
//! quoting is checked by a shell, not by eye.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::path::{Path, PathBuf};

use diffui_plugins::protocol::{
    FoundHost, Launch, Located, PluginCall, ServerCommand, TransportCall, TransportExit,
    TransportHost,
};
use diffui_plugins::{BUNDLED, PluginError};
use serde_json::{Value, json};

use crate::support::{Setup, TestPlugin, scratch};

/// The bundled transport `id` with `config` (a `[plugins.<id>]` body).
fn bundled(id: &str, config: &str) -> TestPlugin {
    let (_, files) = BUNDLED
        .iter()
        .find(|(bundled, _)| *bundled == id)
        .expect("a bundled transport");
    let manifest = files
        .iter()
        .find(|(name, _)| *name == "plugin.toml")
        .map(|(_, text)| text.to_string())
        .unwrap();
    let mut setup = Setup::with_manifest(manifest, "");
    setup.files = files
        .iter()
        .filter(|(name, _)| *name != "plugin.toml")
        .map(|(name, text)| (*name, text.to_string()))
        .collect();
    let plugin = setup.config(config).spawn_bundled();
    let registration = plugin.loaded();
    assert!(registration.transport.is_some(), "{registration:?}");
    plugin
}

fn host(name: &str, options: Value, argv: &[&str], env: &[(&str, &str)]) -> TransportHost {
    TransportHost {
        name: name.to_owned(),
        options: options.as_object().cloned().unwrap_or_default(),
        server: ServerCommand {
            argv: argv.iter().map(|word| word.to_string()).collect(),
            env: env
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        },
    }
}

fn ask<T: serde::de::DeserializeOwned + Send + 'static>(
    plugin: &TestPlugin,
    call: TransportCall,
) -> Result<T, PluginError> {
    plugin.call(PluginCall::Transport { call })
}

fn connect(plugin: &TestPlugin, host: TransportHost) -> Launch {
    ask(plugin, TransportCall::Connect { host }).expect("connect answers")
}

fn explain(
    plugin: &TestPlugin,
    host: &TransportHost,
    code: Option<i32>,
    stderr: &str,
) -> Option<String> {
    ask(
        plugin,
        TransportCall::Explain {
            host: host.clone(),
            exit: TransportExit {
                code,
                stderr: stderr.to_owned(),
            },
        },
    )
    .expect("explain answers")
}

/// An executable script at `dir/name`.
#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Start `launch` the way the app does and return what it printed.
#[cfg(unix)]
fn run(launch: &Launch, home: &Path) -> String {
    let output = std::process::Command::new(&launch.argv[0])
        .args(&launch.argv[1..])
        .envs(&launch.env)
        .env("HOME", home)
        .env("SHELL", "/bin/sh")
        .output()
        .expect("start the transport's program");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A server whose words need every kind of quoting, printed one per line by
/// the far side: what arrives must be what was sent.
const AWKWARD: [&str; 6] = [
    "printf",
    "%s\\n",
    "it's",
    "two words",
    "$HOME;`id`",
    "~/bin/x",
];

#[cfg(unix)]
#[test]
fn ssh_hands_the_server_to_the_far_shell_word_for_word() {
    let dir = scratch("ssh");
    // What ssh does with its last argument: the far side's shell runs it.
    let ssh = script(
        &dir,
        "ssh",
        r#"for last; do :; done; exec /bin/sh -c "$last""#,
    );
    let plugin = bundled("ssh", &format!("program = {:?}", ssh.to_string_lossy()));

    for login_shell in [true, false] {
        let launch = connect(
            &plugin,
            host(
                "box",
                json!({ "destination": "me@box.example", "port": 2222, "login_shell": login_shell }),
                &AWKWARD,
                &[("GREETING", "hello there")],
            ),
        );
        assert_eq!(launch.argv[0], ssh.to_string_lossy());
        let words = &launch.argv[1..];
        assert!(words.contains(&"-T".to_owned()), "{words:?}");
        assert!(
            words.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]),
            "{words:?}"
        );
        assert!(
            words.windows(2).any(|pair| pair == ["-p", "2222"]),
            "{words:?}"
        );
        let at = words
            .iter()
            .position(|word| word == "--")
            .expect("a -- before the destination");
        assert_eq!(words[at + 1], "me@box.example");
        assert_eq!(words.len(), at + 3, "the server is one argument: {words:?}");

        let printed = run(&launch, &dir);
        let home = dir.to_string_lossy();
        assert_eq!(
            printed.lines().collect::<Vec<_>>(),
            ["it's", "two words", "$HOME;`id`", &format!("{home}/bin/x")],
            "login_shell = {login_shell}"
        );
    }

    // The environment reaches the server, not ssh.
    let launch = connect(
        &plugin,
        host(
            "box",
            json!({}),
            &["sh", "-c", "echo $GREETING"],
            &[("GREETING", "hello there")],
        ),
    );
    assert!(launch.env.is_empty());
    assert_eq!(run(&launch, &dir).trim(), "hello there");
    // Without a destination the host's name is it.
    let at = launch.argv.iter().position(|word| word == "--").unwrap();
    assert_eq!(launch.argv[at + 1], "box");
}

#[test]
fn ssh_says_what_a_failed_start_means() {
    let plugin = bundled("ssh", "");
    let host = host(
        "box",
        json!({ "destination": "me@box" }),
        &["diffui-serve"],
        &[],
    );
    let missing = explain(
        &plugin,
        &host,
        Some(127),
        "bash: line 1: exec: diffui-serve: not found\n",
    )
    .unwrap();
    assert!(
        missing.starts_with("box has no diffui-serve to run."),
        "{missing}"
    );
    let denied = explain(
        &plugin,
        &host,
        Some(255),
        "me@box: Permission denied (publickey).\n",
    )
    .unwrap();
    assert!(
        denied.contains("ssh-add") && denied.contains("never prompts"),
        "{denied}"
    );
    let unknown = explain(&plugin, &host, Some(255), "Host key verification failed.\n").unwrap();
    assert!(unknown.contains("ssh me@box"), "{unknown}");
    let down = explain(
        &plugin,
        &host,
        Some(255),
        "ssh: connect to host box port 22: Connection refused\n",
    )
    .unwrap();
    assert_eq!(
        down,
        "Couldn't reach box: ssh: connect to host box port 22: Connection refused"
    );
    assert_eq!(explain(&plugin, &host, Some(1), "something else\n"), None);
}

#[cfg(unix)]
#[test]
fn wsl_carries_the_server_in_the_environment_and_evals_it_inside() {
    let dir = scratch("wsl");
    // What wsl.exe does with what follows `--`: the user's shell inside runs
    // it as one line, the variables WSLENV names carried across.
    let wsl = script(
        &dir,
        "wsl.exe",
        r#"case "$WSLENV" in DIFFUI_SERVE/u) ;; *) echo "WSLENV is $WSLENV" >&2; exit 9;; esac
while [ "$1" != "--" ]; do shift; done; shift
exec /bin/sh -c "$*""#,
    );
    let plugin = bundled("wsl", &format!("program = {:?}", wsl.to_string_lossy()));

    let launch = connect(
        &plugin,
        host(
            "Ubuntu",
            json!({ "user": "me" }),
            &AWKWARD,
            &[("GREETING", "hello there")],
        ),
    );
    let words: Vec<&str> = launch.argv[1..].iter().map(String::as_str).collect();
    assert_eq!(
        words,
        [
            "--distribution",
            "Ubuntu",
            "--user",
            "me",
            "--shell-type",
            "login",
            "--cd",
            "~",
            "--",
            "eval",
            "exec",
            "$DIFFUI_SERVE",
        ]
    );
    // Nothing on the command line needs quoting on the Windows side.
    assert!(
        words
            .iter()
            .all(|word| !word.contains([' ', '"', '\'', '\\'])),
        "{words:?}"
    );
    assert_eq!(launch.env.get("WSL_UTF8").map(String::as_str), Some("1"));

    let printed = run(&launch, &dir);
    let home = dir.to_string_lossy();
    assert_eq!(
        printed.lines().collect::<Vec<_>>(),
        ["it's", "two words", "$HOME;`id`", &format!("{home}/bin/x")]
    );

    let plain = connect(
        &plugin,
        host(
            "Work",
            json!({ "distribution": "Debian", "login_shell": false }),
            &["diffui-serve"],
            &[],
        ),
    );
    assert_eq!(
        plain.argv[1..],
        [
            "--distribution",
            "Debian",
            "--cd",
            "~",
            "--",
            "eval",
            "exec",
            "$DIFFUI_SERVE"
        ]
    );
    assert_eq!(
        plain.env.get("DIFFUI_SERVE").map(String::as_str),
        Some("diffui-serve")
    );
}

#[cfg(unix)]
#[test]
fn wsl_lists_distributions_and_reads_explorer_paths() {
    let dir = scratch("wsl-hosts");
    // UTF-16's leftovers and CRLF, as an old wsl.exe prints them.
    let wsl = script(
        &dir,
        "wsl.exe",
        r#"printf 'U\0b\0u\0n\0t\0u\0\r\nDebian\r\ndocker-desktop\r\n\r\n'"#,
    );
    let plugin = bundled("wsl", &format!("program = {:?}", wsl.to_string_lossy()));
    let found: Vec<FoundHost> = ask(&plugin, TransportCall::Hosts).unwrap();
    let names: Vec<&str> = found.iter().map(|host| host.name.as_str()).collect();
    assert_eq!(names, ["Ubuntu", "Debian"]);

    let locate = |path: &str| -> Option<Located> {
        ask(
            &plugin,
            TransportCall::Locate {
                path: path.to_owned(),
            },
        )
        .unwrap()
    };
    let inside = locate(r"\\wsl.localhost\Ubuntu\home\me\src\repo").unwrap();
    assert_eq!(
        (inside.host.as_str(), inside.path.as_str()),
        ("Ubuntu", "/home/me/src/repo")
    );
    let old = locate(r"\\WSL$\Debian").unwrap();
    assert_eq!((old.host.as_str(), old.path.as_str()), ("Debian", "/"));
    assert_eq!(locate(r"C:\Users\me\repo"), None);

    // No wsl.exe (any machine but Windows): no hosts, not an error.
    let absent = bundled("wsl", "program = \"/nonexistent/wsl.exe\"");
    let found: Vec<FoundHost> = ask(&absent, TransportCall::Hosts).unwrap();
    assert!(found.is_empty());
}

#[test]
fn wsl_says_what_a_failed_start_means() {
    let plugin = bundled("wsl", "");
    let host = host("Ubuntu", json!({}), &["diffui-serve"], &[]);
    let missing = explain(
        &plugin,
        &host,
        Some(127),
        "zsh:1: command not found: diffui-serve\n",
    )
    .unwrap();
    assert!(
        missing.starts_with("Ubuntu has no diffui-serve to run."),
        "{missing}"
    );
    let old = explain(
        &plugin,
        &host,
        Some(1),
        "Invalid command line option: --shell-type\n",
    )
    .unwrap();
    assert!(old.contains("wsl --update"), "{old}");
    let gone = explain(
        &plugin,
        &host,
        Some(1),
        "There is no distribution with the supplied name.\r\nError code: Wsl/Service/WSL_E_DISTRO_NOT_FOUND\r\n",
    )
    .unwrap();
    assert!(gone.contains("no distribution named Ubuntu"), "{gone}");
    assert_eq!(explain(&plugin, &host, Some(1), ""), None);
}

const TRANSPORT: &str = r#"
id = "t"
name = "Test"
version = "0.0.1"
api = 0
main = "main.js"

[permissions]
exec = [["reach"]]
env = ["REACH_TOKEN"]

[provides.transport]
local_roots = ['\\far\']
"#;

#[test]
fn what_connect_answers_is_held_to_the_manifests_permissions() {
    let main = r#"
        import { transport } from "diffui";
        transport.register({
          connect(host) {
            if (host.name === "sneaky") return { argv: ["rm", "-rf", "/"] };
            if (host.name === "leaky") return { argv: ["reach", "x"], env: { PATH: "/tmp" } };
            if (host.name === "empty") return { argv: [] };
            if (host.name === "unset") throw new Error("Set a destination for this host.");
            return { argv: ["reach", host.name, ...host.server.argv], env: { REACH_TOKEN: "t" } };
          },
        });
    "#;
    let plugin = Setup::with_manifest(TRANSPORT.to_owned(), main).spawn();
    let registration = plugin.loaded();
    assert_eq!(
        registration.transport.as_deref(),
        Some(&["connect".to_owned()][..])
    );
    assert_eq!(
        plugin
            .plugin
            .manifest()
            .transport
            .as_ref()
            .unwrap()
            .local_roots,
        [r"\\far\"]
    );

    let named = |name: &str| host(name, json!({}), &["diffui-serve"], &[]);
    let fine = connect(&plugin, named("box"));
    assert_eq!(fine.argv, ["reach", "box", "diffui-serve"]);
    assert_eq!(
        fine.env,
        BTreeMap::from([("REACH_TOKEN".to_owned(), "t".to_owned())])
    );

    let refused = |name: &str| -> String {
        match ask::<Launch>(&plugin, TransportCall::Connect { host: named(name) }) {
            Err(PluginError::BadReply(why)) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    };
    assert!(
        refused("sneaky").contains("[permissions] exec"),
        "{}",
        refused("sneaky")
    );
    assert!(
        refused("leaky").contains("can't set PATH"),
        "{}",
        refused("leaky")
    );
    assert!(
        refused("empty").contains("empty argv"),
        "{}",
        refused("empty")
    );
    // What the script throws is the user's to read.
    match ask::<Launch>(
        &plugin,
        TransportCall::Connect {
            host: named("unset"),
        },
    ) {
        Err(PluginError::Threw(error)) => {
            assert_eq!(error.message, "Set a destination for this host.")
        }
        other => panic!("expected the script's error, got {other:?}"),
    }
    // A method it didn't register isn't called.
    match ask::<Vec<FoundHost>>(&plugin, TransportCall::Hosts) {
        Err(PluginError::NotRegistered(what)) => assert_eq!(what, "transport method hosts"),
        other => panic!("expected NotRegistered, got {other:?}"),
    }
}

#[test]
fn a_transport_must_be_declared_and_registered() {
    let undeclared = Setup::new(
        "",
        r#"import { transport } from "diffui"; transport.register({ connect() {} });"#,
    )
    .spawn();
    assert!(undeclared.failed().contains("[provides.transport]"));

    let unregistered = Setup::with_manifest(TRANSPORT.to_owned(), "").spawn();
    assert!(
        unregistered
            .failed()
            .contains("never called transport.register"),
        "a declared transport needs its provider"
    );
}

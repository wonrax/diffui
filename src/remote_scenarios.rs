//! Repositories on another machine in the real app (the offscreen harness):
//! a host from config.toml whose server runs in this process, so the tab, the
//! open prompt and the lost-connection strip are driven end to end.

use diffui_core::remote::HostId;

use crate::connections::HostState;
use crate::harness::{DEFAULT_SIZE, Harness, fixture_repo};
use crate::remote::in_process;
use crate::{Action, Diffui, LoadStatus};

/// One host, `box`, served in this process.
fn config() -> String {
    format!("[hosts.box]\ncommand = [{:?}]\n", in_process::COMMAND)
}

fn open(config: &str) -> Harness {
    Harness::open_fresh(None, DEFAULT_SIZE, config, "")
}

fn loaded(ui: &Diffui) -> bool {
    let session = &ui.active().session;
    !ui.tabs.is_empty()
        && !session.commits.is_empty()
        && matches!(session.status, LoadStatus::Loaded)
}

/// What a tab on `root` is known by: the host and the root as the server
/// resolves it.
fn key(root: &std::path::Path) -> String {
    format!("box:{}", root.canonicalize().unwrap().display())
}

#[test]
fn a_repository_on_a_host_opens_as_a_tab_and_loads() {
    let root = fixture_repo();
    let mut h = open(&config());
    let target = format!("box:{}/src", root.display());
    h.act(|ui, cx| ui.open_repository(&target, cx));
    h.wait_until("the remote tab's graph", loaded);
    h.settle();

    h.read(|ui| {
        let tab = &ui.tabs[ui.active];
        // Known by its root over there, whatever was typed to open it.
        assert_eq!(tab.key(), Some(key(&root)));
        assert_eq!(tab.root(), None, "nothing of it is on this disk");
        assert_eq!(tab.owner, "box");
        assert_eq!(
            Some(tab.name.as_str()),
            root.file_name().and_then(|name| name.to_str())
        );
        let state = &tab.state;
        assert!(state.local_root().is_none());
        assert!(
            state.session.capabilities.mutate,
            "the server said what it can do"
        );
        assert!(
            !state.default_revset().is_empty(),
            "jj's default revset came with the answer"
        );
        assert_eq!(state.session.revset, state.default_revset());
        let remote = state.remote.as_ref().expect("a remote tab");
        assert!(remote.lost.is_none() && remote.connecting.is_none());
        assert!(!state.session.document.files.is_empty() || state.session.diff_in_flight());

        // The session and the recents know it by `host:root`.
        let saved = ui.current_window_state();
        assert_eq!(saved.open_repos, [key(&root)]);
        assert_eq!(saved.active_repo, Some(key(&root)));
        assert_eq!(ui.recent_repos.first(), Some(&key(&root)));
        let state = ui.ctl_state();
        assert_eq!(state["repos"][0]["host"], "box");

        // What runs a program in the working tree is off, and says why.
        assert!(!crate::notes::app::view_available(ui));
    });

    // Opening it again, by its root or from inside it, finds the tab.
    h.act(|ui, cx| ui.open_repository(&key(&root), cx));
    h.settle();
    h.read(|ui| assert_eq!(ui.tabs.len(), 1));
}

#[test]
fn a_lost_connection_shows_on_the_tab_and_reconnects() {
    let root = fixture_repo();
    let mut h = open(&config());
    h.act(|ui, cx| ui.open_repository(&key(&root), cx));
    h.wait_until("the remote tab's graph", loaded);
    h.settle();

    in_process::stop();
    h.wait_until("the tab to hear its link is gone", |ui| {
        ui.active()
            .remote
            .as_ref()
            .is_some_and(|remote| remote.lost.is_some())
    });
    h.read(|ui| {
        let state = ui.active();
        let lost = state.remote.as_ref().unwrap().lost.as_ref().unwrap();
        assert!(lost.is_transient());
        assert!(lost.to_string().contains("box"), "{lost}");
        // What was on screen stays there.
        assert!(matches!(state.session.status, LoadStatus::Loaded));
        assert!(!state.session.commits.is_empty());
    });

    h.perform(Action::ReconnectHost);
    h.wait_until("the tab to be answered again", |ui| {
        let state = ui.active();
        state.handle.is_some()
            && state
                .remote
                .as_ref()
                .is_some_and(|remote| remote.lost.is_none())
    });
    h.wait_until("the graph after reconnecting", loaded);
}

#[test]
fn a_host_nobody_knows_and_a_path_without_a_repository_say_so() {
    let mut h = open(&config());
    h.act(|ui, cx| ui.open_repository("nowhere:/srv/repo", cx));
    h.wait_until("the open to fail", |ui| {
        ui.open_repo_dialog()
            .is_some_and(|dialog| dialog.error.is_some())
    });
    h.read(|ui| {
        let error = ui.open_repo_dialog().unwrap().error.clone().unwrap();
        assert!(error.starts_with("No host named nowhere."), "{error}");
        assert!(ui.tabs.is_empty());
    });

    h.act(|ui, cx| ui.open_repository("box:/", cx));
    h.wait_until("the host to say there is no repository", |ui| {
        ui.open_repo_dialog()
            .and_then(|dialog| dialog.error.as_deref())
            .is_some_and(|error| error.starts_with("No jj or git repository at / on box."))
    });
}

#[test]
fn the_open_prompt_browses_a_host_and_opens_from_it() {
    let root = fixture_repo();
    let parent = root.parent().unwrap().display().to_string();
    let name = root.file_name().unwrap().to_string_lossy().into_owned();
    let mut h = open(&config());

    h.read(|ui| {
        let rows = ui.open_repo_host_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].name.as_str(), rows[0].via.as_str()),
            ("box", "command")
        );
        assert!(rows[0].configured && rows[0].problem.is_none());
    });

    h.perform(Action::OpenRepoHosts);
    h.read(|ui| {
        assert!(ui.open_repo_dialog().unwrap().picking_host);
        // This machine, then the host.
        assert_eq!(ui.open_repo_row_count(), 2);
    });
    h.perform(Action::OpenRepoHost(Some("box".to_owned())));
    h.read(|ui| {
        let dialog = ui.open_repo_dialog().unwrap();
        assert!(!dialog.picking_host);
        assert_eq!(
            dialog.host.as_ref().map(|host| host.0.as_str()),
            Some("box")
        );
        assert_eq!(dialog.path, "~/", "a host is browsed from its home");
    });

    // Its folders, read over the link: the repository is flagged.
    h.act(|ui, cx| ui.set_open_repo_path(format!("{parent}/"), cx));
    h.wait_until("the host's folder listing", |ui| {
        ui.open_repo_dialog()
            .and_then(|dialog| dialog.suggestions())
            .is_some_and(|(_, rows)| {
                rows.iter()
                    .any(|entry| entry.name == name && entry.vcs.is_some())
            })
    });

    h.act(|ui, cx| ui.set_open_repo_path(format!("{parent}/{name}"), cx));
    h.wait_until("the status to say it is a repository", |ui| {
        matches!(
            ui.open_repo_status(),
            Some(crate::path_browse::Status::Repository {
                at_root: true,
                open: false,
                ..
            })
        )
    });
    h.perform(Action::SubmitRepoDialog);
    h.wait_until("the remote tab's graph", loaded);
    h.read(|ui| {
        assert_eq!(ui.tabs[ui.active].key(), Some(key(&root)));
        assert!(
            ui.open_repo_dialog().is_none(),
            "the prompt closed on the open"
        );
    });

    // Back to this machine: the path starts over, and browses this disk.
    h.perform(Action::OpenRepoDialog);
    h.perform(Action::OpenRepoHost(Some("box".to_owned())));
    h.perform(Action::OpenRepoHost(None));
    h.read(|ui| {
        let dialog = ui.open_repo_dialog().unwrap();
        assert!(dialog.host.is_none() && dialog.link.is_none());
        assert_eq!(dialog.path, "");
    });
}

#[test]
fn a_remote_tab_is_restored_from_the_session() {
    let root = fixture_repo();
    let saved = {
        let mut h = open(&config());
        h.act(|ui, cx| ui.open_repository(&key(&root), cx));
        h.wait_until("the remote tab's graph", loaded);
        h.read(|ui| ui.current_window_state().open_repos)
    };
    assert_eq!(saved, [key(&root)]);
    // The next launch: the tab is there before its host is asked anything,
    // and loads once the server answers.
    let mut h = Harness::open_restoring(saved);
    h.read(|ui| {
        assert_eq!(ui.tabs.len(), 1);
        assert_eq!(ui.tabs[0].key(), Some(key(&root)));
        assert_eq!(ui.tabs[0].owner, "box");
    });
    h.wait_until("the restored tab's graph", loaded);
    h.read(|ui| assert!(!ui.active().default_revset().is_empty()));
}

/// `settings.toml` as the harness's app last wrote it.
fn settings_file(h: &mut Harness) -> String {
    let path = h.read(|ui| {
        ui.config_files
            .settings_path
            .clone()
            .expect("a settings file")
    });
    std::fs::read_to_string(path).unwrap_or_default()
}

fn host(action: crate::host_page::HostAction) -> Action {
    Action::Host(action)
}

fn state(ui: &Diffui, host: &str) -> Option<HostState> {
    ui.host_state(&HostId(host.to_owned()))
}

fn up(ui: &Diffui, host: &str) -> bool {
    matches!(state(ui, host), Some(HostState::Up(_)))
}

/// `box`, a host whose program says something and never answers, and one
/// whose program fails.
fn three_hosts() -> String {
    format!(
        "{}\n[hosts.slow]\ncommand = [\"sh\", \"-c\", \"echo building diffui-serve >&2; sleep 60\"]\n\n\
         [hosts.broken]\ncommand = [\"sh\", \"-c\", \"echo first >&2; echo sh: diffui-serve: not found >&2; exit 127\"]\n",
        config()
    )
}

#[test]
fn a_connection_outlives_the_prompt_that_asked_for_it() {
    let mut h = open(&three_hosts());
    h.perform(Action::OpenRepoDialog);
    h.perform(Action::OpenRepoHost(Some("slow".to_owned())));
    h.wait_until("its program to say something", |ui| {
        matches!(
            state(ui, "slow"),
            Some(HostState::Connecting { said: Some(said), .. }) if said == "building diffui-serve"
        )
    });
    // The prompt turning away (or closing) doesn't end what it started.
    h.perform(Action::OpenRepoHost(None));
    h.settle();
    h.read(|ui| {
        let dialog = ui.open_repo_dialog().expect("the prompt");
        assert!(dialog.host.is_none() && dialog.link.is_none());
        assert!(matches!(
            state(ui, "slow"),
            Some(HostState::Connecting { .. })
        ));
        let transcript = ui.host_transcript(&HostId("slow".to_owned())).unwrap();
        assert!(transcript.command.unwrap().starts_with("sh -c"));
        assert_eq!(transcript.output, "building diffui-serve\n");
    });

    // Stopping it is hanging up: nothing is left of it.
    h.perform(host(crate::host_page::HostAction::Open("slow".to_owned())));
    h.perform(host(crate::host_page::HostAction::Disconnect));
    h.read(|ui| {
        assert!(state(ui, "slow").is_none());
        assert!(ui.host_transcript(&HostId("slow".to_owned())).is_none());
    });
    h.perform(Action::CloseSettings);

    // One that came up stays up with the prompt closed, and the prompt
    // finds it again.
    h.perform(Action::OpenRepoDialog);
    h.perform(Action::OpenRepoHost(Some("box".to_owned())));
    h.wait_until("box to answer", |ui| up(ui, "box"));
    h.perform(Action::OpenRepoHost(None));
    h.settle();
    h.read(|ui| assert!(up(ui, "box")));
}

#[test]
fn a_failed_start_says_why_and_keeps_what_was_run_and_written() {
    let mut h = open(&three_hosts());
    h.act(|ui, _| ui.toasts.clear());
    // Asked for with nothing on screen to say how it went: a toast does.
    h.act(|ui, cx| {
        ui.host_link(&HostId("broken".to_owned()), cx);
    });
    h.wait_until("the start to fail", |ui| {
        matches!(state(ui, "broken"), Some(HostState::Failed(_)))
    });
    h.read(|ui| {
        let Some(HostState::Failed(why)) = state(ui, "broken") else {
            unreachable!()
        };
        assert!(
            why.starts_with("broken has no diffui-serve to run."),
            "{why}"
        );
        let transcript = ui.host_transcript(&HostId("broken".to_owned())).unwrap();
        assert!(transcript.command.unwrap().starts_with("sh -c"));
        assert_eq!(transcript.output, "first\nsh: diffui-serve: not found\n");
        assert_eq!(ui.toasts.len(), 1, "the failure is said");
    });

    // Trying again from its page starts over, and fails the same way.
    h.perform(host(crate::host_page::HostAction::Open(
        "broken".to_owned(),
    )));
    h.perform(host(crate::host_page::HostAction::Connect));
    h.read(|ui| assert!(!matches!(state(ui, "broken"), Some(HostState::Failed(_)))));
    h.wait_until("the second start to fail", |ui| {
        matches!(state(ui, "broken"), Some(HostState::Failed(_)))
    });
    h.read(|ui| assert_eq!(ui.toasts.len(), 1, "Settings shows this one itself"));
}

#[test]
fn settings_makes_a_host_saves_what_is_typed_and_tests_it() {
    use crate::host_page::HostAction;
    let mut h = open(&config());
    h.perform(Action::OpenSettings);
    h.perform(host(HostAction::NewName("a b".to_owned())));
    h.perform(host(HostAction::Add));
    h.read(|ui| {
        assert!(ui.remote.page.is_none(), "a name with a space is no host");
        assert_eq!(ui.remote.new_host, "a b", "what was typed stays");
    });

    h.perform(host(HostAction::NewName(" devbox ".to_owned())));
    h.perform(host(HostAction::Add));
    h.read(|ui| {
        assert_eq!(ui.remote.page.as_ref().unwrap().name, "devbox");
        assert!(ui.remote.new_host.is_empty());
        assert!(
            ui.open_repo_host_rows()
                .iter()
                .any(|row| row.name == "devbox")
        );
    });
    assert!(settings_file(&mut h).contains("[hosts.devbox]"));

    // Typed text is saved on Return, and when the page is left.
    h.perform(host(HostAction::Draft {
        key: "server".to_owned(),
        text: "nix run 'github:me/diffui#diffui-serve' --".to_owned(),
    }));
    assert!(
        !settings_file(&mut h).contains("server"),
        "not while typing"
    );
    h.perform(host(HostAction::Commit("server".to_owned())));
    h.perform(host(HostAction::Draft {
        key: "env".to_owned(),
        text: "JJ_CONFIG=/etc/jj.toml".to_owned(),
    }));
    h.perform(host(HostAction::Back));
    let saved = settings_file(&mut h);
    assert!(
        saved.contains("server = [\"nix\", \"run\", \"github:me/diffui#diffui-serve\", \"--\"]"),
        "{saved}"
    );
    assert!(saved.contains("JJ_CONFIG = \"/etc/jj.toml\""), "{saved}");

    // An environment that isn't one stays in its field and is said.
    h.perform(host(HostAction::Open("devbox".to_owned())));
    h.act(|ui, _| ui.toasts.clear());
    h.perform(host(HostAction::Draft {
        key: "env".to_owned(),
        text: "oops".to_owned(),
    }));
    h.perform(host(HostAction::Commit("env".to_owned())));
    h.read(|ui| assert_eq!(ui.toasts.len(), 1));
    assert!(settings_file(&mut h).contains("JJ_CONFIG"));
    h.perform(host(HostAction::Draft {
        key: "env".to_owned(),
        text: String::new(),
    }));
    h.perform(host(HostAction::Draft {
        key: "server".to_owned(),
        text: String::new(),
    }));

    // Reached by a command: the one whose server is this process.
    // Connecting saves what is typed first.
    h.perform(host(HostAction::Reach(None)));
    h.perform(host(HostAction::Draft {
        key: "command".to_owned(),
        text: format!("'{}'", in_process::COMMAND),
    }));
    h.perform(host(HostAction::Connect));
    h.wait_until("devbox to answer", |ui| up(ui, "devbox"));
    let saved = settings_file(&mut h);
    assert!(
        saved.contains(&format!("command = [{:?}]", in_process::COMMAND)),
        "{saved}"
    );
    assert!(
        !saved.contains("transport") && !saved.contains("JJ_CONFIG"),
        "{saved}"
    );

    // Removed, it is no host any more and the list is back.
    h.perform(host(HostAction::Remove));
    h.read(|ui| {
        assert!(ui.remote.page.is_none());
        assert!(
            !ui.open_repo_host_rows()
                .iter()
                .any(|row| row.name == "devbox")
        );
    });
    assert!(!settings_file(&mut h).contains("devbox"));
}

#[test]
fn settings_adds_to_a_config_toml_host_and_resets_it() {
    use crate::host_page::HostAction;
    let mut h = open(&config());
    h.perform(host(HostAction::Open("box".to_owned())));
    h.read(|ui| assert!(ui.mode(crate::ModeKind::Settings).is_some()));
    h.perform(host(HostAction::Draft {
        key: "env".to_owned(),
        text: "A=1".to_owned(),
    }));
    // Closing Settings leaves the page too.
    h.perform(Action::CloseSettings);
    let saved = settings_file(&mut h);
    assert!(
        saved.contains("[hosts.box.env]\nA = \"1\"") || saved.contains("env = { A = \"1\" }"),
        "{saved}"
    );
    h.read(|ui| {
        let table = &ui.host_tables()["box"];
        assert!(table.contains_key("command"), "config.toml's stays");
        assert!(table.contains_key("env"));
        assert!(ui.remote.problems.is_empty(), "{:?}", ui.remote.problems);
    });

    // It still opens, with what Settings added.
    h.perform(host(HostAction::Open("box".to_owned())));
    h.perform(host(HostAction::Connect));
    h.wait_until("box to answer", |ui| up(ui, "box"));
    // Its settings changing lets go of a connection nothing uses: the next
    // one is made the new way.
    h.perform(host(HostAction::Draft {
        key: "env".to_owned(),
        text: "A=2".to_owned(),
    }));
    h.perform(host(HostAction::Commit("env".to_owned())));
    h.read(|ui| assert!(state(ui, "box").is_none()));

    // Reset takes Settings' part away; the host is config.toml's again.
    h.perform(host(HostAction::Remove));
    h.read(|ui| {
        assert_eq!(ui.remote.page.as_ref().unwrap().name, "box");
        assert!(!ui.host_tables()["box"].contains_key("env"));
    });
    assert!(!settings_file(&mut h).contains("hosts"));
}

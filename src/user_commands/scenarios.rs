//! Declared commands and `diffui ctl` in the real app on the fixture
//! repository.

use std::path::Path;

use futures::channel::oneshot;
use serde_json::Value;

use crate::commands::CommandArg;
use crate::ctl::Request;
use crate::harness::{Harness, fixture_repo};
use diffui_core::RevisionSelection;

fn commit_of(repo: &Path, name: &str) -> String {
    let manifest =
        std::fs::read_to_string(repo.join(".jj/fixture-manifest")).expect("the manifest");
    manifest
        .lines()
        .find_map(|line| {
            let mut parts = line.split(' ');
            let names = parts.next()?;
            let commit = parts.nth(1)?;
            names
                .split(',')
                .any(|it| it == name)
                .then(|| commit.to_owned())
        })
        .unwrap_or_else(|| panic!("no {name} in the manifest"))
}

fn open(config: &str) -> (Harness, std::path::PathBuf) {
    let repo = fixture_repo();
    let mut h = Harness::open_fresh(Some(&repo), (1280.0, 800.0), config, "");
    h.wait_for_graph();
    (h, repo)
}

/// A fake agent CLI: keeps what it was given on stdin beside itself and
/// writes a description. Written after the harness opens, which clears its
/// home.
fn fake_agent(name: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = crate::harness::scratch_home().join("repos");
    std::fs::create_dir_all(&dir).expect("a bin dir");
    let script = dir.join(name);
    std::fs::write(
        &script,
        "#!/bin/sh\ncat > \"$0.stdin\"\nprintf '%s\\n' \"$2\" > \"$0.prompt\"\n\
         printf 'Fix the parser crash\\n\\nIt no longer panics on an empty file.\\n\\n'\n",
    )
    .expect("write the script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make it executable");
    script
}

#[test]
fn generate_writes_the_agents_description_into_the_editor() {
    let script = fake_agent("fake-agent");
    let config = format!(
        "[ai]\ncommand = [{:?}, \"-p\"]\n",
        script.display().to_string()
    );
    let (mut h, repo) = open(&config);
    let revision = RevisionSelection::Commit(commit_of(&repo, "fix/parser-crash"));
    h.perform(crate::Action::SelectRevision(revision.clone()));
    h.wait_until("the revision", |ui| {
        ui.active().session.selected_revision == revision
    });
    h.act(|ui, cx| ui.run_command("ai.describe", CommandArg::Revision(revision.clone()), cx));
    h.wait_until("the agent's description", |ui| {
        ui.active().description_editor().is_some_and(|editor| {
            editor
                .generating
                .as_ref()
                .is_some_and(|generation| !generation.running)
        })
    });
    let content = h.read(|ui| ui.active().description_editor().unwrap().content.clone());
    assert_eq!(
        content,
        "Fix the parser crash\n\nIt no longer panics on an empty file."
    );
    let stdin = std::fs::read_to_string(script.with_extension("stdin")).expect("stdin kept");
    assert!(stdin.starts_with("diff --git"), "{stdin}");
    let prompt = std::fs::read_to_string(script.with_extension("prompt")).expect("prompt kept");
    assert!(prompt.contains("---"), "recent descriptions: {prompt}");
}

#[test]
fn a_choose_prompt_answers_into_the_run() {
    let (mut h, repo) = open(
        r#"
[[commands]]
id = "user.mark"
label = "Mark"
run = "jj bookmark create {prompt.name} -r {commit_id}"

[[commands.prompts]]
name = "name"
kind = "choose"
label = "Bookmark"
choices = ["picked-one", "picked-two"]
"#,
    );
    let revision = RevisionSelection::Commit(commit_of(&repo, "no-description"));
    h.perform(crate::Action::SelectRevision(revision.clone()));
    h.wait_until("the revision", |ui| {
        ui.active().session.selected_revision == revision
    });
    h.act(|ui, cx| ui.run_command("user.mark", CommandArg::None, cx));
    h.settle();
    let choices = h.read(|ui| {
        let column = ui
            .palette()
            .and_then(|state| state.top())
            .expect("a column");
        column.matches.len()
    });
    assert_eq!(choices, 2);
    h.press("down");
    h.press("enter");
    h.wait_until("the bookmark", |ui| {
        ui.active().session.bookmarks.entry("picked-two").is_some()
    });
}

fn ask(h: &mut Harness, request: Request) -> Value {
    let (reply, answer) = oneshot::channel();
    h.act(|ui, cx| ui.on_ctl_request(request, reply, cx));
    let mut answer = answer;
    let mut value = None;
    h.wait_until("the answer", |_| {
        if value.is_none() {
            value = answer.try_recv().ok().flatten();
        }
        value.is_some()
    });
    value.expect("answered")
}

#[test]
fn ctl_reads_state_runs_jj_and_commands() {
    let (mut h, repo) = open("");
    let state = ask(&mut h, Request::State);
    assert_eq!(state["ok"], true, "{state}");
    let root = state["repos"][0]["root"].as_str().expect("a root");
    assert_eq!(
        std::fs::canonicalize(root).ok(),
        std::fs::canonicalize(&repo).ok()
    );
    assert_eq!(state["repos"][0]["kind"], "jj");

    let did = ask(
        &mut h,
        Request::Jj {
            args: vec!["describe".into(), "-m".into(), "from ctl".into()],
            repo: Some(repo.display().to_string()),
        },
    );
    assert_eq!(did["ok"], true, "{did}");
    h.wait_until("the description", |ui| {
        ui.active()
            .session
            .commits
            .working_copy()
            .is_some_and(|row| row.description().trim() == "from ctl")
    });
    let refused = ask(
        &mut h,
        Request::Jj {
            args: vec![
                "rebase".into(),
                "-r".into(),
                "no-such-rev".into(),
                "-o".into(),
                "@".into(),
            ],
            repo: None,
        },
    );
    assert_eq!(refused["ok"], false, "{refused}");

    let ran = ask(
        &mut h,
        Request::Run {
            id: "theme.dark".into(),
            repo: None,
        },
    );
    assert_eq!(ran["ok"], true, "{ran}");
    assert_eq!(
        h.read(|ui| ui.selected_theme),
        crate::theme::ThemePreference::Dark
    );
    let unknown = ask(
        &mut h,
        Request::Run {
            id: "no.such".into(),
            repo: None,
        },
    );
    assert_eq!(unknown["ok"], false);

    let commands = ask(&mut h, Request::Commands { repo: None });
    let listed = commands["commands"].as_array().expect("a list");
    assert!(
        listed
            .iter()
            .any(|command| command["id"] == "ai.describe" && command["declared"] == true)
    );
}

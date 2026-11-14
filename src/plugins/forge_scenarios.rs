//! A plugin's forge in the app, end to end: the test forge of
//! `fixtures/plugins` (a full provider, pull request tab included) against
//! its fake CLI, installed in the plugins folder or as a bundled plugin.
//! Also what being bundled means: no prompt, the off switch, a folder plugin
//! of the same id replacing it, `eject`, and who claims a host first.

use std::path::{Path, PathBuf};

use diffui_core::RevisionSelection;
use diffui_core::forge::ForgePr;

use super::forge::tests::{fixture_dir, install_cli};
use super::{Origin, State};
use crate::Diffui;
use crate::harness::{Harness, scratch_home};

const SIZE: (f32, f32) = (1280.0, 800.0);

/// Where the test forge's program goes, and the config.toml pointing it there.
fn program() -> PathBuf {
    scratch_home().join("testforge-bin/testforge")
}

fn config() -> String {
    format!(
        "[plugins.testforge]\nprogram = \"{}\"\n",
        program().display()
    )
}

/// The permissions `settings.toml` records for the test forge once allowed.
const ALLOWED: &str = "[plugins.testforge]\nallowed = true\n\n[plugins.testforge.permissions]\nexec = [[\"{config.program}\"]]\nenv = [\"TESTFORGE_HOST\"]\n";

/// Copy the test forge's files into `dir`.
fn copy_plugin(dir: &Path, rename: Option<&str>) {
    std::fs::create_dir_all(dir).expect("make the plugin's folder");
    for file in ["plugin.toml", "main.js"] {
        let mut text = std::fs::read_to_string(fixture_dir().join("testforge").join(file))
            .expect("the fixture");
        if let Some(name) = rename {
            text = text.replace("name = \"Test forge\"", &format!("name = \"{name}\""));
        }
        std::fs::write(dir.join(file), text).expect("write the plugin");
    }
}

#[derive(Clone, Copy)]
enum Install {
    Bundled,
    /// Bundled, and a folder copy of the same id under another name.
    Both,
    /// Bundled beside the build's own GitHub plugin.
    WithGitHub,
}

/// The build's GitHub plugin into `dir`: the bundled set a test lays out
/// replaces the build's.
fn copy_github(dir: &Path) {
    let github = super::bundled::built_in()
        .into_iter()
        .find(|plugin| plugin.id == "github")
        .expect("GitHub is bundled");
    std::fs::create_dir_all(dir).expect("make the plugin's folder");
    for (name, text) in github.files {
        std::fs::write(dir.join(name), text).expect("write the plugin");
    }
}

/// The app with the test forge installed, on `repo` or the welcome screen.
fn launch(repo: Option<&Path>, install: Install, config: &str, settings: &str) -> Harness {
    let mut h = Harness::open_prepared(repo, SIZE, config, settings, |home| {
        install_cli(&home.join("testforge-bin"));
        copy_plugin(&home.join(".bundled-test/testforge"), None);
        match install {
            Install::Bundled => {}
            Install::Both => copy_plugin(
                &home.join(".config/diffui/plugins/testforge"),
                Some("My test forge"),
            ),
            Install::WithGitHub => copy_github(&home.join(".bundled-test/github")),
        }
    });
    if repo.is_some() {
        h.wait_for_graph();
    }
    h
}

fn state(ui: &Diffui, id: &str) -> Option<State> {
    ui.plugins.get(id).map(|found| found.state.clone())
}

fn wait_running(h: &mut Harness, id: &'static str) {
    h.wait_until("the plugin to load", |ui| {
        let state = state(ui, id);
        assert!(!matches!(state, Some(State::Failed(_))), "{state:?}");
        matches!(state, Some(State::Running(_)))
    });
}

fn claimed(ui: &Diffui, url: &str) -> Option<String> {
    ui.forge
        .forges
        .claim(url)
        .map(|remote| remote.kind.name().to_owned())
}

fn open_pr(h: &mut Harness, link: &str) -> ForgePr {
    let pr = h
        .read(|ui| ui.forge.forges.parse_pr(link))
        .expect("the link names a pull request");
    h.act(|ui, cx| ui.open_pull_request(pr.clone(), cx));
    pr
}

/// A bundled forge runs without asking, claims its hosts (and the one its
/// `hosts()` adds), opens its links, and its pull request opens in a tab:
/// commits as rows, the diff streamed in and parsed file by file, a commit's
/// own diff on selecting it.
#[test]
fn a_bundled_plugin_forges_pull_request_opens_in_a_tab() {
    let mut h = launch(None, Install::WithGitHub, &config(), "");
    wait_running(&mut h, "testforge");
    h.read(|ui| {
        let found = ui.plugins.get("testforge").unwrap();
        assert_eq!(found.origin, Origin::Bundled);
        assert!(ui.plugins.reviews.is_empty(), "nothing asked");
    });
    h.wait_until("hosts() to answer", |ui| {
        claimed(ui, "git@git.corp.example:team/app.git").is_some()
    });
    h.read(|ui| {
        assert_eq!(
            claimed(ui, "https://testforge.invalid/team/app.git").as_deref(),
            Some("Test forge")
        );
        assert_eq!(
            claimed(ui, "git@ci.testforge.example:team/app.git").as_deref(),
            Some("Test forge")
        );
        // github.com stays GitHub's.
        assert_eq!(
            claimed(ui, "git@github.com:team/app.git").as_deref(),
            Some("GitHub")
        );
    });

    let pr = open_pr(&mut h, "https://testforge.invalid/team/app/pull/1/files");
    assert_eq!(pr.kind.id(), "testforge");
    assert_eq!(pr.label(), "app#1");
    // A reference without a host means the forge's default host when no
    // other forge writes them that way first; with the host it is this one.
    let shorthand = h.read(|ui| ui.forge.forges.parse_pr("testforge.invalid/team/app#1"));
    assert_eq!(shorthand.map(|pr| pr.kind), Some(pr.kind));

    h.wait_until("the pull request's diff", |ui| {
        let session = &ui.active().session;
        // The commits (the graph) and the diff load side by side.
        !session.diff_in_flight() && session.document.files.len() == 3 && session.commits.len() == 3
    });
    h.read(|ui| {
        let session = &ui.active().session;
        let paths: Vec<&str> = session
            .document
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, ["a.txt", "b.txt", "c.txt"]);
        // The header's counts, not the files' sum.
        assert_eq!(
            (
                session.document.total_additions,
                session.document.total_deletions
            ),
            (3, 1)
        );
        let rows: Vec<&str> = session
            .commits
            .iter()
            .map(|row| row.description())
            .collect();
        assert_eq!(rows, ["All changes", "Second", "First"]);
    });

    h.act(|ui, _| {
        let effects = ui
            .active_mut()
            .session
            .load_diff(RevisionSelection::Commit("aaa111".into()));
        let tab = ui.active_tab_id().expect("the tab");
        ui.run_effects(tab, effects);
    });
    h.wait_until("the commit's diff", |ui| {
        let session = &ui.active().session;
        !session.diff_in_flight()
            && session
                .document
                .files
                .first()
                .map(|file| file.path.as_str())
                == Some("one.txt")
    });
}

/// Closing a pull request tab while its diff still streams stops the
/// plugin's program; a pull request the forge refuses says why in the app's
/// words.
#[test]
fn a_closed_tab_stops_the_plugins_diff_and_a_refusal_reads_well() {
    let mut h = launch(None, Install::Bundled, &config(), "");
    wait_running(&mut h, "testforge");
    open_pr(&mut h, "https://testforge.invalid/team/app/pull/2");
    let pids = scratch_home().join("testforge-bin/pids");
    h.wait_until("the endless diff to start", |_| pids.exists());
    let pid = std::fs::read_to_string(&pids).unwrap().trim().to_owned();
    h.act(|ui, cx| {
        let tab = ui.active_tab_id().expect("the tab");
        ui.close_tab(tab, cx);
    });
    let alive = |pid: &str| {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .is_ok_and(|status| status.success())
    };
    h.wait_until("the program to stop", |_| !alive(&pid));

    open_pr(&mut h, "https://testforge.invalid/team/app/pull/3");
    h.wait_until("the refusal", |ui| {
        ui.toasts.iter().any(|toast| {
            toast
                .detail
                .to_string()
                .contains("testforge isn't signed in to testforge.invalid")
        })
    });
}

/// `[plugins.<id>] enabled = false` turns a bundled plugin off; a folder
/// plugin of the same id replaces it (asked about like any other), and
/// claims its hosts first.
#[test]
fn a_bundled_plugin_turns_off_and_a_folder_plugin_replaces_it() {
    let off = "[plugins.testforge]\nenabled = false\n";
    let mut h = launch(None, Install::Bundled, &config(), off);
    h.read(|ui| {
        assert_eq!(state(ui, "testforge"), Some(State::Disabled));
        assert_eq!(claimed(ui, "https://testforge.invalid/team/app"), None);
    });
    drop(h);

    // Not allowed yet: it asks, and the bundled one doesn't stand in.
    let mut h = launch(None, Install::Both, &config(), "");
    h.read(|ui| {
        let found = ui.plugins.get("testforge").unwrap();
        assert_eq!(found.origin, Origin::Replaces);
        assert_eq!(found.name, "My test forge");
        assert_eq!(found.state, State::NotAllowed);
        assert_eq!(
            ui.plugins
                .found
                .iter()
                .filter(|f| f.id == "testforge")
                .count(),
            1
        );
    });
    drop(h);

    let mut h = launch(None, Install::Both, &config(), ALLOWED);
    wait_running(&mut h, "testforge");
    h.read(|ui| {
        assert_eq!(
            claimed(ui, "https://testforge.invalid/team/app").as_deref(),
            Some("My test forge")
        );
    });
}

/// A plugin from the folder can't take a host a bundled forge claims; it
/// gets it once the bundled forge is off.
#[test]
fn a_bundled_forge_claims_before_a_folder_one() {
    let rival = |home: &Path| {
        let dir = home.join(".config/diffui/plugins/rival");
        let manifest = std::fs::read_to_string(fixture_dir().join("testforge/plugin.toml"))
            .unwrap()
            .replace("id = \"testforge\"", "id = \"rival\"")
            .replace("name = \"Test forge\"", "name = \"Rival\"")
            .replace("hosts = [", "hosts = [\"git.rival.example\", ");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
        std::fs::copy(fixture_dir().join("testforge/main.js"), dir.join("main.js")).unwrap();
    };
    let config = format!(
        "{}\n[plugins.rival]\nprogram = \"{}\"\n",
        config(),
        program().display()
    );
    let allowed = ALLOWED.replace("testforge", "rival");
    for (settings, expected) in [
        (allowed.clone(), "Test forge"),
        (
            format!("{allowed}\n[plugins.testforge]\nenabled = false\n"),
            "Rival",
        ),
    ] {
        let mut h = Harness::open_prepared(None, SIZE, &config, &settings, |home| {
            install_cli(&home.join("testforge-bin"));
            copy_plugin(&home.join(".bundled-test/testforge"), None);
            rival(home);
        });
        wait_running(&mut h, "rival");
        h.read(|ui| {
            assert_eq!(
                claimed(ui, "https://testforge.invalid/team/app").as_deref(),
                Some(expected)
            );
            // The rival's own host is the rival's either way.
            assert_eq!(
                claimed(ui, "https://git.rival.example/team/app").as_deref(),
                Some("Rival")
            );
        });
    }
}

/// `diffui plugin eject` copies a bundled plugin into the plugins folder,
/// with the API's types; the copy replaces the bundled one at the next look.
#[test]
fn eject_copies_a_bundled_plugin_to_edit() {
    let mut h = launch(None, Install::Bundled, &config(), "");
    wait_running(&mut h, "testforge");
    let folder = h
        .read(|ui| ui.plugins.dir.clone())
        .expect("a plugins folder");
    let said = super::bundled::eject("testforge", &folder).expect("ejected");
    assert!(said.contains("plugins/testforge"), "{said}");
    for file in ["plugin.toml", "main.js", "diffui.d.ts", "jsconfig.json"] {
        assert!(folder.join("testforge").join(file).is_file(), "{file}");
    }
    assert!(
        super::bundled::eject("testforge", &folder).is_err(),
        "twice"
    );
    assert!(
        super::bundled::eject("nope", &folder)
            .unwrap_err()
            .contains("there are testforge")
    );
    h.act(|ui, _| {
        ui.reload_plugins();
    });
    h.read(|ui| {
        let found = ui.plugins.get("testforge").unwrap();
        assert_eq!(found.origin, Origin::Replaces);
        assert_eq!(found.state, State::NotAllowed);
    });
}

/// A forge claims its hosts from its manifest, before its script has loaded:
/// nothing about claiming waits on JavaScript.
#[test]
fn a_forge_claims_before_its_script_loads() {
    let mut h = Harness::open_prepared(None, SIZE, &config(), "", |home| {
        install_cli(&home.join("testforge-bin"));
        let dir = home.join(".bundled-test/testforge");
        copy_plugin(&dir, None);
        let main = std::fs::read_to_string(dir.join("main.js")).unwrap();
        let slow = format!("await new Promise((done) => setTimeout(done, 3000));\n{main}");
        std::fs::write(dir.join("main.js"), slow).unwrap();
    });
    h.read(|ui| {
        assert_eq!(state(ui, "testforge"), Some(State::Loading));
        assert_eq!(
            claimed(ui, "https://testforge.invalid/team/app").as_deref(),
            Some("Test forge")
        );
        // Its links open before it has loaded, too.
        assert!(
            ui.forge
                .forges
                .parse_pr("https://testforge.invalid/team/app/pull/1")
                .is_some()
        );
    });
    wait_running(&mut h, "testforge");
}

//! diffui's fetch against the real `jj git fetch` 0.45.1: each scenario sets
//! up two identical repos fetching the same bare remotes, fetches one through
//! the actor and the other through the jj CLI, and compares the resulting
//! views (bookmarks, remote bookmarks and their tracking, tags, visible
//! heads) and jj's warnings and ref lines.
//!
//! Needs the git CLI and a jj 0.45.1 binary: `$DIFFUI_PARITY_JJ`, else the UI
//! kit's `/tmp/diffui-fix/uikit/jj`. A scenario without either skips.

use crate::harness;

use std::path::{Path, PathBuf};
use std::process::Command as Process;

use diffui_core::repo::{Command, Payload};
use diffui_core::{FetchOutcome, FetchTarget};
use harness::{TestRepo, block_on, run_git, scratch_repo};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRefState};

const IDENTITY: &str = "[user]\nname = \"Scenario Test\"\nemail = \"scenario@example.com\"\n\
                        [signing]\nbehavior = \"keep\"\n";

fn jj_binary() -> Option<PathBuf> {
    let path = std::env::var_os("DIFFUI_PARITY_JJ")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/diffui-fix/uikit/jj"));
    let version = Process::new(&path).arg("version").output().ok()?;
    let version = String::from_utf8_lossy(&version.stdout).into_owned();
    version.contains("0.45.1").then_some(path)
}

/// A bare git remote and the work tree that feeds it.
struct Remote {
    bare: PathBuf,
    seed: PathBuf,
}

impl Remote {
    /// A bare remote whose `main` holds one commit.
    fn new(test: &str, name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("diffui-parity-{test}-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let bare = base.join("remote.git");
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("create seed dir");
        run_git(Path::new("."), &["init", "--bare", &bare.to_string_lossy()]);
        run_git(&seed, &["init", "-b", "main"]);
        let remote = Self { bare, seed };
        remote.commit("main", &format!("{name} main"));
        remote
    }

    fn git(&self, args: &[&str]) -> String {
        let mut all = vec![
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ];
        all.extend_from_slice(args);
        run_git(&self.seed, &all)
    }

    /// A commit on `branch` (created from `main` when new), pushed.
    fn commit(&self, branch: &str, message: &str) {
        let exists = !run_git(&self.seed, &["branch", "--list", branch])
            .trim()
            .is_empty();
        if exists {
            self.git(&["checkout", "-q", branch]);
        } else if branch == "main" {
            // The first commit makes `main` exist.
        } else {
            self.git(&["checkout", "-q", "-b", branch, "main"]);
        }
        std::fs::write(self.seed.join(format!("{branch}.txt")), message).expect("write");
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["push", "-q", &self.bare.to_string_lossy(), branch]);
    }

    fn tag(&self, name: &str, branch: &str) {
        self.git(&["tag", name, branch]);
        self.git(&["push", "-q", &self.bare.to_string_lossy(), name]);
    }

    fn delete_branch(&self, branch: &str) {
        self.git(&[
            "push",
            "-q",
            &self.bare.to_string_lossy(),
            &format!(":{branch}"),
        ]);
    }
}

fn add_remote(root: &Path, name: &str, remote: &Remote) {
    let colocated = root.join(".git");
    let git_dir = if colocated.is_dir() {
        colocated
    } else {
        harness::repo_dir(root).join("store").join("git")
    };
    run_git(
        root,
        &[
            "--git-dir",
            &git_dir.to_string_lossy(),
            "remote",
            "add",
            name,
            &remote.bare.to_string_lossy(),
        ],
    );
}

/// The two repos of a scenario: one fetched by diffui, one by the jj CLI,
/// both with the same remotes and the same jj config.
struct Pair {
    jj: PathBuf,
    diffui: TestRepo,
    cli: PathBuf,
    home: PathBuf,
    config: PathBuf,
}

impl Pair {
    fn new(test: &str, remotes: &[(&str, &Remote)], config: &str) -> Option<Self> {
        Self::with_layout(test, remotes, config, false)
    }

    fn with_layout(
        test: &str,
        remotes: &[(&str, &Remote)],
        config: &str,
        colocated: bool,
    ) -> Option<Self> {
        if !harness::git_available() {
            eprintln!("skipping: the git CLI is not on PATH");
            return None;
        }
        let Some(jj) = jj_binary() else {
            eprintln!("skipping: no jj 0.45.1 binary (set DIFFUI_PARITY_JJ)");
            return None;
        };
        let init = if colocated {
            harness::scratch_colocated_repo
        } else {
            scratch_repo
        };
        let diffui_root = init(&format!("parity-{test}-diffui"));
        let cli = init(&format!("parity-{test}-cli"));
        for (name, remote) in remotes {
            add_remote(&diffui_root, name, remote);
            add_remote(&cli, name, remote);
        }
        // diffui's fixture reads the repo's own config file; the CLI gets the
        // same text, plus the identity, as its only user config.
        std::fs::write(harness::repo_dir(&diffui_root).join("config.toml"), config)
            .expect("write repo config");
        let home = cli.with_extension("home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create home");
        let config_path = home.join("config.toml");
        std::fs::write(&config_path, format!("{IDENTITY}{config}")).expect("write cli config");
        Some(Self {
            jj,
            diffui: TestRepo::open(&diffui_root),
            cli,
            home,
            config: config_path,
        })
    }

    /// Fetch through the actor.
    fn fetch_diffui(&mut self, target: FetchTarget) -> FetchOutcome {
        let events = self.diffui.run(|job| Command::Fetch { job, target });
        match events.last() {
            Some(Payload::FetchDone { outcome, .. }) => outcome.clone(),
            other => panic!("the fetch failed: {other:?}"),
        }
    }

    /// Fetch through the actor, expecting jj's refusal.
    fn fetch_diffui_error(&mut self, target: FetchTarget) -> String {
        let events = self.diffui.run(|job| Command::Fetch { job, target });
        match events.last() {
            Some(Payload::Failed { error, .. }) => error.to_string(),
            other => panic!("the fetch should fail: {other:?}"),
        }
    }

    /// `jj git fetch ARGS` in the CLI's repo; its stderr, and whether it
    /// succeeded.
    fn fetch_cli(&self, args: &[&str]) -> (bool, String) {
        let output = Process::new(&self.jj)
            .current_dir(&self.cli)
            .args(["git", "fetch"])
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("JJ_CONFIG", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run jj");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Both fetches, then the views compared and jj's own lines matched.
    fn fetch_both(&mut self, target: FetchTarget, args: &[&str]) -> (FetchOutcome, String) {
        let outcome = self.fetch_diffui(target);
        let (ok, stderr) = self.fetch_cli(args);
        assert!(ok, "jj git fetch {args:?} failed:\n{stderr}");
        assert_eq!(
            view(&self.diffui.root),
            view(&self.cli),
            "the views differ after `jj git fetch {args:?}`\njj said:\n{stderr}"
        );
        assert_eq!(
            outcome
                .warnings
                .iter()
                .map(|warning| format!("Warning: {warning}"))
                .collect::<Vec<_>>(),
            jj_lines(&stderr, &["Warning: "]),
            "warnings, jj said:\n{stderr}"
        );
        let refs: Vec<String> = outcome
            .output
            .iter()
            .filter(|line| line.starts_with("bookmark: ") || line.starts_with("tag: "))
            .cloned()
            .collect();
        assert_eq!(
            refs,
            jj_lines(&stderr, &["bookmark: ", "tag: "]),
            "ref lines, jj said:\n{stderr}"
        );
        (outcome, stderr)
    }
}

fn jj_lines(stderr: &str, prefixes: &[&str]) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| prefixes.iter().any(|prefix| line.starts_with(prefix)))
        .map(str::to_owned)
        .collect()
}

fn target(target: &RefTarget) -> String {
    let ids = |ids: Vec<String>| ids.join(",");
    let added = ids(target.added_ids().map(|id| id.hex()).collect());
    let removed = ids(target.removed_ids().map(|id| id.hex()).collect());
    if removed.is_empty() {
        added
    } else {
        format!("{added} - {removed}")
    }
}

/// Everything a fetch can change in a view, as sorted lines: bookmarks and
/// tags, local and remote (with tracking), and the visible heads other than
/// the working-copy commits (which are each repo's own).
fn view(root: &Path) -> Vec<String> {
    let workspace = harness::load_workspace(root);
    let repo = block_on(workspace.repo_loader().load_at_head()).expect("load repo");
    let view = repo.view();
    let state = |state: RemoteRefState| match state {
        RemoteRefState::New => "untracked",
        RemoteRefState::Tracked => "tracked",
    };
    let mut lines = Vec::new();
    for (name, value) in view.local_bookmarks() {
        lines.push(format!("bookmark {} {}", name.as_str(), target(value)));
    }
    for (symbol, remote_ref) in view.all_remote_bookmarks() {
        lines.push(format!(
            "remote bookmark {symbol} {} {}",
            target(&remote_ref.target),
            state(remote_ref.state)
        ));
    }
    for (name, value) in view.local_tags() {
        lines.push(format!("tag {} {}", name.as_str(), target(value)));
    }
    for (symbol, remote_ref) in view.all_remote_tags() {
        lines.push(format!(
            "remote tag {symbol} {} {}",
            target(&remote_ref.target),
            state(remote_ref.state)
        ));
    }
    let working_copies: Vec<_> = view.wc_commit_ids().values().cloned().collect();
    for head in view.heads() {
        if !working_copies.contains(head) {
            lines.push(format!("head {}", head.hex()));
        }
    }
    lines.sort();
    lines
}

#[test]
fn a_plain_fetch_takes_origins_bookmarks_and_tags() {
    let origin = Remote::new("plain", "origin");
    origin.commit("feature", "feature work");
    origin.tag("v1", "main");
    let Some(mut pair) = Pair::new("plain", &[("origin", &origin)], "") else {
        return;
    };
    let (outcome, _) = pair.fetch_both(FetchTarget::Default, &[]);
    assert_eq!(outcome.remotes, ["origin"]);
    assert!(outcome.changed);
    assert!(
        view(&pair.cli)
            .iter()
            .any(|line| line.starts_with("remote tag v1@origin")),
        "tags come along"
    );
}

#[test]
fn without_git_fetch_origin_is_the_default_among_several_remotes() {
    let origin = Remote::new("several", "origin");
    let upstream = Remote::new("several", "upstream");
    upstream.commit("other", "upstream work");
    let Some(mut pair) = Pair::new(
        "several",
        &[("origin", &origin), ("upstream", &upstream)],
        "",
    ) else {
        return;
    };
    let (outcome, _) = pair.fetch_both(FetchTarget::Default, &[]);
    assert_eq!(outcome.remotes, ["origin"]);
}

#[test]
fn the_only_remote_is_fetched_whatever_its_name() {
    let upstream = Remote::new("only", "upstream");
    let Some(mut pair) = Pair::new("only", &[("upstream", &upstream)], "") else {
        return;
    };
    let (outcome, stderr) = pair.fetch_both(FetchTarget::Default, &[]);
    assert_eq!(outcome.remotes, ["upstream"]);
    assert!(stderr.contains("Fetching from the only existing remote: upstream"));
    assert!(
        outcome
            .output
            .contains(&"Fetching from the only existing remote: upstream".to_owned())
    );
}

#[test]
fn git_fetch_names_the_remotes_and_warns_about_missing_ones() {
    let origin = Remote::new("git-fetch", "origin");
    let upstream = Remote::new("git-fetch", "upstream");
    let fork = Remote::new("git-fetch", "fork");
    let Some(mut pair) = Pair::new(
        "git-fetch",
        &[
            ("origin", &origin),
            ("upstream", &upstream),
            ("fork", &fork),
        ],
        "[git]\nfetch = [\"upstream\", \"fork\", \"gone\"]\n",
    ) else {
        return;
    };
    let (outcome, _) = pair.fetch_both(FetchTarget::Default, &[]);
    assert_eq!(outcome.remotes, ["fork", "upstream"]);
    assert_eq!(outcome.warnings, ["No matching remotes for names: gone"]);
}

#[test]
fn git_fetch_naming_no_existing_remote_is_refused_like_jj() {
    let origin = Remote::new("git-fetch-none", "origin");
    let Some(mut pair) = Pair::new(
        "git-fetch-none",
        &[("origin", &origin)],
        "[git]\nfetch = \"gone\"\n",
    ) else {
        return;
    };
    let error = pair.fetch_diffui_error(FetchTarget::Default);
    let (ok, stderr) = pair.fetch_cli(&[]);
    assert!(!ok);
    assert!(stderr.contains("No git remotes to fetch from"), "{stderr}");
    assert!(error.contains("No git remotes to fetch from"), "{error}");
}

#[test]
fn fetch_bookmarks_and_fetch_tags_patterns_narrow_the_fetch() {
    let origin = Remote::new("patterns", "origin");
    origin.commit("feat-a", "a");
    origin.commit("feat-b", "b");
    origin.commit("wip", "wip");
    origin.tag("v1", "main");
    origin.tag("nightly", "main");
    let Some(mut pair) = Pair::new(
        "patterns",
        &[("origin", &origin)],
        "[remotes.origin]\nfetch-bookmarks = \"feat-* | main\"\nfetch-tags = \"v*\"\n\
         auto-track-bookmarks = \"feat-a\"\n",
    ) else {
        return;
    };
    pair.fetch_both(FetchTarget::Default, &[]);
    let view = view(&pair.cli);
    assert!(view.iter().any(|line| line.starts_with("bookmark feat-a ")));
    assert!(!view.iter().any(|line| line.contains("wip@origin")));
    assert!(!view.iter().any(|line| line.contains("nightly")));
}

#[test]
fn all_remotes_and_a_single_branch_match_their_flags() {
    let origin = Remote::new("flags", "origin");
    origin.commit("feature", "feature");
    origin.tag("v1", "main");
    let upstream = Remote::new("flags", "upstream");
    upstream.commit("other", "other");
    let Some(mut pair) = Pair::new("flags", &[("origin", &origin), ("upstream", &upstream)], "")
    else {
        return;
    };
    let (outcome, _) = pair.fetch_both(
        FetchTarget::RemoteBranch {
            remote: "origin".to_owned(),
            branch: "feature".to_owned(),
        },
        &["--remote", "origin", "--branch", "feature"],
    );
    assert_eq!(outcome.remotes, ["origin"]);
    let (outcome, _) = pair.fetch_both(
        FetchTarget::RemoteBranch {
            remote: "origin".to_owned(),
            branch: "nosuch".to_owned(),
        },
        &["--remote", "origin", "--branch", "nosuch"],
    );
    assert_eq!(
        outcome.warnings,
        ["No matching branches found on any specified/configured remote: nosuch"]
    );
    let (outcome, _) = pair.fetch_both(FetchTarget::AllRemotes, &["--all-remotes"]);
    assert_eq!(outcome.remotes, ["origin", "upstream"]);
}

#[test]
fn a_bookmark_deleted_on_the_remote_goes_and_its_commits_are_abandoned() {
    let origin = Remote::new("deleted", "origin");
    origin.commit("feature", "feature");
    let Some(mut pair) = Pair::new(
        "deleted",
        &[("origin", &origin)],
        "[remotes.origin]\nauto-track-bookmarks = \"*\"\n",
    ) else {
        return;
    };
    pair.fetch_both(FetchTarget::Default, &[]);
    origin.delete_branch("feature");
    let (outcome, _) = pair.fetch_both(FetchTarget::Default, &[]);
    assert!(
        outcome
            .output
            .iter()
            .any(|line| line.starts_with("Abandoned 1 commits")),
        "{outcome:?}"
    );
    assert!(
        !view(&pair.cli).iter().any(|line| line.contains("feature")),
        "the bookmark is gone"
    );
}

#[test]
fn a_remote_with_nothing_new_changes_nothing() {
    let origin = Remote::new("nothing", "origin");
    let Some(mut pair) = Pair::new("nothing", &[("origin", &origin)], "") else {
        return;
    };
    pair.fetch_both(FetchTarget::Default, &[]);
    let ops_before = harness::operation_log(&pair.diffui.root).len();
    let (outcome, stderr) = pair.fetch_both(FetchTarget::Default, &[]);
    assert!(!outcome.changed);
    assert!(stderr.contains("Nothing changed."), "{stderr}");
    assert!(outcome.output.contains(&"Nothing changed.".to_owned()));
    assert_eq!(
        harness::operation_log(&pair.diffui.root).len(),
        ops_before,
        "no operation, like jj"
    );
}

/// Colocated, jj also exports the fetched refs to the git repo it shares.
#[test]
fn a_colocated_fetch_exports_like_jj() {
    let origin = Remote::new("colocated", "origin");
    origin.commit("feature", "feature");
    origin.tag("v1", "main");
    let Some(mut pair) = Pair::with_layout(
        "colocated",
        &[("origin", &origin)],
        "[remotes.origin]\nauto-track-bookmarks = \"*\"\n",
        true,
    ) else {
        return;
    };
    pair.fetch_both(FetchTarget::Default, &[]);
    assert!(
        view(&pair.cli)
            .iter()
            .any(|line| line.starts_with("remote bookmark feature@git")),
        "the fetched bookmark reached git"
    );
}

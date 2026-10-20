//! `jj git push` parity: the bookmark workflow (create → push new → move &
//! push → delete on the remote) and the CLI's pre-push checks, against a bare
//! git repository on disk as the remote.
//!
//! jj-lib's push shells out to `git push`, so every scenario here skips when
//! the git CLI isn't on PATH, like the git suite does.

mod harness;

use std::path::{Path, PathBuf};

use diffui_core::repo::{Command, Payload, RepoError};
use diffui_core::{
    BookmarksInfo, MutationOp, PushAllow, PushCheck, PushRequest, PushTarget, RevisionSelection,
};
use harness::{TestRepo, scratch_repo, write};

/// A scratch repo with a bare remote `origin`, opened through the actor.
fn setup(test: &str) -> Option<(TestRepo, PathBuf)> {
    setup_with_config(test, None)
}

/// [`setup`], with `config` as the repo's own `.jj/repo/config.toml`.
fn setup_with_config(test: &str, config: Option<&str>) -> Option<(TestRepo, PathBuf)> {
    if !harness::git_available() {
        eprintln!("skipping: the git CLI is not on PATH");
        return None;
    }
    let root = scratch_repo(test);
    let remote = std::env::temp_dir().join(format!("diffui-actor-{test}-remote.git"));
    harness::init_git_remote(&root, "origin", &remote);
    if let Some(config) = config {
        std::fs::write(harness::repo_dir(&root).join("config.toml"), config)
            .expect("write repo config");
    }
    Some((TestRepo::open(&root), remote))
}

/// `jj commit -m <message>` with `name` written: returns the new commit's id.
fn commit(repo: &mut TestRepo, name: &str, message: &str) -> String {
    harness::commit_file(repo, name, message, &format!("{message}\n"));
    one(repo, "@-")
}

fn one(repo: &mut TestRepo, revset: &str) -> String {
    let rows = repo.graph(revset);
    assert_eq!(rows.len(), 1, "{revset} must name one revision");
    rows[0].summary.commit_id.clone()
}

fn at(id: &str) -> RevisionSelection {
    RevisionSelection::Commit(id.to_owned())
}

fn push(remote: &str, targets: Vec<PushTarget>) -> MutationOp {
    MutationOp::Push(PushRequest {
        remote: remote.to_owned(),
        targets,
        allow: PushAllow::default(),
        confirm: false,
    })
}

/// The bookmark table the menus read, as of now.
fn bookmarks(repo: &mut TestRepo) -> BookmarksInfo {
    let events = repo.run(|job| Command::LoadGraph {
        job,
        revset: "all()".to_owned(),
    });
    for payload in events {
        if let Payload::GraphLoaded { tail, .. } = payload {
            return tail.bookmarks;
        }
    }
    panic!("graph load produced no tail");
}

/// Where `refs/heads/<name>` points on the bare remote, if anywhere.
fn remote_ref(remote: &Path, name: &str) -> Option<String> {
    let out = harness::run_git(
        remote,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            &format!("refs/heads/{name}"),
        ],
    );
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_owned())
}

#[test]
fn create_then_push_new_bookmark_creates_and_tracks_it_on_the_remote() {
    let Some((mut repo, remote)) = setup("push-new") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    repo.mutate(MutationOp::CreateBookmark {
        name: "feature".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");

    let info = bookmarks(&mut repo);
    assert_eq!(info.remotes, vec!["origin".to_owned()]);
    assert_eq!(info.default_push_remote(), Some("origin"));
    let entry = info.entry("feature").expect("bookmark exists");
    assert!(
        !entry.is_tracked_anywhere(),
        "new bookmarks start untracked"
    );

    let outcome = repo
        .mutate(push("origin", vec![PushTarget::Bookmark("feature".into())]))
        .expect("push a new bookmark");
    assert_eq!(outcome.message, "Pushed feature to origin");
    assert!(
        outcome
            .output
            .iter()
            .any(|line| line.contains("bookmark: feature [add to")),
        "the plan is reported: {:?}",
        outcome.output
    );
    assert_eq!(remote_ref(&remote, "feature"), Some(first));
    assert!(
        bookmarks(&mut repo)
            .entry("feature")
            .is_some_and(|entry| entry.tracks("origin")),
        "a pushed bookmark tracks the remote it went to"
    );
}

#[test]
fn create_refuses_existing_and_invalid_names() {
    let Some((mut repo, _remote)) = setup("push-create-guards") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    repo.mutate(MutationOp::CreateBookmark {
        name: "main".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");
    for name in ["main", "has space", "a..b"] {
        let error = repo
            .mutate(MutationOp::CreateBookmark {
                name: name.to_owned(),
                to: at(&first),
            })
            .expect_err("must refuse");
        assert!(!error.to_string().is_empty(), "{name}");
    }
}

#[test]
fn empty_description_blocks_the_push_until_allowed() {
    let Some((mut repo, remote)) = setup("push-empty-desc") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    let wip = harness::new_commit(&repo.root, std::slice::from_ref(&first), "");
    repo.mutate(MutationOp::CreateBookmark {
        name: "wip".to_owned(),
        to: at(&wip),
    })
    .expect("create bookmark");

    let error = repo
        .mutate(push("origin", vec![PushTarget::Bookmark("wip".into())]))
        .expect_err("an empty description must block the push");
    let RepoError::PushRejected(rejection) = error else {
        panic!("expected a typed rejection, got {error:?}");
    };
    assert_eq!(rejection.commits.len(), 1, "{:?}", rejection.commits);
    assert_eq!(
        rejection.commits[0].reasons,
        vec![PushCheck::EmptyDescription]
    );
    assert!(
        rejection.commits[0].commit_id.starts_with(&wip[..12]),
        "names the offending commit"
    );
    assert_eq!(
        remote_ref(&remote, "wip"),
        None,
        "nothing reached the remote"
    );

    // The CLI's --allow-empty-description is the escape hatch.
    let allow = rejection
        .allowances()
        .expect("an empty description is allowable");
    assert!(allow.empty_description && !allow.private && !allow.conflicts);
    let mut rerun = rejection.request.clone();
    rerun.allow = allow;
    repo.mutate(MutationOp::Push(rerun))
        .expect("allowed push goes through");
    assert_eq!(remote_ref(&remote, "wip"), Some(wip));
}

#[test]
fn move_and_push_onto_wip_is_refused_and_rolls_the_move_back() {
    let Some((mut repo, remote)) = setup("push-move-wip") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    repo.mutate(MutationOp::CreateBookmark {
        name: "main".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");
    repo.mutate(push("origin", vec![PushTarget::Bookmark("main".into())]))
        .expect("initial push");

    // `@` is an undescribed working-copy commit with a change in it.
    write(&repo.root, "b.txt", "wip\n");
    repo.snapshot();
    let error = repo
        .mutate(MutationOp::MoveBookmark {
            name: "main".to_owned(),
            to: RevisionSelection::WorkingCopy,
            push_remote: Some("origin".to_owned()),
        })
        .expect_err("pushing the WIP working copy must be refused");
    let RepoError::PushRejected(rejection) = error else {
        panic!("expected a typed rejection, got {error:?}");
    };
    assert!(rejection.commits[0].working_copy, "flags the working copy");
    assert_eq!(remote_ref(&remote, "main"), Some(first.clone()));
    assert_eq!(one(&mut repo, "main"), first, "the local move rolled back");
}

#[test]
fn private_commits_are_refused_with_the_configured_revset() {
    let Some((mut repo, remote)) = setup_with_config(
        "push-private",
        Some("[git]\nprivate-commits = \"description(glob:'secret*')\"\n"),
    ) else {
        return;
    };
    let secret = commit(&mut repo, "a.txt", "secret stuff");
    let public = commit(&mut repo, "b.txt", "public on top");
    repo.mutate(MutationOp::CreateBookmark {
        name: "topic".to_owned(),
        to: at(&public),
    })
    .expect("create bookmark");

    let error = repo
        .mutate(push("origin", vec![PushTarget::Bookmark("topic".into())]))
        .expect_err("a private ancestor blocks the push");
    let RepoError::PushRejected(rejection) = error else {
        panic!("expected a typed rejection, got {error:?}");
    };
    assert_eq!(rejection.commits.len(), 1);
    assert!(rejection.commits[0].commit_id.starts_with(&secret[..12]));
    assert_eq!(rejection.commits[0].reasons, vec![PushCheck::Private]);
    assert_eq!(
        rejection.private_commits.as_deref(),
        Some("description(glob:'secret*')")
    );
    assert!(rejection.allowances().is_some_and(|allow| allow.private));
    assert_eq!(remote_ref(&remote, "topic"), None);
}

#[test]
fn conflicted_commits_are_refused() {
    let Some((mut repo, remote)) = setup("push-conflict") else {
        return;
    };
    let base = commit(&mut repo, "f.txt", "base");
    let side_a = commit(&mut repo, "f.txt", "side a");
    repo.mutate(MutationOp::New { parent: at(&base) })
        .expect("new on base");
    let side_b = commit(&mut repo, "f.txt", "side b");
    repo.mutate(MutationOp::Merge {
        parents: vec![at(&side_a), at(&side_b)],
    })
    .expect("merge the sides");
    repo.mutate(MutationOp::Describe {
        target: RevisionSelection::WorkingCopy,
        description: "merge\n".to_owned(),
    })
    .expect("describe the merge");
    repo.mutate(MutationOp::CreateBookmark {
        name: "merged".to_owned(),
        to: RevisionSelection::WorkingCopy,
    })
    .expect("create bookmark");

    let error = repo
        .mutate(push("origin", vec![PushTarget::Bookmark("merged".into())]))
        .expect_err("a conflicted commit blocks the push");
    let RepoError::PushRejected(rejection) = error else {
        panic!("expected a typed rejection, got {error:?}");
    };
    assert!(
        rejection
            .commits
            .iter()
            .any(|commit| commit.reasons.contains(&PushCheck::Conflicts)),
        "{:?}",
        rejection.commits
    );
    assert_eq!(remote_ref(&remote, "merged"), None);
}

#[test]
fn delete_on_remote_pushes_the_deletion() {
    let Some((mut repo, remote)) = setup("push-delete") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    repo.mutate(MutationOp::CreateBookmark {
        name: "gone".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");
    repo.mutate(push("origin", vec![PushTarget::Bookmark("gone".into())]))
        .expect("push");
    assert!(remote_ref(&remote, "gone").is_some());

    let outcome = repo
        .mutate(push(
            "origin",
            vec![PushTarget::DeleteBookmark("gone".into())],
        ))
        .expect("delete on the remote");
    assert!(
        outcome
            .output
            .iter()
            .any(|line| line.contains("bookmark: gone [delete from")),
        "{:?}",
        outcome.output
    );
    assert_eq!(remote_ref(&remote, "gone"), None);
    assert!(
        bookmarks(&mut repo).entry("gone").is_none(),
        "nothing is left locally either"
    );
}

#[test]
fn push_change_creates_the_generated_bookmark() {
    let Some((mut repo, remote)) = setup("push-change") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    let change = repo.graph("@-")[0].summary.change_id.clone();
    let outcome = repo
        .mutate(push("origin", vec![PushTarget::Change(at(&first))]))
        .expect("push --change");
    let name = format!("push-{}", &change[..12]);
    assert_eq!(outcome.message, format!("Pushed {name} to origin"));
    assert_eq!(remote_ref(&remote, &name), Some(first.clone()));
    assert!(
        bookmarks(&mut repo)
            .entry(&name)
            .is_some_and(|entry| entry.tracks("origin"))
    );
    // Pushing the same change again is a no-op, not a "bookmark exists" error.
    let again = repo
        .mutate(push("origin", vec![PushTarget::Change(at(&first))]))
        .expect("re-push --change");
    assert!(
        again.message.starts_with("Nothing to push"),
        "{}",
        again.message
    );
}

/// `templates.git_push_bookmark` renders through jj-cli's template engine, so
/// anything a jj template can say names the bookmark.
#[test]
fn push_change_renders_the_bookmark_template_like_jj() {
    let config = "[templates]\n\
                  git_push_bookmark = '\"me/\" ++ change_id.shortest(8) ++ \"-\" ++ \
                  description.first_line()'\n";
    let Some((mut repo, remote)) = setup_with_config("push-change-template", Some(config)) else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    let change = repo.graph("@-")[0].summary.change_id.clone();
    let outcome = repo
        .mutate(push("origin", vec![PushTarget::Change(at(&first))]))
        .expect("push --change");
    let name = format!("me/{}-first", &change[..8]);
    assert_eq!(outcome.message, format!("Pushed {name} to origin"));
    assert_eq!(remote_ref(&remote, &name), Some(first));
}

#[test]
fn untracked_remote_bookmark_blocks_the_push_until_tracked() {
    let Some((mut repo, _remote)) = setup("push-untracked") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    repo.mutate(MutationOp::CreateBookmark {
        name: "main".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");
    repo.mutate(push("origin", vec![PushTarget::Bookmark("main".into())]))
        .expect("push");
    repo.mutate(MutationOp::UntrackBookmark {
        name: "main".to_owned(),
        remote: "origin".to_owned(),
    })
    .expect("untrack");
    assert!(!bookmarks(&mut repo).entry("main").unwrap().tracks("origin"));

    let second = commit(&mut repo, "b.txt", "second");
    repo.mutate(MutationOp::MoveBookmark {
        name: "main".to_owned(),
        to: at(&second),
        push_remote: None,
    })
    .expect("move");
    let error = repo
        .mutate(push("origin", vec![PushTarget::Bookmark("main".into())]))
        .expect_err("an untracked remote bookmark is in the way");
    assert!(
        error
            .to_string()
            .contains("non-tracking remote bookmark main@origin"),
        "{error}"
    );

    repo.mutate(MutationOp::TrackBookmark {
        name: "main".to_owned(),
        remote: "origin".to_owned(),
    })
    .expect("track");
    repo.mutate(push("origin", vec![PushTarget::Bookmark("main".into())]))
        .expect("push after tracking");
}

#[test]
fn a_remote_that_moved_since_the_last_fetch_rejects_the_push() {
    let Some((mut repo, remote)) = setup("push-lease") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    let second = commit(&mut repo, "b.txt", "second");
    for (name, id) in [("main", &first), ("other", &second)] {
        repo.mutate(MutationOp::CreateBookmark {
            name: name.to_owned(),
            to: at(id),
        })
        .expect("create bookmark");
        repo.mutate(push("origin", vec![PushTarget::Bookmark(name.into())]))
            .expect("push");
    }
    // Someone else moves `main` on the remote behind our back.
    harness::run_git(&remote, &["update-ref", "refs/heads/main", &second]);

    let third = commit(&mut repo, "c.txt", "third");
    let error = repo
        .mutate(MutationOp::MoveBookmark {
            name: "main".to_owned(),
            to: at(&third),
            push_remote: Some("origin".to_owned()),
        })
        .expect_err("the lease must fail");
    assert!(
        error
            .to_string()
            .contains("remote moved since the last fetch"),
        "{error}"
    );
    assert_eq!(remote_ref(&remote, "main"), Some(second));
    assert_eq!(one(&mut repo, "main"), first, "the local move rolled back");
}

#[test]
fn rename_carries_tracking_and_forget_untracks() {
    let Some((mut repo, _remote)) = setup("push-rename-forget") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    repo.mutate(MutationOp::CreateBookmark {
        name: "old".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");
    repo.mutate(push("origin", vec![PushTarget::Bookmark("old".into())]))
        .expect("push");

    repo.mutate(MutationOp::RenameBookmark {
        old: "old".to_owned(),
        new: "new".to_owned(),
    })
    .expect("rename");
    let info = bookmarks(&mut repo);
    let new = info.entry("new").expect("renamed bookmark");
    assert_eq!(new.local_target(), Some(first.as_str()));
    let old = info
        .entry("old")
        .expect("old remote bookmark is still there");
    assert!(
        old.local_targets.is_empty() && old.tracks("origin"),
        "the remote copy waits for a deletion push"
    );

    repo.mutate(MutationOp::ForgetBookmark {
        name: "old".to_owned(),
    })
    .expect("forget");
    let info = bookmarks(&mut repo);
    let old = info
        .entry("old")
        .expect("untracked remote bookmark remains");
    assert!(old.local_targets.is_empty() && !old.is_tracked_anywhere());
}

#[test]
fn tracked_push_sends_tracked_bookmarks_that_moved() {
    let Some((mut repo, remote)) = setup("push-tracked") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    repo.mutate(MutationOp::CreateBookmark {
        name: "main".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");
    repo.mutate(push("origin", vec![PushTarget::Bookmark("main".into())]))
        .expect("push");
    let second = commit(&mut repo, "b.txt", "second");
    repo.mutate(MutationOp::MoveBookmark {
        name: "main".to_owned(),
        to: at(&second),
        push_remote: None,
    })
    .expect("move");
    // An untracked local bookmark is not part of a plain push.
    repo.mutate(MutationOp::CreateBookmark {
        name: "local-only".to_owned(),
        to: at(&second),
    })
    .expect("create bookmark");

    let outcome = repo
        .mutate(push("origin", vec![PushTarget::Tracked]))
        .expect("plain push");
    assert_eq!(outcome.message, "Pushed main to origin");
    assert_eq!(remote_ref(&remote, "main"), Some(second));
    assert_eq!(remote_ref(&remote, "local-only"), None);
}

#[test]
fn tags_reach_the_graph_as_read_only_labels() {
    let Some((mut repo, _remote)) = setup("push-tags") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    harness::block_on(async {
        let workspace = harness::load_workspace(&repo.root);
        let base = workspace
            .repo_loader()
            .load_at_head()
            .await
            .expect("load repo");
        let mut tx = base.start_transaction();
        let id = jj_lib::backend::CommitId::try_from_hex(&first).expect("commit id");
        tx.repo_mut().set_local_tag_target(
            jj_lib::ref_name::RefName::new("v1.0"),
            jj_lib::op_store::RefTarget::normal(id),
        );
        tx.commit("test: tag").await.expect("commit the tag");
    });
    let info = bookmarks(&mut repo);
    assert_eq!(info.tags.get(&first), Some(&vec!["v1.0".to_owned()]));
    assert!(info.entry("v1.0").is_none(), "a tag is not a bookmark");
}

#[test]
fn a_confirmed_push_reports_its_plan_before_pushing() {
    let Some((mut repo, remote)) = setup("push-confirm") else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    repo.mutate(MutationOp::CreateBookmark {
        name: "main".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");
    repo.mutate(push("origin", vec![PushTarget::Bookmark("main".into())]))
        .expect("push");
    let second = commit(&mut repo, "b.txt", "second");
    repo.mutate(MutationOp::MoveBookmark {
        name: "main".to_owned(),
        to: at(&second),
        push_remote: None,
    })
    .expect("move");

    let error = repo
        .mutate(MutationOp::Push(PushRequest {
            remote: "origin".to_owned(),
            targets: vec![PushTarget::Tracked],
            allow: PushAllow::default(),
            confirm: true,
        }))
        .expect_err("a confirmed push stops at its plan");
    let RepoError::PushRejected(plan) = error else {
        panic!("expected the plan, got {error:?}");
    };
    assert!(plan.is_confirmation());
    assert_eq!(
        plan.plan,
        vec![format!(
            "bookmark: main [move forward from {} to {}]",
            &first[..12],
            &second[..12]
        )]
    );
    assert_eq!(
        remote_ref(&remote, "main"),
        Some(first),
        "nothing pushed yet"
    );

    let mut go = plan.request.clone();
    go.confirm = false;
    repo.mutate(MutationOp::Push(go))
        .expect("push after confirming");
    assert_eq!(remote_ref(&remote, "main"), Some(second));
}

// ── git.sign-on-push ────────────────────────────────────────────────────────

/// A stand-in for `ssh-keygen -Y sign` (or 1Password's `op-ssh-sign`) that
/// logs each call and prints a signature-shaped blob, or fails like a
/// cancelled prompt. Returns the repo config that signs through it with
/// `behavior`, plus `extra`, and the call log's path.
fn signing_config(test: &str, behavior: &str, fail: bool, extra: &str) -> (String, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = std::env::temp_dir().join(format!("diffui-actor-{test}-signer"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create signer dir");
    let log = dir.join("calls.log");
    let program = dir.join("sign.sh");
    let result = if fail {
        "echo 'cancelled by user' >&2\nexit 1\n"
    } else {
        "printf '%s\\n' '-----BEGIN SSH SIGNATURE-----' 'ZmFrZQ==' '-----END SSH SIGNATURE-----'\n"
    };
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\ncat > /dev/null\n{result}",
            log.display()
        ),
    )
    .expect("write signer");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .expect("make signer executable");
    let config = format!(
        "[git]\nsign-on-push = true\n\
         [signing]\nbehavior = \"{behavior}\"\nbackend = \"ssh\"\nkey = \"ssh-ed25519 AAAAtest\"\n\
         [signing.backends.ssh]\nprogram = \"{}\"\n{extra}",
        program.display()
    );
    (config, log)
}

/// How many times the signer ran.
fn sign_calls(log: &Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |text| text.lines().count())
}

fn is_signed(root: &Path, commit_id: &str) -> bool {
    harness::block_on(async {
        let workspace = harness::load_workspace(root);
        let repo = workspace
            .repo_loader()
            .load_at_head()
            .await
            .expect("load repo at head");
        let id = jj_lib::backend::CommitId::try_from_hex(commit_id).expect("commit id");
        jj_lib::repo::Repo::store(repo.as_ref())
            .get_commit_async(&id)
            .await
            .expect("load commit")
            .is_signed()
    })
}

/// Whether the commit at `refs/heads/<name>` on the remote carries a signature.
fn remote_signed(remote: &Path, name: &str) -> bool {
    harness::run_git(
        remote,
        &["cat-file", "commit", &format!("refs/heads/{name}")],
    )
    .lines()
    .any(|line| line.starts_with("gpgsig"))
}

#[test]
fn sign_on_push_with_everything_signed_pushes_without_rewriting() {
    let (config, log) = signing_config("push-sign-all-signed", "own", false, "");
    let Some((mut repo, remote)) = setup_with_config("push-sign-all-signed", Some(&config)) else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    assert!(is_signed(&repo.root, &first), "own signs on write");
    repo.mutate(MutationOp::CreateBookmark {
        name: "feature".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");
    let calls = sign_calls(&log);
    let ops = harness::operation_log(&repo.root).len();

    let outcome = repo
        .mutate(push("origin", vec![PushTarget::Bookmark("feature".into())]))
        .expect("push signed commits");
    assert_eq!(outcome.message, "Pushed feature to origin");
    assert_eq!(remote_ref(&remote, "feature"), Some(first.clone()));
    assert!(remote_signed(&remote, "feature"));
    assert_eq!(one(&mut repo, "feature"), first, "nothing was rewritten");
    assert_eq!(sign_calls(&log), calls, "nothing was signed again");
    assert_eq!(
        harness::operation_log(&repo.root).len(),
        ops + 1,
        "just the push"
    );
}

#[test]
fn sign_on_push_signs_unsigned_commits_then_pushes_them() {
    let (config, _log) = signing_config("push-sign-unsigned", "keep", false, "");
    let Some((mut repo, remote)) = setup_with_config("push-sign-unsigned", Some(&config)) else {
        return;
    };
    commit(&mut repo, "a.txt", "first");
    let second = commit(&mut repo, "b.txt", "second");
    assert!(
        !is_signed(&repo.root, &second),
        "keep leaves new commits unsigned"
    );
    repo.mutate(MutationOp::CreateBookmark {
        name: "feature".to_owned(),
        to: at(&second),
    })
    .expect("create bookmark");
    // An edit the app hasn't snapshotted yet survives `@` being rebased.
    write(&repo.root, "wip.txt", "wip\n");

    let outcome = repo
        .mutate(push("origin", vec![PushTarget::Bookmark("feature".into())]))
        .expect("sign and push");
    assert_eq!(outcome.message, "Pushed feature to origin");
    assert!(
        outcome
            .output
            .iter()
            .any(|line| line == "Signed 2 commits, rebased 1 descendant"),
        "{:?}",
        outcome.output
    );

    let signed = one(&mut repo, "feature");
    assert_ne!(signed, second, "the bookmark follows the signed rewrite");
    assert!(is_signed(&repo.root, &signed));
    let parent = one(&mut repo, "feature-");
    assert!(is_signed(&repo.root, &parent));
    assert_eq!(remote_ref(&remote, "feature"), Some(signed.clone()));
    assert!(remote_signed(&remote, "feature"));
    assert_eq!(one(&mut repo, "@-"), signed, "@ was rebased onto it");
    let wc = one(&mut repo, "@");
    assert_eq!(harness::file_at(&repo.root, &wc, "wip.txt"), "wip\n");

    let ops: Vec<String> = harness::operation_log(&repo.root)
        .into_iter()
        .map(|(description, _)| description)
        .collect();
    assert!(ops[0].starts_with("diffui: Pushed feature"), "{ops:?}");
    assert_eq!(ops[1], "diffui: sign commits before push", "{ops:?}");
}

#[test]
fn sign_on_push_signs_before_a_move_and_push_and_a_change_push() {
    let (config, _log) = signing_config("push-sign-move", "keep", false, "");
    let Some((mut repo, remote)) = setup_with_config("push-sign-move", Some(&config)) else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    let outcome = repo
        .mutate(MutationOp::MoveBookmark {
            name: "main".to_owned(),
            to: at(&first),
            push_remote: Some("origin".to_owned()),
        })
        .expect("move, sign, and push");
    assert!(
        outcome.message.ends_with("Pushed main to origin"),
        "{}",
        outcome.message
    );
    let main = one(&mut repo, "main");
    assert_ne!(main, first);
    assert!(is_signed(&repo.root, &main));
    assert_eq!(remote_ref(&remote, "main"), Some(main));

    let second = commit(&mut repo, "b.txt", "second");
    let change = repo.graph("@-")[0].summary.change_id.clone();
    repo.mutate(push("origin", vec![PushTarget::Change(at(&second))]))
        .expect("push --change");
    let name = format!("push-{}", &change[..12]);
    let pushed = one(&mut repo, &name);
    assert!(is_signed(&repo.root, &pushed));
    assert_eq!(remote_ref(&remote, &name), Some(pushed));
}

#[test]
fn sign_on_push_skips_immutable_commits_with_a_warning() {
    let (config, _log) = signing_config(
        "push-sign-immutable",
        "keep",
        false,
        "[revset-aliases]\n'immutable_heads()' = 'description(glob:\"frozen*\")'\n",
    );
    let Some((mut repo, remote)) = setup_with_config("push-sign-immutable", Some(&config)) else {
        return;
    };
    let frozen = commit(&mut repo, "a.txt", "frozen");
    let frozen_change = repo.graph("@-")[0].summary.change_id.clone();
    let next = commit(&mut repo, "b.txt", "next");
    repo.mutate(MutationOp::CreateBookmark {
        name: "feature".to_owned(),
        to: at(&next),
    })
    .expect("create bookmark");

    let outcome = repo
        .mutate(push("origin", vec![PushTarget::Bookmark("feature".into())]))
        .expect("push around the immutable commit");
    assert!(
        outcome.output.contains(&format!(
            "Skipped signing 1 immutable commit: {}",
            &frozen_change[..8]
        )),
        "{:?}",
        outcome.output
    );
    let signed = one(&mut repo, "feature");
    assert!(is_signed(&repo.root, &signed));
    assert_eq!(one(&mut repo, "feature-"), frozen, "left as it was");
    assert!(!is_signed(&repo.root, &frozen));
    assert_eq!(remote_ref(&remote, "feature"), Some(signed));
}

#[test]
fn a_signing_failure_pushes_nothing_and_rewrites_nothing() {
    let (config, log) = signing_config("push-sign-fails", "keep", true, "");
    let Some((mut repo, remote)) = setup_with_config("push-sign-fails", Some(&config)) else {
        return;
    };
    let first = commit(&mut repo, "a.txt", "first");
    repo.mutate(MutationOp::CreateBookmark {
        name: "feature".to_owned(),
        to: at(&first),
    })
    .expect("create bookmark");
    let wc = one(&mut repo, "@");
    let ops = harness::operation_log(&repo.root).len();

    let error = repo
        .mutate(push("origin", vec![PushTarget::Bookmark("feature".into())]))
        .expect_err("a cancelled signature stops the push");
    let message = error.to_string();
    assert!(
        message.starts_with("signing failed, nothing was pushed: ")
            && message.contains("cancelled by user"),
        "{message}"
    );
    assert!(sign_calls(&log) > 0, "the signer was asked");
    assert_eq!(remote_ref(&remote, "feature"), None);
    assert_eq!(one(&mut repo, "feature"), first);
    assert_eq!(one(&mut repo, "@"), wc);
    assert_eq!(harness::operation_log(&repo.root).len(), ops);
}

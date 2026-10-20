//! The `args` tag on diffui's own operations: the jj command each one is
//! equivalent to, recorded where jj-cli records the command line it ran, so
//! `jj op log` in a terminal says what diffui did.

use std::collections::HashMap;

use jj_lib::{backend::CommitId, repo::Repo};

use crate::cli::RevisionNames;
use crate::mutations::MutationOp;
use crate::plan::{args_tag, change_id_name, ignoring_immutable};

/// Change ids by commit id (hex), for the commits an op names.
struct Names(HashMap<String, String>);

impl RevisionNames for Names {
    fn change_id(&self, commit_id: &str) -> Option<String> {
        self.0.get(commit_id).cloned()
    }
}

/// Collects the commit ids a render asks for, naming none.
struct Asked(std::cell::RefCell<Vec<String>>);

impl RevisionNames for Asked {
    fn change_id(&self, commit_id: &str) -> Option<String> {
        self.0.borrow_mut().push(commit_id.to_owned());
        None
    }
}

/// The tag for `op` run against `repo` (the repository before it ran).
/// Commits are named by change id where that names exactly one visible
/// commit, the way the loaded graph names them; anything else keeps its
/// commit id.
pub(crate) async fn op_args<R: Repo + ?Sized>(
    repo: &R,
    op: &MutationOp,
    allow_immutable: bool,
) -> String {
    let asked = Asked(Default::default());
    op.cli_equivalent(&asked);
    let mut names = HashMap::new();
    for hex in asked.0.into_inner() {
        if names.contains_key(&hex) {
            continue;
        }
        if let Some(name) = change_name(repo, &hex).await {
            names.insert(hex, name);
        }
    }
    let equivalent = op.cli_equivalent(&Names(names));
    let equivalent = if allow_immutable {
        ignoring_immutable(equivalent)
    } else {
        equivalent
    };
    args_tag(&equivalent)
}

async fn change_name<R: Repo + ?Sized>(repo: &R, hex: &str) -> Option<String> {
    let id = CommitId::try_from_hex(hex)?;
    let commit = repo.store().get_commit_async(&id).await.ok()?;
    let change_id = commit.change_id();
    let targets = repo.resolve_change_id(change_id).await.ok().flatten()?;
    let mut visible = targets.visible_with_offsets().map(|(_, id)| id);
    if visible.next() != Some(&id) || visible.next().is_some() {
        return None;
    }
    let shortest = repo
        .shortest_unique_change_id_prefix_len(change_id)
        .await
        .ok();
    Some(change_id_name(&change_id.to_string(), shortest))
}

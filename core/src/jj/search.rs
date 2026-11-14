//! Searching all of a repository's history by text, for the palette: every
//! visible commit scored as [`crate::commit_search`] scores the graph's rows,
//! whether or not the log revset (or the walk so far) has loaded it.

use std::path::Path;

use anyhow::{Context, Result};
use futures::StreamExt as _;
use jj_lib::{
    object_id::ObjectId as _,
    ref_name::WorkspaceName,
    repo::{ReadonlyRepo, Repo as _},
    revset::{RevsetExpression, SymbolResolver, SymbolResolverExtension, UserRevsetExpression},
};

use super::walk::{parse_user_revset, ref_labels};
use crate::commit_search::{Best, CommitHit, MAX_HITS, Scorer, haystack};
use crate::repo::CancelFlag;

/// Commits read between two looks at the cancel flag.
const CANCEL_STRIDE: usize = 256;

/// [`search_commits`] against the repo as it stood at `operation_hex`, for a
/// thread of its own: jj-lib's repo isn't `Send`, so the search loads its own.
pub(crate) async fn search_at_operation(
    settings: &jj_lib::settings::UserSettings,
    root: &Path,
    operation_hex: &str,
    needle: &str,
    log_revset: &str,
    cancel: &CancelFlag,
) -> Result<Option<Vec<CommitHit>>> {
    let workspace = super::workspace::load_workspace(settings, root)?;
    let loader = workspace.repo_loader();
    let operation = super::history::load_operation(loader, operation_hex).await?;
    let repo = loader
        .load_at(&operation)
        .await
        .context("failed to load the repo to search")?;
    search_commits(
        &repo,
        workspace.workspace_name(),
        root,
        needle,
        log_revset,
        cancel,
    )
    .await
}

/// The best [`MAX_HITS`] visible commits for `needle`, best first, each saying
/// whether `log_revset` holds it. `None` when cancelled.
pub(crate) async fn search_commits(
    repo: &ReadonlyRepo,
    workspace_name: &WorkspaceName,
    root: &Path,
    needle: &str,
    log_revset: &str,
    cancel: &CancelFlag,
) -> Result<Option<Vec<CommitHit>>> {
    let all = evaluate(repo, RevsetExpression::all()).context("failed to list the commits")?;
    let labels = ref_labels(repo.view(), workspace_name);
    let mut scorer = Scorer::new(needle);
    let mut best = Best::new(MAX_HITS);
    let mut text = String::new();
    let mut ids = all.stream();
    let mut order = 0;
    while let Some(id) = ids.next().await {
        let id = id.context("failed to walk the commits")?;
        let commit = repo
            .store()
            .get_commit_async(&id)
            .await
            .with_context(|| format!("failed to load jj commit {}", id.hex()))?;
        // As the walk shows it: the first line, trimmed.
        let description = commit.description().lines().next().unwrap_or("").trim();
        let change_id = commit.change_id().to_string();
        let commit_id = id.hex();
        let author = &commit.author().name;
        let refs = labels.get(&id).map(Vec::as_slice).unwrap_or_default();
        haystack(
            &change_id,
            &commit_id,
            (!description.is_empty()).then_some(description),
            author,
            refs.iter().map(String::as_str),
            &mut text,
        );
        if let Some(score) = scorer.score(&text) {
            best.offer(score, order, || CommitHit {
                change_id,
                commit_id,
                shortest_change_id_len: None,
                change_offset: None,
                description: description.to_owned(),
                author: author.clone(),
                score,
                in_log: false,
            });
        }
        order += 1;
        if order % CANCEL_STRIDE == 0 {
            if cancel.is_cancelled() {
                return Ok(None);
            }
            // A warm commit cache can carry this loop a long way without
            // awaiting anything that yields.
            tokio::task::yield_now().await;
        }
    }
    drop(ids);
    if cancel.is_cancelled() {
        return Ok(None);
    }

    // What the rows show, for the few kept: the prefixes and `/N` suffixes
    // the walk works out for every row, and whether the log holds them.
    let in_log = match parse_user_revset(root, repo.settings(), workspace_name, log_revset)
        .and_then(|expression| evaluate(repo, expression))
    {
        Ok(revset) => Some(revset),
        Err(error) => {
            tracing::warn!("the log revset is unavailable to the search: {error:#}");
            None
        }
    };
    let in_log = in_log.as_ref().map(|revset| revset.containing_fn());
    let prefix_context = super::revset::id_prefix_context(root, repo.settings(), workspace_name);
    let prefixes = prefix_context.populate(repo).ok();
    let mut hits = Vec::new();
    for (_, mut hit) in best.into_sorted() {
        let id = jj_lib::backend::CommitId::try_from_hex(&hit.commit_id)
            .context("a commit id the walk wrote")?;
        let commit = repo.store().get_commit_async(&id).await?;
        if let Some(index) = &prefixes {
            hit.shortest_change_id_len = index
                .shortest_change_prefix_len(repo, commit.change_id())
                .await
                .ok();
        }
        hit.change_offset = repo
            .resolve_change_id(commit.change_id())
            .await
            .ok()
            .flatten()
            .filter(|targets| targets.is_divergent() || !targets.has_visible(&id))
            .and_then(|targets| targets.find_offset(&id));
        if let Some(contains) = &in_log {
            hit.in_log = contains(&id).await.unwrap_or(false);
        }
        hits.push(hit);
    }
    Ok(Some(hits))
}

fn evaluate(
    repo: &ReadonlyRepo,
    expression: std::sync::Arc<UserRevsetExpression>,
) -> Result<Box<dyn jj_lib::revset::Revset + '_>> {
    let resolver = SymbolResolver::new(repo, &[] as &[Box<dyn SymbolResolverExtension>]);
    let resolved = expression
        .resolve_user_expression(repo, &resolver)
        .map_err(super::walk::describe_resolution_error)
        .context("failed to resolve the revset")?;
    resolved.evaluate().context("failed to evaluate the revset")
}

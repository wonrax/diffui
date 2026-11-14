//! The palette's search of all history against a live repository actor:
//! what it finds outside the log, how it ranks against the graph's own rows,
//! and that a cancelled search never answers.

use crate::harness;

use diffui_core::commit_search::{CommitHit, MAX_HITS, Scorer, haystack};
use diffui_core::repo::{Command, Payload};
use diffui_core::{MutationOp, RevisionSelection, StreamRow};
use harness::{OpBase, TestRepo, commit_file, commit_ids, scratch_repo};

fn search(repo: &mut TestRepo, needle: &str, revset: &str) -> Vec<CommitHit> {
    let (needle, revset) = (needle.to_owned(), revset.to_owned());
    let events = repo.run(|job| Command::SearchCommits {
        job,
        needle,
        revset,
    });
    match events.into_iter().next_back() {
        Some(Payload::CommitsFound { hits, .. }) => hits,
        other => panic!("expected CommitsFound, got {other:?}"),
    }
}

/// A repo with a long chain under `@`, a bookmark on its root and a divergent
/// change, so the search has rows past a narrow log and ids worth checking.
fn fixture(test: &str) -> (TestRepo, Vec<String>) {
    let root = scratch_repo(test);
    let mut repo = TestRepo::open(&root);
    commit_file(&mut repo, "a.txt", "a\n", "base of the chain");
    let base = commit_ids(&root)[1].clone();
    let chain = harness::new_chain(&root, &base, 300);
    harness::set_bookmark(&root, OpBase::Head, "deep-marker", &chain[3]);
    // Two copies of one change: a leaf described twice, concurrently.
    let leaf = harness::new_commit(&root, std::slice::from_ref(&base), "split");
    harness::describe(&root, OpBase::Head, &leaf, "split personality one");
    harness::describe(&root, OpBase::ParentOfHead, &leaf, "split personality two");
    repo.mutate(MutationOp::New {
        parent: RevisionSelection::Commit(chain[299].clone()),
    })
    .expect("@ on the chain's tip");
    (repo, chain)
}

/// The graph's rows scored the way the palette scores them, best first, ties
/// in no particular order: the same set the search should keep.
fn scored_rows(rows: &[StreamRow], needle: &str) -> Vec<(u16, String)> {
    let mut scorer = Scorer::new(needle);
    let mut text = String::new();
    let mut scored: Vec<(u16, String)> = rows
        .iter()
        .filter_map(|row| {
            let summary = &row.summary;
            haystack(
                &summary.change_id,
                &summary.commit_id,
                summary
                    .has_description
                    .then_some(summary.description.as_str()),
                &summary.author,
                summary.bookmarks.iter().map(String::as_str),
                &mut text,
            );
            scorer
                .score(&text)
                .map(|score| (score, summary.commit_id.clone()))
        })
        .collect();
    scored.sort_by(|a, b| b.cmp(a));
    scored
}

#[test]
fn a_history_search_finds_what_the_log_leaves_out() {
    let (mut repo, chain) = fixture("search-outside-the-log");
    let log = "@ | @-";
    let loaded: Vec<String> = repo
        .graph(log)
        .into_iter()
        .map(|row| row.summary.commit_id)
        .collect();

    let hits = search(&mut repo, "chain 42", log);
    let deep = hits
        .iter()
        .find(|hit| hit.description == "chain 42")
        .expect("the deep commit is found");
    assert_eq!(deep.commit_id, chain[42]);
    assert!(!loaded.contains(&deep.commit_id), "it isn't a loaded row");
    assert!(!deep.in_log, "and the search says the log leaves it out");

    // Found by a bookmark's name, as a graph row's chip would be.
    let hits = search(&mut repo, "deep-marker", log);
    assert_eq!(hits.first().map(|hit| &hit.commit_id), Some(&chain[3]));

    // One the log holds says so.
    let hits = search(&mut repo, "chain 299", log);
    let tip = hits
        .iter()
        .find(|hit| hit.commit_id == chain[299])
        .expect("the tip is found");
    assert!(tip.in_log);
}

#[test]
fn a_history_search_ranks_as_the_graphs_rows_do() {
    let (mut repo, _) = fixture("search-ranks-like-the-graph");
    let rows = repo.graph("all()");
    assert!(rows.len() > MAX_HITS, "enough rows to cut");

    for needle in ["chain 1", "split", "base", "ch9"] {
        let hits = search(&mut repo, needle, "all()");
        let mut expected = scored_rows(&rows, needle);
        let cut = expected.len().min(MAX_HITS);
        // A tie at the cut may keep either of its rows; compare up to it.
        let floor = expected.get(cut.saturating_sub(1)).map(|(score, _)| *score);
        expected.truncate(cut);
        let mut found: Vec<(u16, String)> = hits
            .iter()
            .map(|hit| (hit.score, hit.commit_id.clone()))
            .collect();
        found.sort_by(|a, b| b.cmp(a));
        assert_eq!(found.len(), expected.len(), "{needle}");
        let above = |list: &[(u16, String)]| -> Vec<(u16, String)> {
            list.iter()
                .filter(|(score, _)| Some(*score) > floor)
                .cloned()
                .collect()
        };
        assert_eq!(above(&found), above(&expected), "{needle}");
        // Best first, as they arrive.
        assert!(hits.windows(2).all(|pair| pair[0].score >= pair[1].score));

        // What a row shows matches the graph's row for the same commit.
        for hit in &hits {
            let row = rows
                .iter()
                .find(|row| row.summary.commit_id == hit.commit_id)
                .expect("every visible commit is in all()");
            assert_eq!(hit.change_id, row.summary.change_id);
            assert_eq!(
                hit.shortest_change_id_len,
                row.summary.shortest_change_id_len
            );
            assert_eq!(hit.change_offset, row.summary.change_offset);
            assert_eq!(hit.author, row.summary.author);
            assert!(hit.in_log);
        }
    }
    let split = search(&mut repo, "split personality", "all()");
    assert_eq!(split.len(), 2, "{split:?}");
    assert!(split.iter().all(|hit| hit.change_offset.is_some()));
}

#[test]
fn a_cancelled_history_search_never_answers() {
    let (mut repo, _) = fixture("search-cancelled");
    let older = repo.send(|job| Command::SearchCommits {
        job,
        needle: "chain".to_owned(),
        revset: "all()".to_owned(),
    });
    // A newer query supersedes it: the caller cancels the old one behind it.
    let newer = repo.send(|job| Command::SearchCommits {
        job,
        needle: "chain 7".to_owned(),
        revset: "all()".to_owned(),
    });
    repo.handle.send(Command::Cancel { job: older });

    let answered = repo.drain(older);
    assert!(
        matches!(answered.as_slice(), [Payload::Cancelled { .. }]),
        "{answered:?}"
    );
    match repo.drain(newer).last() {
        Some(Payload::CommitsFound { hits, .. }) => {
            assert!(hits.iter().any(|hit| hit.description == "chain 7"));
        }
        other => panic!("expected the newer search's hits, got {other:?}"),
    }
}

/// How long a search of a long history takes, against the graph walk of the
/// same commits for scale. Ignored: building the history takes a while.
/// `DIFFUI_SEARCH_BENCH_COMMITS` sets its length (100k by default);
/// `DIFFUI_SEARCH_BENCH_REPO` measures an existing jj repo instead, searching
/// for `DIFFUI_SEARCH_BENCH_NEEDLE`.
#[test]
#[ignore = "a measurement, not a check"]
fn a_history_search_over_a_long_history() {
    let (mut repo, needle) = match std::env::var_os("DIFFUI_SEARCH_BENCH_REPO") {
        Some(root) => {
            let needle = std::env::var("DIFFUI_SEARCH_BENCH_NEEDLE")
                .unwrap_or_else(|_| "chain 33333".to_owned());
            (TestRepo::open(std::path::Path::new(&root)), needle)
        }
        None => {
            let count: usize = std::env::var("DIFFUI_SEARCH_BENCH_COMMITS")
                .ok()
                .and_then(|count| count.parse().ok())
                .unwrap_or(100_000);
            let root = scratch_repo("search-long-history");
            let mut repo = TestRepo::open(&root);
            commit_file(&mut repo, "a.txt", "a\n", "base");
            let base = commit_ids(&root)[1].clone();
            let started = std::time::Instant::now();
            harness::new_chain(&root, &base, count);
            eprintln!("built {count} commits in {:?}", started.elapsed());
            repo.snapshot();
            (repo, format!("chain {}", count / 3))
        }
    };
    for pass in ["cold", "warm"] {
        let started = std::time::Instant::now();
        let hits = search(&mut repo, &needle, "@");
        eprintln!(
            "{pass} search: {:?}, {} hits",
            started.elapsed(),
            hits.len()
        );
        assert!(!hits.is_empty());
    }
    let started = std::time::Instant::now();
    let rows = repo.graph("all()").len();
    eprintln!("graph walk of {rows} rows: {:?}", started.elapsed());
}

//! Typed revsets against a live repository actor: resolution with jj's errors
//! and their spans.

mod harness;

use std::path::Path;

use diffui_core::repo::{Command, Payload};
use diffui_core::revset::{RevsetError, RevsetExpect, RevsetMatch, RevsetQuery};
use diffui_core::{MutationOp, RevisionSelection};
use harness::{TestRepo, commit_file, commit_ids, full_description, scratch_repo, write};

/// The fixture every scenario here shares:
///
/// ```text
/// @  (empty)
/// C  a.txt: two → TWO
/// │ D  d.txt added
/// B ╯ b.txt added       (D's parent is A, so B and D are siblings)
/// A  a.txt added
/// ```
struct Fixture {
    repo: TestRepo,
    a: String,
    b: String,
    c: String,
    d: String,
    working_copy: String,
}

fn fixture(test: &str) -> Fixture {
    let root = scratch_repo(test);
    let mut repo = TestRepo::open(&root);
    commit_file(&mut repo, "a.txt", "one\ntwo\nthree\n", "A");
    commit_file(&mut repo, "b.txt", "bee\n", "B");
    commit_file(&mut repo, "a.txt", "one\nTWO\nthree\n", "C");
    let by_title = |root: &Path| {
        let mut found = std::collections::HashMap::new();
        for id in commit_ids(root) {
            if id.bytes().all(|b| b == b'0') {
                continue;
            }
            found.insert(full_description(root, &id), id);
        }
        found
    };
    let ids = by_title(&root);
    let (a, b, c) = (ids["A"].clone(), ids["B"].clone(), ids["C"].clone());

    // D: a sibling of B, then `@` back on top of C.
    repo.mutate(MutationOp::New {
        parent: RevisionSelection::Commit(a.clone()),
    })
    .expect("start D on A");
    write(&root, "d.txt", "dee\n");
    repo.snapshot();
    repo.mutate(MutationOp::Describe {
        target: RevisionSelection::WorkingCopy,
        description: "D".to_owned(),
    })
    .expect("describe D");
    repo.mutate(MutationOp::New {
        parent: RevisionSelection::Commit(c.clone()),
    })
    .expect("back on C");
    let ids = by_title(&root);
    let d = ids["D"].clone();
    let working_copy = ids[""].clone();
    Fixture {
        repo,
        a,
        b,
        c,
        d,
        working_copy,
    }
}

impl Fixture {
    fn resolve(&mut self, query: RevsetQuery) -> Result<RevsetMatch, RevsetError> {
        let events = self.repo.run(|job| Command::ResolveRevset { job, query });
        match events.into_iter().next_back() {
            Some(Payload::RevsetResolved { result, .. }) => result,
            other => panic!("expected RevsetResolved, got {other:?}"),
        }
    }
}

#[test]
fn revsets_resolve_to_counted_ids() {
    let mut fixture = fixture("revset-resolve");
    let (a, b, c, d, wc) = (
        fixture.a.clone(),
        fixture.b.clone(),
        fixture.c.clone(),
        fixture.d.clone(),
        fixture.working_copy.clone(),
    );
    let found = fixture.resolve(RevsetQuery::new("::@ ~ root()")).unwrap();
    assert_eq!(found.commits, [wc.clone(), c.clone(), b.clone(), a.clone()]);
    assert_eq!(found.count, 4);
    assert!(found.count_exact);
    assert_eq!(found.count_label(), "4 revisions");

    // Capped ids, exact count.
    let mut query = RevsetQuery::new("all()");
    query.limit = 2;
    let found = fixture.resolve(query).unwrap();
    assert_eq!(found.commits.len(), 2);
    assert_eq!(found.count, 6);

    // The walk's aliases resolve here too.
    let found = fixture.resolve(RevsetQuery::new("trunk()")).unwrap();
    assert_eq!(found.count, 1, "trunk() falls back to root()");
    let found = fixture.resolve(RevsetQuery::new(format!("{d} | mutable()")));
    assert!(found.unwrap().commits.contains(&d));
}

#[test]
fn revset_errors_carry_jj_wording_and_spans() {
    let mut fixture = fixture("revset-errors");

    let error = fixture
        .resolve(RevsetQuery::new("@ | nosuch()"))
        .unwrap_err();
    assert_eq!(error.message, "Function `nosuch` doesn't exist");
    assert_eq!(error.span, Some(4..10), "the function name");

    let error = fixture.resolve(RevsetQuery::new("trunk()..")).map(|_| ());
    assert!(error.is_ok(), "a trailing range operator is valid");

    let error = fixture.resolve(RevsetQuery::new("trunk() |")).unwrap_err();
    assert_eq!(error.message, "Syntax error");
    assert_eq!(error.span, Some(9..9));

    let error = fixture.resolve(RevsetQuery::new("@- | xyzzy")).unwrap_err();
    assert_eq!(error.message, "Revision `xyzzy` doesn't exist");
    assert_eq!(error.span, Some(5..10));

    let error = fixture
        .resolve(RevsetQuery::new("all()").expect(RevsetExpect::Single))
        .unwrap_err();
    assert_eq!(
        error.message,
        "Revset `all()` resolved to more than one revision"
    );
    let error = fixture
        .resolve(RevsetQuery::new("none()").expect(RevsetExpect::Single))
        .unwrap_err();
    assert_eq!(
        error.message,
        "Revset `none()` didn't resolve to any revisions"
    );
    assert!(
        fixture
            .resolve(RevsetQuery::new("@").expect(RevsetExpect::Single))
            .is_ok()
    );
}

#[test]
fn graph_loads_report_revset_symbols() {
    let root = scratch_repo("revset-symbols");
    std::fs::write(
        harness::repo_dir(&root).join("config.toml"),
        "[revset-aliases]\n'stack(x, n)' = 'ancestors(x, n) & mutable()'\n'wip' = 'description(wip)'\n",
    )
    .expect("write the repo config");
    let mut repo = TestRepo::open(&root);
    let events = repo.run(|job| Command::LoadGraph {
        job,
        revset: "all()".to_owned(),
    });
    let Some(Payload::GraphLoaded { tail, .. }) = events.into_iter().next_back() else {
        panic!("expected the graph to load");
    };
    let names: Vec<String> = tail
        .symbols
        .aliases
        .iter()
        .map(|alias| alias.signature())
        .collect();
    assert_eq!(names[..2], ["stack(x, n)", "wip"], "user aliases first");
    assert!(names.contains(&"trunk()".to_owned()));
    assert!(names.contains(&"mutable()".to_owned()));
    assert_eq!(tail.symbols.workspaces, ["default"]);
}

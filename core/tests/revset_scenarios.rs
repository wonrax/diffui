//! Typed revsets against a live repository actor: resolution with jj's errors
//! and spans, `jj diff`'s argument rules, and range diffs checked against what
//! jj 0.45.1's own `jj diff --git -r` prints.
//!
//! The expected diffs are golden text captured from the real CLI on these
//! exact fixtures. Set `DIFFUI_GOLDEN_JJ` to a jj 0.45.1 binary to have each
//! test run it again and compare, which is how the goldens were made and how
//! to refresh them after a jj bump.

mod harness;

use std::path::Path;

use diffui_core::repo::{Command, Payload, RepoError};
use diffui_core::revset::{RevsetError, RevsetExpect, RevsetMatch, RevsetQuery};
use diffui_core::{
    DiffBase, DiffDocument, DiffFileStatus, DiffRequest, MutationOp, RevisionSelection,
    parse_unified_diff,
};
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

    fn range_diff(&mut self, revset: &str) -> Result<DiffDocument, RepoError> {
        let revset = revset.to_owned();
        let events = self.repo.run(|job| Command::LoadDiff {
            job,
            revision: RevisionSelection::WorkingCopy,
            base: DiffBase::Range(revset),
            expand: Vec::new(),
        });
        match events.into_iter().next_back() {
            Some(Payload::DiffLoaded {
                document, details, ..
            }) => {
                assert!(details.is_none(), "a range has no single header");
                Ok(document)
            }
            Some(Payload::Failed { error, .. }) => Err(error),
            other => panic!("expected a diff, got {other:?}"),
        }
    }

    fn resolve_diff(
        &mut self,
        request: DiffRequest,
    ) -> Result<(RevisionSelection, DiffBase), RepoError> {
        let events = self.repo.run(|job| Command::ResolveDiff { job, request });
        match events.into_iter().next_back() {
            Some(Payload::DiffResolved { revision, base, .. }) => Ok((revision, base)),
            Some(Payload::Failed { error, .. }) => Err(error),
            other => panic!("expected DiffResolved, got {other:?}"),
        }
    }

    /// Check `document` against jj's own `jj diff --git -r revset`.
    fn assert_matches_jj(&self, revset: &str, document: &DiffDocument, golden: &str) {
        if let Ok(jj) = std::env::var("DIFFUI_GOLDEN_JJ") {
            let live = run_jj(&jj, &self.repo.root, &["diff", "--git", "-r", revset]);
            assert_eq!(live, golden, "jj's output moved; refresh the golden");
        }
        let expected = parse_unified_diff(golden);
        // One line per file, hunk and row, so a mismatch reads like a diff.
        let shape = |document: &DiffDocument| {
            let mut lines = Vec::new();
            for file in &document.files {
                lines.push(format!("{} {:?}", file.path, file.status));
                for hunk in &file.hunks {
                    lines.push(format!("  {:?}", hunk_range(&hunk.header)));
                    for line in &hunk.lines {
                        lines.push(format!("    {:?} {}", line.kind, line.content));
                    }
                }
            }
            lines
        };
        assert_eq!(shape(document), shape(&expected), "diff of {revset}");
        assert_eq!(document.total_additions, expected.total_additions);
        assert_eq!(document.total_deletions, expected.total_deletions);
    }
}

/// A hunk header's line ranges. diffui writes git's short form (`+1` for one
/// line) where jj writes `+1,1`; the ranges are what has to agree.
fn hunk_range(header: &str) -> [usize; 4] {
    let mut numbers = [0, 1, 0, 1];
    let sides = header.split_whitespace().skip(1).take(2);
    for (index, side) in sides.enumerate() {
        let (start, len) = side[1..].split_once(',').unwrap_or((&side[1..], "1"));
        numbers[index * 2] = start.parse().expect("a hunk start");
        numbers[index * 2 + 1] = len.parse().expect("a hunk length");
    }
    numbers
}

/// Run the real CLI against a scratch repo with nobody's config but ours.
fn run_jj(jj: &str, root: &Path, args: &[&str]) -> String {
    let config = root.with_extension("jjconfig.toml");
    std::fs::write(
        &config,
        "[user]\nname = \"Scenario Test\"\nemail = \"scenario@example.com\"\n",
    )
    .expect("write the CLI config");
    let output = std::process::Command::new(jj)
        .args(args)
        .args(["--no-pager", "--color=never", "--ignore-working-copy"])
        .current_dir(root)
        .env("JJ_CONFIG", &config)
        .output()
        .expect("run jj");
    let mut text = String::from_utf8(output.stdout).expect("utf-8 output");
    text.push_str(&String::from_utf8(output.stderr).expect("utf-8 errors"));
    text
}

const GOLDEN_A_B: &str = "\
diff --git a/a.txt b/a.txt
new file mode 100644
index 0000000000..4cb29ea38f
--- /dev/null
+++ b/a.txt
@@ -0,0 +1,3 @@
+one
+two
+three
diff --git a/b.txt b/b.txt
new file mode 100644
index 0000000000..af9c6fd168
--- /dev/null
+++ b/b.txt
@@ -0,0 +1,1 @@
+bee
";

const GOLDEN_B_C: &str = "\
diff --git a/a.txt b/a.txt
index 4cb29ea38f..ddc897f039 100644
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
diff --git a/b.txt b/b.txt
new file mode 100644
index 0000000000..af9c6fd168
--- /dev/null
+++ b/b.txt
@@ -0,0 +1,1 @@
+bee
";

const GOLDEN_B_D: &str = "\
diff --git a/b.txt b/b.txt
new file mode 100644
index 0000000000..af9c6fd168
--- /dev/null
+++ b/b.txt
@@ -0,0 +1,1 @@
+bee
diff --git a/d.txt b/d.txt
new file mode 100644
index 0000000000..8fa56d0232
--- /dev/null
+++ b/d.txt
@@ -0,0 +1,1 @@
+dee
";

const GOLDEN_ROOT_A: &str = "\
diff --git a/a.txt b/a.txt
new file mode 100644
index 0000000000..4cb29ea38f
--- /dev/null
+++ b/a.txt
@@ -0,0 +1,3 @@
+one
+two
+three
";

#[test]
fn range_diffs_match_jj() {
    let mut fixture = fixture("revset-range-golden");
    let (a, b, c, d) = (
        fixture.a.clone(),
        fixture.b.clone(),
        fixture.c.clone(),
        fixture.d.clone(),
    );

    // Root and child: nothing between them.
    let revset = format!("{a}|{b}");
    let document = fixture.range_diff(&revset).expect("A|B diffs");
    fixture.assert_matches_jj(&revset, &document, GOLDEN_A_B);
    let range = document
        .range
        .as_ref()
        .expect("a range carries its commits");
    assert_eq!(range.revset, revset);
    assert_eq!(range.count, 2);
    assert_eq!(range.commits, [b.clone(), a.clone()], "newest first");
    assert_eq!(range.heads, std::slice::from_ref(&b));
    assert_eq!(range.roots, std::slice::from_ref(&a));
    assert_eq!(
        range.base,
        ["0".repeat(40)],
        "A's parent is the root commit"
    );
    assert!(!range.includes_working_copy);

    // A modification on top of an addition: from A's tree.
    let revset = format!("{b}|{c}");
    let document = fixture.range_diff(&revset).expect("B|C diffs");
    fixture.assert_matches_jj(&revset, &document, GOLDEN_B_C);
    assert_eq!(
        document.range.as_ref().unwrap().base,
        std::slice::from_ref(&a)
    );

    // Two heads, one shared parent: both sides merged.
    let revset = format!("{b}|{d}");
    let document = fixture.range_diff(&revset).expect("B|D diffs");
    fixture.assert_matches_jj(&revset, &document, GOLDEN_B_D);
    let range = document.range.as_ref().unwrap();
    assert_eq!(range.heads.len(), 2);
    assert_eq!(
        range.base,
        std::slice::from_ref(&a),
        "the shared parent counts once"
    );

    // The root commit as a root: from the empty tree.
    let revset = format!("root()|{a}");
    let document = fixture.range_diff(&revset).expect("root()|A diffs");
    fixture.assert_matches_jj(&revset, &document, GOLDEN_ROOT_A);
    assert!(document.range.as_ref().unwrap().base.is_empty());

    // An empty set diffs nothing, as jj does.
    let document = fixture.range_diff("none()").expect("none() diffs");
    assert!(document.files.is_empty());
    assert_eq!(document.range.as_ref().unwrap().count, 0);
}

#[test]
fn range_diffs_refuse_gaps_in_jj_words() {
    let mut fixture = fixture("revset-range-gaps");
    let (a, b, c) = (fixture.a.clone(), fixture.b.clone(), fixture.c.clone());
    let revset = format!("{a}|{c}");
    let expected = format!(
        "Cannot diff revsets with gaps in. Revision {} would need to be in the set.",
        &b[..12]
    );
    match fixture.range_diff(&revset) {
        Err(RepoError::Revset(message)) => assert_eq!(message, expected),
        other => panic!("expected the gap error, got {other:?}"),
    }
    if let Ok(jj) = std::env::var("DIFFUI_GOLDEN_JJ") {
        let live = run_jj(&jj, &fixture.repo.root, &["diff", "-r", &revset]);
        assert!(
            live.contains("Error: Cannot diff revsets with gaps in."),
            "{live}"
        );
        assert!(
            live.contains(&format!(
                "Hint: Revision {} would need to be in the set.",
                &b[..12]
            )),
            "{live}"
        );
    }
    // The same rule answers a query that asks for it, before Enter.
    let error = fixture
        .resolve(RevsetQuery::new(&revset).expect(RevsetExpect::Contiguous))
        .expect_err("a gap");
    assert_eq!(error.message, "Cannot diff revsets with gaps in.");
}

#[test]
fn range_diffs_follow_the_working_copy() {
    let mut fixture = fixture("revset-range-wc");
    let c = fixture.c.clone();
    let revset = format!("{c}::");
    let document = fixture.range_diff(&revset).expect("C:: diffs");
    let range = document.range.as_ref().unwrap();
    assert!(range.includes_working_copy);
    assert_eq!(range.count, 2);
    assert_eq!(document.files.len(), 1);

    // An edit on disk lands in the range on the next load.
    write(&fixture.repo.root, "e.txt", "new\n");
    fixture.repo.snapshot();
    let document = fixture.range_diff(&revset).expect("C:: diffs again");
    let paths: Vec<&str> = document
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(paths, ["a.txt", "e.txt"]);
}

#[test]
fn range_files_expand_and_read_both_sides() {
    let mut fixture = fixture("revset-range-files");
    let (b, c) = (fixture.b.clone(), fixture.c.clone());
    let base = DiffBase::Range(format!("{b}|{c}"));

    let events = fixture.repo.run(|job| Command::ExpandFile {
        job,
        revision: RevisionSelection::WorkingCopy,
        base: base.clone(),
        path: "a.txt".to_owned(),
    });
    match events.into_iter().next_back() {
        Some(Payload::FileExpanded {
            file: Some(file), ..
        }) => {
            assert_eq!(file.status, DiffFileStatus::Modified);
            assert_eq!((file.additions, file.deletions), (1, 1));
        }
        other => panic!("expected the expanded file, got {other:?}"),
    }

    let events = fixture.repo.run(|job| Command::FilePair {
        job,
        revision: RevisionSelection::WorkingCopy,
        path: "a.txt".to_owned(),
        old_path: None,
        base,
    });
    match events.into_iter().next_back() {
        Some(Payload::FilePairRead { old, new, .. }) => {
            assert_eq!(old.as_deref(), Some("one\ntwo\nthree\n"));
            assert_eq!(new.as_deref(), Some("one\nTWO\nthree\n"));
        }
        other => panic!("expected the file pair, got {other:?}"),
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
fn diff_arguments_resolve_like_jj_diff() {
    let mut fixture = fixture("revset-diff-request");
    let (a, b, c) = (fixture.a.clone(), fixture.b.clone(), fixture.c.clone());

    // One commit is that commit's own diff.
    let resolved = fixture.resolve_diff(DiffRequest::Revisions(vec![b.clone()]));
    assert_eq!(
        resolved.unwrap(),
        (RevisionSelection::Commit(b.clone()), DiffBase::Parents)
    );
    // No `-r` is `@`, addressed as the working copy.
    let resolved = fixture.resolve_diff(DiffRequest::Revisions(Vec::new()));
    assert_eq!(
        resolved.unwrap(),
        (RevisionSelection::WorkingCopy, DiffBase::Parents)
    );
    // Several `-r`s are a range of their union, the cursor on its newest.
    let resolved = fixture.resolve_diff(DiffRequest::Revisions(vec![a.clone(), b.clone()]));
    assert_eq!(
        resolved.unwrap(),
        (
            RevisionSelection::Commit(b.clone()),
            DiffBase::Range(format!("({a}) | ({b})"))
        )
    );
    let resolved = fixture.resolve_diff(DiffRequest::Revisions(vec![format!("{c}::")]));
    assert_eq!(
        resolved.unwrap(),
        (
            RevisionSelection::WorkingCopy,
            DiffBase::Range(format!("{c}::"))
        )
    );
    // A gap is refused up front.
    let resolved = fixture.resolve_diff(DiffRequest::Revisions(vec![format!("{a}|{c}")]));
    assert!(
        matches!(resolved, Err(RepoError::Revset(message)) if message.starts_with("Cannot diff revsets with gaps in."))
    );

    // `--from`/`--to` take revsets naming one commit each.
    let resolved = fixture.resolve_diff(DiffRequest::FromTo {
        from: Some(format!("{c}-")),
        to: None,
        interdiff: false,
    });
    assert_eq!(
        resolved.unwrap(),
        (
            RevisionSelection::WorkingCopy,
            DiffBase::Revision(RevisionSelection::Commit(b.clone()))
        )
    );
    let resolved = fixture.resolve_diff(DiffRequest::FromTo {
        from: Some(a.clone()),
        to: Some(c.clone()),
        interdiff: true,
    });
    assert_eq!(
        resolved.unwrap(),
        (
            RevisionSelection::Commit(c.clone()),
            DiffBase::Interdiff(RevisionSelection::Commit(a.clone()))
        )
    );
    let resolved = fixture.resolve_diff(DiffRequest::FromTo {
        from: Some("all()".to_owned()),
        to: None,
        interdiff: false,
    });
    assert!(
        matches!(&resolved, Err(RepoError::Revset(message)) if message == "Revset `all()` resolved to more than one revision"),
        "{resolved:?}"
    );
    let resolved = fixture.resolve_diff(DiffRequest::FromTo {
        from: None,
        to: Some("nope".to_owned()),
        interdiff: false,
    });
    assert!(
        matches!(&resolved, Err(RepoError::Revset(message)) if message == "Revision `nope` doesn't exist"),
        "{resolved:?}"
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

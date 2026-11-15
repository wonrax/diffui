//! A pull request's review threads, read from its forge and laid into the
//! diff with the notes' cards: in the pull request's own tab (its whole
//! diff), and on the revision whose bookmark has it. Read-only: they are
//! never stored, and nothing here writes to the forge.
//!
//! They are read when that diff is on screen and kept per pull request,
//! read again once the tab's pull requests refresh (a repository tab) or
//! its commits reload (a pull request tab). A read that fails or never
//! answers only leaves them out.

use std::collections::HashMap;
use std::rc::Rc;

use diffui_core::forge::{ForgeAbility, ForgePr, ReviewThread};
use diffui_core::notes::Side;
use diffui_core::{DiffFile, DiffLineKind};

use super::app::{Note, shown_change};
use super::locate::{Located, Place};
use super::store::{Anchor, Lines, Message, Resolved, Role, Subject, Thread};
use crate::diff_view::{DocSpot, LineAt};
use crate::root::PaneSet;
use crate::{Cx, Diffui, MainView, TabSource};

/// The review threads of the pull requests a tab showed, by pull request.
pub(crate) type Cache = HashMap<ForgePr, Fetched>;

/// One pull request's threads, as last read.
#[derive(Debug, Default)]
pub(crate) struct Fetched {
    /// What they were read for: a newer epoch reads them again.
    epoch: u64,
    pub(crate) threads: Rc<Vec<Shown>>,
}

/// A comment's Markdown, parsed.
pub(crate) type Blocks = Rc<Vec<crate::markdown::Block>>;

/// A thread, its comments' Markdown parsed once.
#[derive(Debug)]
pub(crate) struct Shown {
    pub(crate) thread: ReviewThread,
    pub(crate) blocks: Vec<Blocks>,
}

/// The pull request whose diff is on screen: a pull request tab showing
/// all its changes, or a jj revision with a local bookmark that has one.
pub(crate) fn shown_pull_request(ui: &Diffui) -> Option<ForgePr> {
    let tab = ui.tabs.get(ui.active)?;
    let state = &tab.state;
    if state.main_view != MainView::Diff {
        return None;
    }
    match &tab.source {
        // Its commits' own diffs aren't what the comments are on.
        TabSource::ForgePr(pr) => {
            (state.scope() == Some(crate::review::Scope::All)).then(|| pr.clone())
        }
        TabSource::Repo { .. } => {
            shown_change(state)?;
            let row = state
                .session
                .commits
                .find(&state.session.selected_revision)?;
            row.bookmarks()
                .iter()
                .filter_map(|label| crate::forge::local_name(label))
                .find_map(|name| state.forge.of(name))
                .map(|decoration| decoration.remote.pull_request(decoration.pr.number))
        }
        TabSource::Diff(_) => None,
    }
}

/// When a tab's threads are read again: its pull requests' refreshes (a
/// repository tab), or its commits' loads (a pull request tab).
fn epoch(ui: &Diffui) -> u64 {
    let Some(tab) = ui.tabs.get(ui.active) else {
        return 0;
    };
    match tab.source {
        TabSource::ForgePr(_) | TabSource::Diff(_) => {
            tab.state.session.graph_version().map_or(0, |job| job.0)
        }
        TabSource::Repo { .. } => tab.state.forge.generation(),
    }
}

impl Diffui {
    /// Read the review threads of the pull request on screen, unless they
    /// were read for this epoch already. Runs on every reconcile.
    pub(crate) fn sync_review_comments(&mut self, cx: &mut Cx) {
        let (Some(tab), Some(pr)) = (self.active_tab_id(), shown_pull_request(self)) else {
            return;
        };
        let Some(provider) = self
            .forge
            .forges
            .provider(pr.kind)
            .filter(|provider| provider.can(ForgeAbility::ReviewComments))
        else {
            return;
        };
        // 0: the tab hasn't read its pull requests or its commits yet.
        let epoch = epoch(self);
        let fetched = self
            .active_mut()
            .forge
            .comments
            .entry(pr.clone())
            .or_default();
        if epoch == 0 || fetched.epoch == epoch {
            return;
        }
        // What was read before stays on screen until the new read lands.
        fetched.epoch = epoch;
        let asked = pr.clone();
        Self::run_async_on(
            async move { provider.review_comments(asked).await },
            PaneSet::MAIN,
            cx,
            move |this, read, _| {
                let Some(fetched) = this
                    .tab_mut(tab)
                    .and_then(|state| state.forge.comments.get_mut(&pr))
                    .filter(|fetched| fetched.epoch == epoch)
                else {
                    return;
                };
                match read {
                    Ok(threads) => fetched.threads = shown(threads),
                    Err(error) => {
                        tracing::info!(
                            "couldn't read {}'s review comments: {error}",
                            pr.reference()
                        );
                    }
                }
            },
        );
    }
}

/// The review threads of the pull request on screen, each where it goes in
/// the diff, as a note the cards draw, with its comments' Markdown. Threads
/// on files the diff doesn't have are left out.
pub(crate) fn placed(ui: &Diffui) -> Vec<(DocSpot, Note, Vec<Blocks>)> {
    let Some(pr) = shown_pull_request(ui) else {
        return Vec::new();
    };
    let tab = ui.active();
    let Some(fetched) = tab.forge.comments.get(&pr) else {
        return Vec::new();
    };
    let files = &tab.session.document.files;
    let mut placed = Vec::new();
    for shown in fetched.threads.iter() {
        let thread = &shown.thread;
        let Some((spot, place)) = place(files, thread) else {
            continue;
        };
        let note = Note {
            thread: as_thread(thread),
            located: Located { now: None, place },
        };
        placed.push((spot, note, shown.blocks.clone()));
    }
    placed
}

/// Where `thread` goes in `files`: under its last line when that line is in
/// a hunk and still reads as it did; at its file's header when its lines
/// aren't in a hunk, it is on the whole file, or its lines changed (then
/// outdated). `None` when the diff hasn't its file.
fn place(files: &[DiffFile], thread: &ReviewThread) -> Option<(DocSpot, Place)> {
    let file = files.iter().position(|file| {
        file.path == thread.path || file.old_path.as_deref() == Some(&thread.path)
    })?;
    let header = |outdated| Some((DocSpot::File(file), Place::File { outdated }));
    let Some(end) = thread.line.filter(|_| !thread.outdated) else {
        return header(thread.outdated);
    };
    let start = thread.start_line.unwrap_or(end).min(end);
    for (hunk, view) in files[file].hunks.iter().enumerate() {
        for (line, row) in view.lines.iter().enumerate() {
            let number = match thread.side {
                Side::New if row.kind != DiffLineKind::Deletion => row.new_line,
                Side::Old if row.kind != DiffLineKind::Addition => row.old_line,
                _ => None,
            };
            if number != Some(end) {
                continue;
            }
            if thread
                .text
                .last()
                .is_some_and(|text| line_text(text) != line_text(&row.content))
            {
                return header(true);
            }
            return Some((
                DocSpot::Line(LineAt { file, hunk, line }),
                Place::Lines { start, end },
            ));
        }
    }
    Some((DocSpot::File(file), Place::Lines { start, end }))
}

fn line_text(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

/// `thread` in the notes' shape, for the card: its comments as messages,
/// its lines as the anchor. Never stored; its subject says nothing.
fn as_thread(thread: &ReviewThread) -> Thread {
    let end = thread.line.or(thread.original_line);
    let lines = end.filter(|_| !thread.text.is_empty()).map(|end| Lines {
        side: thread.side,
        start: end + 1 - thread.text.len().min(end),
        text: thread.text.clone(),
    });
    Thread {
        version: super::store::VERSION,
        id: thread.url.clone(),
        subject: Subject::Change {
            change_id: String::new(),
            commit_id: String::new(),
            title: String::new(),
        },
        anchor: Some(Anchor {
            path: thread.path.clone(),
            lines,
        }),
        resolved: thread.resolved.then(|| Resolved {
            by: String::new(),
            at: 0,
        }),
        messages: thread
            .comments
            .iter()
            .map(|comment| Message {
                author: comment.author.clone(),
                role: Role::Agent,
                at: comment
                    .created_millis
                    .map_or(0, |millis| (millis / 1000) as u64),
                body: comment.body.clone(),
            })
            .collect(),
    }
}

/// `threads`, their comments' Markdown parsed.
fn shown(threads: Vec<ReviewThread>) -> Rc<Vec<Shown>> {
    Rc::new(
        threads
            .into_iter()
            .map(|thread| Shown {
                blocks: thread
                    .comments
                    .iter()
                    .map(|comment| Rc::new(crate::markdown::parse(&comment.body)))
                    .collect(),
                thread,
            })
            .collect(),
    )
}

/// `threads` as a read leaves them, for a scene to show without a forge.
#[cfg(test)]
pub(crate) fn fetched(threads: Vec<ReviewThread>) -> Fetched {
    Fetched {
        epoch: 0,
        threads: shown(threads),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use diffui_core::forge::{ReviewComment, patch_to_file};
    use diffui_core::{DiffFileStatus, RevisionSelection};

    use super::*;

    fn thread(path: &str, side: Side, line: Option<usize>, text: &[&str]) -> ReviewThread {
        ReviewThread {
            path: path.to_owned(),
            side,
            line,
            start_line: None,
            original_line: line,
            text: text.iter().map(|line| (*line).to_owned()).collect(),
            outdated: false,
            resolved: false,
            url: format!("https://github.com/team/app/pull/1#{path}"),
            comments: vec![ReviewComment {
                author: "ada".into(),
                body: "Why?".into(),
                created_millis: None,
            }],
        }
    }

    fn files() -> Vec<DiffFile> {
        vec![patch_to_file(
            "src/a.rs".into(),
            None,
            DiffFileStatus::Modified,
            Some("@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n@@ -10,1 +10,1 @@\n-ten\n+TEN"),
        )]
    }

    #[test]
    fn a_thread_sits_under_its_line_while_the_line_reads_as_it_did() {
        let files = files();
        let at = |hunk, line| {
            DocSpot::Line(LineAt {
                file: 0,
                hunk,
                line,
            })
        };
        let new = thread("src/a.rs", Side::New, Some(2), &["TWO"]);
        assert_eq!(
            place(&files, &new),
            Some((at(0, 2), Place::Lines { start: 2, end: 2 }))
        );
        let old = thread("src/a.rs", Side::Old, Some(2), &["two"]);
        assert_eq!(
            place(&files, &old),
            Some((at(0, 1), Place::Lines { start: 2, end: 2 }))
        );
        // The line moved under it since: outdated, at the file's header.
        let moved = thread("src/a.rs", Side::New, Some(10), &["TWO"]);
        assert_eq!(
            place(&files, &moved),
            Some((DocSpot::File(0), Place::File { outdated: true }))
        );
        // Out of the hunks: at the header, its lines said there.
        let unseen = thread("src/a.rs", Side::New, Some(6), &["six"]);
        assert_eq!(
            place(&files, &unseen),
            Some((DocSpot::File(0), Place::Lines { start: 6, end: 6 }))
        );
        let outdated = ReviewThread {
            outdated: true,
            ..thread("src/a.rs", Side::New, None, &["SEVEN"])
        };
        assert_eq!(
            place(&files, &outdated),
            Some((DocSpot::File(0), Place::File { outdated: true }))
        );
        let file = thread("src/a.rs", Side::New, None, &[]);
        assert_eq!(
            place(&files, &file),
            Some((DocSpot::File(0), Place::File { outdated: false }))
        );
        assert_eq!(
            place(&files, &thread("src/b.rs", Side::New, Some(1), &[])),
            None
        );
    }

    #[test]
    fn a_thread_reads_as_a_note_with_its_lines_and_comments() {
        let mut review = thread("src/a.rs", Side::New, Some(3), &["two", "three"]);
        review.resolved = true;
        let note = as_thread(&review);
        assert_eq!(note.id, review.url);
        let lines = note
            .anchor
            .as_ref()
            .and_then(|anchor| anchor.lines.as_ref());
        assert_eq!(lines.map(|lines| (lines.start, lines.end())), Some((2, 3)));
        assert!(note.resolved.is_some());
        assert_eq!(note.messages[0].author, "ada");
    }

    /// `team/app#1`'s review threads, on `stack-1`'s one line: a comment
    /// there with a reply, and an outdated one.
    pub(crate) const THREADS: &str = r#"{"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": [
      {"path": "stack-one.txt", "line": 1, "startLine": null, "originalLine": 1, "diffSide": "RIGHT",
       "isOutdated": false, "isResolved": false, "comments": {"nodes": [
         {"author": {"login": "ada"}, "body": "Is **one** enough?", "createdAt": "2026-11-14T09:30:00Z",
          "url": "https://github.invalid/team/app/pull/1#discussion_r1", "diffHunk": "@@ -0,0 +1 @@\n+one"},
         {"author": {"login": "lin"}, "body": "For now.", "createdAt": "2026-11-14T10:30:00Z",
          "url": "https://github.invalid/team/app/pull/1#discussion_r2", "diffHunk": "@@ -0,0 +1 @@\n+one"}]}},
      {"path": "stack-one.txt", "line": null, "startLine": null, "originalLine": 1, "diffSide": "RIGHT",
       "isOutdated": true, "isResolved": false, "comments": {"nodes": [
         {"author": {"login": "ada"}, "body": "Was this uno?", "createdAt": "2026-11-13T09:30:00Z",
          "url": "https://github.invalid/team/app/pull/1#discussion_r3", "diffHunk": "@@ -0,0 +1 @@\n+uno"}]}}],
      "pageInfo": {"hasNextPage": false, "endCursor": null}}}}}}"#;

    /// `stack-1`'s diff on screen, its pull request badged.
    fn show_stack_one(h: &mut crate::harness::Harness) {
        h.wait_until("the badge", |ui| ui.active().forge.of("stack-1").is_some());
        let commit = h.read(|ui| ui.active().forge.of("stack-1").unwrap().target.clone());
        h.perform(crate::Action::SelectRevision(RevisionSelection::Commit(
            commit,
        )));
        h.wait_until("stack-1's diff", |ui| {
            let files = &ui.active().session.document.files;
            files.iter().any(|file| file.path == "stack-one.txt")
        });
    }

    fn thread_reads(fake: &crate::forge_dialog::tests::FakeCli) -> usize {
        fake.calls()
            .iter()
            .filter(|call| call.iter().any(|arg| arg.contains("reviewThreads")))
            .count()
    }

    fn fake(name: &str, threads: (&str, i32)) -> crate::forge_dialog::tests::FakeCli {
        use crate::forge_dialog::tests::{FakeCli, listed};
        let fake = FakeCli::new(name, &[]);
        fake.answer_or_fail(&[
            ("pr list *", &listed(&[(1, "stack-1", "main")]), 0),
            ("api graphql *reviewThreads*", threads.0, threads.1),
            ("api graphql *", "PENDING\n", 0),
        ]);
        fake
    }

    #[test]
    fn review_threads_show_on_the_revision_whose_bookmark_has_the_pull_request() {
        let fake = fake("threads-shown", (THREADS, 0));
        let (mut h, _) = crate::forge_dialog::tests::stacked_repo(&fake, "");
        show_stack_one(&mut h);
        h.wait_until("the review threads", |ui| placed(ui).len() == 2);
        let placed = h.read(|ui| {
            placed(ui)
                .into_iter()
                .map(|(spot, note, _)| (spot, note.located.place, note.thread.messages.len()))
                .collect::<Vec<_>>()
        });
        let file = h.read(|ui| {
            ui.active()
                .session
                .document
                .files
                .iter()
                .position(|file| file.path == "stack-one.txt")
                .unwrap()
        });
        assert_eq!(
            placed,
            [
                (
                    DocSpot::Line(LineAt {
                        file,
                        hunk: 0,
                        line: 0
                    }),
                    Place::Lines { start: 1, end: 1 },
                    2
                ),
                (DocSpot::File(file), Place::File { outdated: true }, 1),
            ]
        );
        // On the overview strip and stepped through in document order, the
        // outdated one at its file's header first; never in the counts.
        let line = DocSpot::Line(LineAt {
            file,
            hunk: 0,
            line: 0,
        });
        let reviews = h.read(|ui| {
            super::super::app::spots(ui)
                .into_iter()
                .map(|(spot, kind)| {
                    assert!(matches!(kind, super::super::app::SpotKind::Review { .. }));
                    spot
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(reviews, [DocSpot::File(file), line]);
        let step = |h: &mut crate::harness::Harness, by| {
            h.perform(crate::Action::Notes(super::super::NotesMsg::Step(by)));
            h.read(|ui| ui.active().notes.reveal.map(|(spot, _)| spot))
        };
        assert_eq!(step(&mut h, 1), Some(DocSpot::File(file)));
        assert_eq!(step(&mut h, 1), Some(line));
        assert_eq!(step(&mut h, -1), Some(DocSpot::File(file)));
        h.read(|ui| {
            assert_eq!(super::super::app::file_note_count(ui, file), 0);
            let change = ui.active().session.revision_details.as_ref().unwrap();
            let change = change.change_id.as_deref().unwrap();
            assert_eq!(ui.active().notes.open_on_change(change), 0);
        });
        // Opened, the card says where it's from and offers the forge, not a
        // reply.
        let id = "https://github.invalid/team/app/pull/1#discussion_r1";
        h.perform(crate::Action::Notes(super::super::NotesMsg::Toggle(
            id.into(),
        )));
        h.settle();
        let footer = h.bounds(gpui_kit::ElementId::Name(
            format!("note-{id}-footer").into(),
        ));
        assert!(footer.size.height > gpui_kit::px(0.0));
        // Read once, kept while nothing refreshes.
        h.settle();
        assert_eq!(thread_reads(&fake), 1);

        // GitHub off: no badge, so no threads, and nothing more is asked.
        h.perform(crate::Action::Plugin(crate::plugins::PluginAction::Enable(
            "github".into(),
            false,
        )));
        h.act(|ui, cx| {
            let tab = ui.active_tab_id().expect("a tab");
            ui.refresh_pull_requests(tab, false, cx);
        });
        h.wait_until("the threads to go", |ui| super::placed(ui).is_empty());
        h.settle();
        assert_eq!(thread_reads(&fake), 1);
    }

    #[test]
    fn a_failing_read_leaves_the_diff_without_threads() {
        let fake = fake("threads-failing", ("gh: HTTP 502 Bad Gateway\n", 1));
        let (mut h, _) = crate::forge_dialog::tests::stacked_repo(&fake, "");
        show_stack_one(&mut h);
        h.wait_until("the read", |_| thread_reads(&fake) == 1);
        h.settle();
        h.settle();
        assert!(h.read(|ui| placed(ui).is_empty()));
        assert!(h.read(|ui| ui.active().session.document.files.len()) > 0);
        assert_eq!(thread_reads(&fake), 1, "not asked again until a refresh");
    }

    #[test]
    fn review_threads_show_in_a_pull_requests_own_tab() {
        use crate::forge_dialog::tests::FakeCli;
        let diff = "diff --git a/stack-one.txt b/stack-one.txt\nnew file mode 100644\n--- /dev/null\n+++ b/stack-one.txt\n@@ -0,0 +1 @@\n+one\n";
        let fake = FakeCli::new("threads-tab", &[]);
        fake.answer_or_fail(&[
            (
                "pr view 1 --repo github.invalid/team/app --json number,title,*",
                "1\u{1f}Stack one\u{1f}ada\u{1f}OPEN\u{1f}main\u{1f}stack-1\u{1f}1\u{1f}0\u{1f}1\u{1f}https://github.invalid/team/app/pull/1\n",
                0,
            ),
            (
                "api graphql *commits(first:100*",
                &crate::plugins::github_scenarios::gh_commits(&[(
                    "aaa1",
                    "ada",
                    "Stack one",
                    Some("2026-11-14T09:30:00Z"),
                    1,
                    0,
                )]),
                0,
            ),
            ("pr diff 1 *", diff, 0),
            ("api graphql *reviewThreads*", THREADS, 0),
            (
                "api repos/team/app/commits/aaa1*",
                r#"{"files": [{"filename": "stack-one.txt", "status": "added", "additions": 1, "deletions": 0, "patch": "@@ -0,0 +1 @@\n+one"}]}"#,
                0,
            ),
        ]);
        let config = format!(
            "[plugins.github]\nprogram = \"{}\"\n",
            fake.program().display()
        );
        let mut h = crate::harness::Harness::open_fresh(None, (1280.0, 800.0), &config, "");
        h.wait_until("GitHub's plugin", |ui| {
            ui.plugins
                .get("github")
                .is_some_and(|found| matches!(found.state, crate::plugins::State::Running(_)))
        });
        h.perform(crate::Action::OpenRepo(
            "https://github.invalid/team/app/pull/1".into(),
        ));
        h.wait_until("the threads in the tab", |ui| placed(ui).len() == 2);
        let spots: Vec<DocSpot> =
            h.read(|ui| placed(ui).into_iter().map(|(spot, ..)| spot).collect());
        assert_eq!(
            spots,
            [
                DocSpot::Line(LineAt {
                    file: 0,
                    hunk: 0,
                    line: 0
                }),
                DocSpot::File(0),
            ]
        );
        // One of its commits: not what the threads are on.
        h.perform(crate::Action::SelectRevision(RevisionSelection::Commit(
            "aaa1".into(),
        )));
        h.wait_until("the commit's diff", |ui| {
            ui.active().session.selected_revision == RevisionSelection::Commit("aaa1".into())
        });
        h.settle();
        assert!(h.read(|ui| placed(ui).is_empty()));
        assert_eq!(thread_reads(&fake), 1);
    }
}

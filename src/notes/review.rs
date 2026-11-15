//! An AI review's findings as review notes: `output = "notes"` streams the
//! reply into the output panel, then files each finding that cites a line as
//! a note there, from the provider that wrote it. A reply with no findings
//! stays in the panel, as `panel` would leave it.

use std::path::PathBuf;

use diffui_core::RevisionSelection;
use diffui_core::notes::{NotesRepo, Side};

use super::store::{Anchor, Lines, Message, Role, Store, Thread, now};
use crate::{Cx, Diffui, TabId};

/// One finding: where it points and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    /// `None`: it cites no line.
    pub(crate) at: Option<(String, usize, usize)>,
    pub(crate) body: String,
}

/// The findings of a review written as a Markdown list, each item led by
/// its location: `` - `src/lib.rs:40-52`: why it matters ``. Bold, plain or
/// backticked locations all read; an item without one is a finding on the
/// change. Text outside the list is left out. Empty when it isn't a list.
pub(crate) fn parse_findings(text: &str) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if let Some(item) = list_item(trimmed).filter(|_| indent < 2) {
            let (at, rest) = match location(item) {
                Some((at, rest)) => (Some(at), rest),
                None => (None, item),
            };
            findings.push(Finding {
                at,
                body: rest.trim().to_owned(),
            });
            open = true;
        } else if open && !trimmed.is_empty() && indent >= 2 {
            let last = findings.last_mut().expect("an open item");
            last.body.push('\n');
            last.body.push_str(trimmed);
        } else {
            open = false;
        }
    }
    findings.retain(|finding| !finding.body.is_empty() || finding.at.is_some());
    findings
}

/// A list item's text after its marker.
fn list_item(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(rest);
    }
    let digits = line.find(|c: char| !c.is_ascii_digit())?;
    (digits > 0)
        .then(|| {
            line[digits..]
                .strip_prefix(". ")
                .or_else(|| line[digits..].strip_prefix(") "))
        })
        .flatten()
}

/// `path:40` or `path:40-52` leading `item`, however it's dressed, and the
/// rest of the item after it.
fn location(item: &str) -> Option<((String, usize, usize), &str)> {
    let dressing = |c: char| c == '`' || c == '*' || c == '_';
    let start = item.trim_start_matches(dressing);
    let end = start
        .find(|c: char| c.is_whitespace() || dressing(c))
        .unwrap_or(start.len());
    let cited = start[..end].trim_end_matches([':', ',']);
    let (path, lines) = cited.rsplit_once(':')?;
    let (first, last) = match lines.split_once(['-', '\u{2013}']) {
        Some((first, last)) => (
            first.parse().ok()?,
            last.trim_start_matches('L').parse().ok()?,
        ),
        None => {
            let line = lines.parse().ok()?;
            (line, line)
        }
    };
    let path = path.trim_start_matches("./");
    if path.is_empty() || first == 0 || last < first || path.contains("://") {
        return None;
    }
    let rest = start[end..]
        .trim_start_matches(dressing)
        .trim_start()
        .trim_start_matches([':', '-', '\u{2014}', '\u{2013}'])
        .trim_start();
    Some(((path.to_owned(), first, last), rest))
}

/// The threads `findings` make on the change `revision` names in `root`:
/// one per finding on lines the change has, and one on the change for the
/// rest.
fn threads_for(
    root: &std::path::Path,
    revision: &RevisionSelection,
    author: &str,
    findings: Vec<Finding>,
) -> Result<Vec<Thread>, String> {
    let repo = NotesRepo::open(root, false).map_err(|error| format!("{error:#}"))?;
    let revset = match revision {
        RevisionSelection::WorkingCopy => "@".to_owned(),
        RevisionSelection::Commit(id) => id.clone(),
    };
    let change = repo
        .resolve(&revset)?
        .into_iter()
        .next()
        .ok_or("the revision is gone")?;
    let thread = |anchor: Option<Anchor>, body: String| Thread {
        id: String::new(),
        change_id: change.change_id.clone(),
        commit_id: change.commit_id.clone(),
        title: change.title.clone(),
        anchor,
        resolved: None,
        messages: vec![Message {
            author: author.to_owned(),
            role: Role::Agent,
            at: now(),
            body,
        }],
    };
    let mut threads = Vec::new();
    let mut rest = Vec::new();
    for finding in findings {
        let placed = finding.at.as_ref().and_then(|(path, first, last)| {
            let text = repo.read(&change.commit_id, path, Side::New)?;
            let lines: Vec<&str> = text.lines().collect();
            let slice = lines.get(first - 1..*last)?;
            Some(Anchor {
                path: path.clone(),
                lines: Some(Lines {
                    side: Side::New,
                    start: *first,
                    text: slice.iter().map(|line| (*line).to_owned()).collect(),
                }),
            })
        });
        match placed {
            Some(anchor) => threads.push(thread(Some(anchor), finding.body)),
            None => rest.push(match finding.at {
                Some((path, first, last)) if first == last => {
                    format!("- `{path}:{first}`: {}", finding.body)
                }
                Some((path, first, last)) => format!("- `{path}:{first}-{last}`: {}", finding.body),
                None => format!("- {}", finding.body),
            }),
        }
    }
    if !rest.is_empty() {
        threads.push(thread(None, rest.join("\n")));
    }
    Ok(threads)
}

impl Diffui {
    /// File an AI review's reply as notes on `revision` from `author`. The
    /// reply stays in the panel, the way it streamed, when it has no
    /// findings or they can't be filed.
    pub(crate) fn file_review(
        &mut self,
        tab: TabId,
        revision: Option<RevisionSelection>,
        author: String,
        reply: &str,
        cx: &mut Cx,
    ) {
        let findings = parse_findings(reply);
        let state = self.tab(tab);
        let root: Option<PathBuf> = state
            .and_then(|state| state.repository.as_ref())
            .map(|repository| repository.root.clone());
        let store: Option<Store> = state.and_then(|state| state.notes.store.clone());
        let (Some(revision), Some(root), Some(store), false) = (
            revision,
            root,
            store,
            !findings.iter().any(|finding| finding.at.is_some()),
        ) else {
            return;
        };
        let (send, receive) = futures::channel::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("notes-review".to_owned())
            .spawn(move || {
                let filed = threads_for(&root, &revision, &author, findings).and_then(|threads| {
                    let count = threads.len();
                    threads
                        .into_iter()
                        .try_for_each(|thread| store.create(thread).map(|_| ()))
                        .map_err(|error| error.to_string())?;
                    Ok(count)
                });
                let _ = send.send(filed);
            });
        if spawned.is_err() {
            return;
        }
        Self::run_async(
            async move { receive.await.ok() },
            cx,
            move |this, filed, _| match filed {
                Some(Ok(count)) => {
                    if let Some(state) = this.tab_mut(tab) {
                        state.notes.mark_stale();
                    }
                    if this.is_active(tab) {
                        this.pop_mode(crate::modes::ModeKind::CliOutput);
                    }
                    let noun = if count == 1 { "note" } else { "notes" };
                    this.push_info_toast(
                        format!("The review left {count} {noun}"),
                        "They're in the diff, and in the Notes view.",
                    );
                }
                Some(Err(error)) => {
                    tracing::warn!("couldn't file the review as notes: {error}");
                }
                None => {}
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(path: &str, first: usize, last: usize) -> Option<(String, usize, usize)> {
        Some((path.to_owned(), first, last))
    }

    #[test]
    fn findings_read_however_the_location_is_dressed() {
        let reply = "Two things stand out.\n\n\
            - `src/lib.rs:40-52`: the lock is taken twice.\n  \
              That deadlocks under load.\n\
            - **src/main.rs:7** - `unwrap` on user input\n\
            1. src/old.rs:3: dead code\n\
            - The tests don't cover an empty file.\n\n\
            Otherwise fine.";
        assert_eq!(
            parse_findings(reply),
            [
                Finding {
                    at: at("src/lib.rs", 40, 52),
                    body: "the lock is taken twice.\nThat deadlocks under load.".to_owned(),
                },
                Finding {
                    at: at("src/main.rs", 7, 7),
                    body: "`unwrap` on user input".to_owned(),
                },
                Finding {
                    at: at("src/old.rs", 3, 3),
                    body: "dead code".to_owned(),
                },
                Finding {
                    at: None,
                    body: "The tests don't cover an empty file.".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn prose_has_no_findings() {
        assert!(parse_findings("Nothing stands out.").is_empty());
        assert!(parse_findings("See https://example.com:8080 for more.").is_empty());
    }
}

//! Notes as Markdown to paste elsewhere: into a coding agent's prompt first
//! of all, a pull request or a message too. The app's Copy and `diffui notes
//! export` both write it, so it reads the same wherever it's pasted.
//!
//! Each change gets a heading with its full change id (an agent can hand it
//! to jj), then each thread a heading naming where it is, the lines it's on
//! in a fence so the paste stands on its own, and its messages.

use super::locate::{Located, Place};
use super::store::{Role, Side, Thread};

/// `notes` as Markdown, grouped by change in the order the changes first
/// appear, each change's notes on the whole change first, then by file and
/// line.
pub(crate) fn markdown(notes: &[(&Thread, &Located)]) -> String {
    let mut changes: Vec<&str> = Vec::new();
    for (thread, _) in notes {
        if !changes.contains(&thread.change_id()) {
            changes.push(thread.change_id());
        }
    }
    let mut out = String::from("# Review notes\n");
    for change in changes {
        let mut threads: Vec<&(&Thread, &Located)> = notes
            .iter()
            .filter(|(thread, _)| thread.change_id() == change)
            .collect();
        threads.sort_by_key(|(thread, located)| order(thread, located));
        let (first, located) = threads[0];
        let title = located
            .now
            .as_ref()
            .map_or(first.title(), |now| now.title.as_str());
        let title = if title.is_empty() {
            "(no description)"
        } else {
            title
        };
        out.push_str(&format!("\n## Change `{change}`: {title}\n"));
        if located.archived() {
            out.push_str("\nThis change was abandoned.\n");
        }
        for (thread, located) in threads {
            out.push_str(&thread_markdown(thread, located));
        }
    }
    out
}

/// Notes on the whole change, then each file's by line.
fn order<'a>(thread: &'a Thread, located: &Located) -> (Option<&'a str>, usize) {
    let line = match (&located.place, &thread.anchor) {
        (Place::Lines { start, .. }, _) => *start,
        (_, Some(anchor)) => anchor.lines.as_ref().map_or(0, |lines| lines.start),
        _ => 0,
    };
    (
        thread.anchor.as_ref().map(|anchor| anchor.path.as_str()),
        line,
    )
}

fn thread_markdown(thread: &Thread, located: &Located) -> String {
    let mut heading = match &thread.anchor {
        None => "The whole change".to_owned(),
        Some(anchor) => match &anchor.lines {
            None => format!("`{}` (the whole file)", anchor.path),
            Some(lines) => {
                let (start, end) = match located.place {
                    Place::Lines { start, end } => (start, end),
                    _ => (lines.start, lines.end()),
                };
                let at = if start == end {
                    format!("`{}:{start}`", anchor.path)
                } else {
                    format!("`{}:{start}-{end}`", anchor.path)
                };
                match lines.side {
                    Side::New => at,
                    Side::Old => format!("{at}, before the change"),
                }
            }
        },
    };
    let mut states = Vec::new();
    if thread.resolved.is_some() {
        states.push("resolved");
    }
    if located.outdated() {
        states.push("outdated: these lines are no longer in the change");
    }
    if !states.is_empty() {
        heading.push_str(&format!(" ({})", states.join("; ")));
    }
    let mut out = format!("\n### {heading}\n");
    if let Some(lines) = thread
        .anchor
        .as_ref()
        .and_then(|anchor| anchor.lines.as_ref())
    {
        let fence = fence(&lines.text);
        out.push_str(&format!("\n{fence}\n"));
        for line in &lines.text {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&format!("{fence}\n"));
    }
    for message in &thread.messages {
        let who = match message.role {
            Role::User => "Reviewer",
            Role::Agent => message.author.as_str(),
        };
        out.push_str(&format!("\n**{who}:** {}\n", message.body.trim()));
    }
    out
}

/// A code fence longer than any run of backticks in `lines`.
fn fence(lines: &[String]) -> String {
    let longest = lines
        .iter()
        .flat_map(|line| line.split(|c| c != '`').map(str::len))
        .max()
        .unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::store::{Anchor, Lines, Message, Resolved};
    use diffui_core::notes::ChangeNow;

    fn change() -> ChangeNow {
        ChangeNow {
            change_id: "xmoupulpltnpupqoxomwvltzrtvyuslp".to_owned(),
            commit_id: "6671f1a9".to_owned(),
            title: "Add the parser".to_owned(),
        }
    }

    fn said(author: &str, role: Role, body: &str) -> Message {
        Message {
            author: author.to_owned(),
            role,
            at: 0,
            body: body.to_owned(),
        }
    }

    fn at(place: Place) -> Located {
        Located {
            now: Some(change()),
            place,
        }
    }

    #[test]
    fn line_file_and_change_notes_read_on_their_own() {
        let mut line = Thread::on_change(
            &change(),
            Some(Anchor {
                path: "src/parse.rs".to_owned(),
                lines: Some(Lines {
                    side: Side::New,
                    start: 40,
                    text: vec![
                        "let x = s.lock();".to_owned(),
                        "let y = s.lock();".to_owned(),
                    ],
                }),
            }),
            said("claude", Role::Agent, "Unsure about the lock order."),
        );
        line.messages
            .push(said("you", Role::User, "Take one lock.\nThen read both."));
        let mut file = Thread::on_change(
            &change(),
            Some(Anchor {
                path: "src/old.rs".to_owned(),
                lines: None,
            }),
            said("you", Role::User, "Delete this file."),
        );
        file.resolved = Some(Resolved {
            by: "you".to_owned(),
            at: 0,
        });
        let whole = Thread::on_change(&change(), None, said("codex", Role::Agent, "Split in two."));
        let gone = Thread::on_change(
            &change(),
            Some(Anchor {
                path: "src/a.rs".to_owned(),
                lines: Some(Lines {
                    side: Side::Old,
                    start: 7,
                    text: vec!["uses `x` here".to_owned()],
                }),
            }),
            said("claude", Role::Agent, "Removed."),
        );
        let (moved, plain, outdated) = (
            at(Place::Lines { start: 42, end: 43 }),
            at(Place::File { outdated: false }),
            at(Place::File { outdated: true }),
        );
        let notes = [
            (&line, &moved),
            (&file, &plain),
            (&whole, &at(Place::Change)),
            (&gone, &outdated),
        ];
        let notes: Vec<(&Thread, &Located)> = notes.iter().map(|(t, l)| (*t, *l)).collect();
        assert_eq!(
            markdown(&notes),
            "# Review notes

## Change `xmoupulpltnpupqoxomwvltzrtvyuslp`: Add the parser

### The whole change

**codex:** Split in two.

### `src/a.rs:7`, before the change (outdated: these lines are no longer in the change)

```
uses `x` here
```

**claude:** Removed.

### `src/old.rs` (the whole file) (resolved)

**Reviewer:** Delete this file.

### `src/parse.rs:42-43`

```
let x = s.lock();
let y = s.lock();
```

**claude:** Unsure about the lock order.

**Reviewer:** Take one lock.
Then read both.
"
        );
    }
}

//! Where each thread sits now: its change's current commit, and its lines in
//! that commit's files (see [`diffui_core::notes::reattach`]).

use std::collections::HashMap;

use diffui_core::notes::{ChangeNow, NotesRepo, Side, reattach};

use super::store::Thread;

/// Where a thread shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place {
    /// Its change was abandoned: the thread is archived.
    Archived,
    /// On the change as a whole.
    Change,
    /// At its file's header: a file note, or one whose lines are gone
    /// (`outdated`), which is never dropped.
    File { outdated: bool },
    /// On these lines (1-based, inclusive) of the anchor's side.
    Lines { start: usize, end: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Located {
    /// The change now, `None` when it's gone.
    pub(crate) now: Option<ChangeNow>,
    pub(crate) place: Place,
}

impl Located {
    pub(crate) fn outdated(&self) -> bool {
        matches!(self.place, Place::File { outdated: true })
    }

    pub(crate) fn archived(&self) -> bool {
        self.place == Place::Archived
    }
}

/// Where every thread of `threads` is in `repo` now, in the same order. Each
/// change is looked up once and each file read once.
pub(crate) fn locate(repo: &NotesRepo, threads: &[Thread]) -> Vec<Located> {
    let mut changes: HashMap<&str, Option<ChangeNow>> = HashMap::new();
    let mut files: HashMap<(String, String, Side), Option<String>> = HashMap::new();
    threads
        .iter()
        .map(|thread| {
            let now = changes
                .entry(thread.change_id.as_str())
                .or_insert_with(|| repo.change(&thread.change_id))
                .clone();
            let Some(change) = &now else {
                return Located {
                    now,
                    place: Place::Archived,
                };
            };
            let place = match &thread.anchor {
                None => Place::Change,
                Some(anchor) => {
                    let mut read = |side: Side| {
                        files
                            .entry((change.commit_id.clone(), anchor.path.clone(), side))
                            .or_insert_with(|| repo.read(&change.commit_id, &anchor.path, side))
                            .clone()
                    };
                    let file = match &anchor.lines {
                        Some(lines) => read(lines.side),
                        // A note on a file the change deleted is still on it.
                        None => read(Side::New).or_else(|| read(Side::Old)),
                    };
                    match (file, &anchor.lines) {
                        (None, _) => Place::File { outdated: true },
                        (Some(_), None) => Place::File { outdated: false },
                        (Some(file), Some(lines)) => {
                            match reattach(&lines.text, lines.start, &file) {
                                Some(start) => Place::Lines {
                                    start,
                                    end: start + lines.text.len() - 1,
                                },
                                None => Place::File { outdated: true },
                            }
                        }
                    }
                }
            };
            Located { now, place }
        })
        .collect()
}

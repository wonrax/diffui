//! The notes store: one JSON file per thread, in diffui's data directory,
//! one folder per jj repository.
//!
//! ```text
//! <data>/diffui/notes/<name>-<hash>/
//!   repo                 the repository's .jj/repo path, for people
//!   lock                 what writers lock
//!   threads/<id>.json    one thread
//! ```
//!
//! The folder is keyed by the repository every workspace of it shares, so an
//! agent in another `jj workspace` posts where the user reviews. Nothing goes
//! in the repository, and nothing is pushed.
//!
//! Writers (the CLI, the app, several agents) take an exclusive lock on
//! `lock`, read the thread, change it and rename the new file over the old.
//! Readers take no lock: a rename is atomic, so a reader sees a thread whole,
//! before or after. The app writes on a background thread, so a slow writer
//! never holds up a frame.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub(crate) use diffui_core::notes::Side;

/// Who wrote a message: an agent from its shell, or the user in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    Agent,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Message {
    /// The name the agent passed (`--as`), or "you" for the user.
    pub(crate) author: String,
    pub(crate) role: Role,
    /// Unix seconds.
    pub(crate) at: u64,
    /// Markdown.
    pub(crate) body: String,
}

/// Which lines a note is on, as they were when it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Lines {
    #[serde(with = "side_name")]
    pub(crate) side: Side,
    /// 1-based, the first of `text`.
    pub(crate) start: usize,
    /// The anchored lines' text, what [`diffui_core::notes::reattach`] looks for.
    pub(crate) text: Vec<String>,
}

impl Lines {
    pub(crate) fn end(&self) -> usize {
        self.start + self.text.len().saturating_sub(1)
    }
}

/// A file, and optionally lines of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Anchor {
    pub(crate) path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lines: Option<Lines>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Resolved {
    pub(crate) by: String,
    pub(crate) at: u64,
}

/// One note and its replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Thread {
    pub(crate) id: String,
    /// Full, in jj's reverse hex.
    pub(crate) change_id: String,
    /// The commit the note was written against.
    pub(crate) commit_id: String,
    /// The change's title then, shown once the change is gone.
    #[serde(default)]
    pub(crate) title: String,
    /// `None`: a note on the whole change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) anchor: Option<Anchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resolved: Option<Resolved>,
    pub(crate) messages: Vec<Message>,
}

impl Thread {
    pub(crate) fn last(&self) -> Option<&Message> {
        self.messages.last()
    }

    /// When anything last happened to it.
    pub(crate) fn activity(&self) -> u64 {
        let message = self.last().map_or(0, |message| message.at);
        self.resolved
            .as_ref()
            .map_or(message, |resolved| resolved.at.max(message))
    }

    /// Open, and the user had the last word: the agent's turn.
    pub(crate) fn waiting_on_agent(&self) -> bool {
        self.resolved.is_none() && self.last().is_some_and(|last| last.role == Role::User)
    }

    /// Open, and the agent had the last word: the user's turn to look.
    pub(crate) fn waiting_on_user(&self) -> bool {
        self.resolved.is_none() && self.last().is_some_and(|last| last.role == Role::Agent)
    }

    /// The agents that wrote in it.
    pub(crate) fn agents(&self) -> impl Iterator<Item = &str> {
        self.messages
            .iter()
            .filter(|message| message.role == Role::Agent)
            .map(|message| message.author.as_str())
    }
}

mod side_name {
    use super::Side;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(side: &Side, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match side {
            Side::Old => "old",
            Side::New => "new",
        })
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Side, D::Error> {
        match String::deserialize(d)?.as_str() {
            "old" => Ok(Side::Old),
            "new" => Ok(Side::New),
            other => Err(serde::de::Error::custom(format!("no side {other}"))),
        }
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// One repository's notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Store {
    dir: PathBuf,
}

/// FNV-1a: stable across builds, unlike std's hasher, and the folder name
/// must not move under an upgrade.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl Store {
    /// The store of the repository at `repo_dir` (a workspace's `.jj/repo`,
    /// resolved), in diffui's data directory.
    pub(crate) fn for_repo(repo_dir: &Path) -> Option<Self> {
        // Tests keep it where the harness's fresh launches leave it.
        #[cfg(test)]
        let root = crate::harness::scratch_home().join("repos/notes-store");
        #[cfg(not(test))]
        let root = crate::app_files::app_file(crate::app_files::AppDir::Data, "notes")?;
        Some(Self::in_dir(&root, repo_dir))
    }

    /// [`Store::for_repo`] under `root` instead of the data directory.
    pub(crate) fn in_dir(root: &Path, repo_dir: &Path) -> Self {
        // `<checkout>/.jj/repo`: the checkout's name reads best.
        let name: String = repo_dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(32)
            .collect();
        let hash = fnv(repo_dir.as_os_str().as_encoded_bytes());
        let store = Self {
            dir: root.join(format!("{name}-{hash:016x}")),
        };
        // For people browsing the folder; the hash is what keys it.
        if !store.dir.join("repo").exists() {
            let _ = fs::create_dir_all(&store.dir);
            let _ = fs::write(
                store.dir.join("repo"),
                repo_dir.as_os_str().as_encoded_bytes(),
            );
        }
        store
    }

    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the threads are: what to watch.
    pub(crate) fn threads_dir(&self) -> PathBuf {
        self.dir.join("threads")
    }

    fn path(&self, id: &str) -> PathBuf {
        self.threads_dir().join(format!("{id}.json"))
    }

    /// Every thread, in no order. Takes no lock. A file that doesn't read is
    /// skipped with a warning rather than hiding the rest.
    pub(crate) fn load(&self) -> Vec<Thread> {
        let Ok(entries) = fs::read_dir(self.threads_dir()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| {
                let path = entry.path();
                let bytes = fs::read(&path).ok()?;
                match serde_json::from_slice::<Thread>(&bytes) {
                    Ok(thread) => Some(thread),
                    Err(error) => {
                        tracing::warn!("skipping note {}: {error}", path.display());
                        None
                    }
                }
            })
            .collect()
    }

    pub(crate) fn get(&self, id: &str) -> Option<Thread> {
        let bytes = fs::read(self.path(id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Hold the writers' lock for as long as the guard lives.
    fn lock(&self) -> io::Result<File> {
        fs::create_dir_all(self.threads_dir())?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))?;
        file.lock()?;
        Ok(file)
    }

    fn write(&self, thread: &Thread) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(thread).map_err(io::Error::other)?;
        crate::app_files::write_atomic(&self.path(&thread.id), &bytes)
    }

    /// Store a new thread under a fresh id, which it returns.
    pub(crate) fn create(&self, mut thread: Thread) -> io::Result<String> {
        let _lock = self.lock()?;
        let mut seed = fnv(&now().to_le_bytes())
            ^ u64::from(std::process::id()).rotate_left(32)
            ^ fnv(thread.change_id.as_bytes());
        seed ^= SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| u64::from(since.subsec_nanos()));
        loop {
            let id = short_id(seed);
            if !self.path(&id).exists() {
                thread.id = id.clone();
                self.write(&thread)?;
                return Ok(id);
            }
            seed = fnv(&seed.to_le_bytes());
        }
    }

    /// Change thread `id` under the lock. `change` says whether it did
    /// anything; the file is written only then. `NotFound` when there's no
    /// such thread.
    pub(crate) fn update(
        &self,
        id: &str,
        change: impl FnOnce(&mut Thread) -> bool,
    ) -> io::Result<Thread> {
        let _lock = self.lock()?;
        let bytes = fs::read(self.path(id))?;
        let mut thread: Thread = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if change(&mut thread) {
            self.write(&thread)?;
        }
        Ok(thread)
    }

    /// Delete the threads `ids` names; how many there were.
    pub(crate) fn delete(&self, ids: &[String]) -> io::Result<usize> {
        let _lock = self.lock()?;
        let mut deleted = 0;
        for id in ids {
            match fs::remove_file(self.path(id)) {
                Ok(()) => deleted += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(deleted)
    }
}

/// Six characters from an alphabet without look-alikes.
fn short_id(seed: u64) -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut value = seed;
    (0..6)
        .map(|_| {
            let c = ALPHABET[(value % ALPHABET.len() as u64) as usize] as char;
            value /= ALPHABET.len() as u64;
            c
        })
        .collect()
}

/// A reply to `thread`.
pub(crate) fn reply(thread: &mut Thread, author: &str, role: Role, body: &str) {
    thread.messages.push(Message {
        author: author.to_owned(),
        role,
        at: now(),
        body: body.to_owned(),
    });
    // The user answering a resolved thread wants it looked at again.
    if role == Role::User {
        thread.resolved = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("diffui-notes-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    pub(crate) fn thread(change: &str, body: &str) -> Thread {
        Thread {
            id: String::new(),
            change_id: change.to_owned(),
            commit_id: "00".to_owned(),
            title: String::new(),
            anchor: None,
            resolved: None,
            messages: vec![Message {
                author: "claude".to_owned(),
                role: Role::Agent,
                at: now(),
                body: body.to_owned(),
            }],
        }
    }

    #[test]
    fn a_thread_round_trips_and_ids_are_unique() {
        let root = scratch("round-trip");
        let store = Store::in_dir(&root, Path::new("/work/app/.jj/repo"));
        assert!(
            store
                .dir()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("app-")
        );
        let mut ids = std::collections::HashSet::new();
        for n in 0..50 {
            ids.insert(store.create(thread("k", &format!("note {n}"))).unwrap());
        }
        assert_eq!(ids.len(), 50);
        assert_eq!(store.load().len(), 50);
        let id = ids.into_iter().next().unwrap();
        let updated = store
            .update(&id, |thread| {
                reply(thread, "you", Role::User, "why?");
                true
            })
            .unwrap();
        assert!(updated.waiting_on_agent());
        assert_eq!(store.get(&id), Some(updated));
        assert_eq!(store.delete(&[id.clone(), "nope".to_owned()]).unwrap(), 1);
        assert!(store.get(&id).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// Writers in several threads, each with its own handle as separate
    /// processes would have: every reply lands, and every file reads whole.
    #[test]
    fn concurrent_writers_lose_nothing() {
        let root = scratch("concurrent");
        let store = Store::in_dir(&root, Path::new("/work/app/.jj/repo"));
        let id = store.create(thread("k", "first")).unwrap();
        let writers: Vec<_> = (0..8)
            .map(|writer| {
                let store = Store::in_dir(&root, Path::new("/work/app/.jj/repo"));
                let id = id.clone();
                std::thread::spawn(move || {
                    for n in 0..25 {
                        store
                            .update(&id, |thread| {
                                reply(thread, &format!("w{writer}"), Role::Agent, &n.to_string());
                                true
                            })
                            .unwrap();
                        store.create(thread("k", "more")).unwrap();
                    }
                })
            })
            .collect();
        // A reader racing them never sees a torn file.
        for _ in 0..50 {
            for thread in store.load() {
                assert!(!thread.messages.is_empty());
            }
        }
        for writer in writers {
            writer.join().unwrap();
        }
        assert_eq!(store.get(&id).unwrap().messages.len(), 1 + 8 * 25);
        assert_eq!(store.load().len(), 1 + 8 * 25);
        let _ = fs::remove_dir_all(&root);
    }
}

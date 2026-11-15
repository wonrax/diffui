//! Notes on a diff opened without a repository (a patch, a compare): no
//! store and no change id to key them by, so they live in memory, owned by
//! the tab, and go with it. Agents can't reach them through `diffui notes`.

use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use diffui_core::notes::ChangeNow;

use super::store::{Source, Subject, Thread};

#[derive(Debug, Default)]
pub(crate) struct Memory {
    threads: Mutex<Vec<Thread>>,
}

impl Memory {
    fn threads(&self) -> std::sync::MutexGuard<'_, Vec<Thread>> {
        self.threads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Source for Memory {
    fn load(&self) -> Vec<Thread> {
        self.threads().clone()
    }

    fn create(&self, mut thread: Thread) -> io::Result<String> {
        let mut threads = self.threads();
        // Short and unique within the tab: nothing else ever sees them.
        let id = (1..)
            .map(|n: usize| format!("n{n}"))
            .find(|id| threads.iter().all(|thread| &thread.id != id))
            .expect("an unused id");
        thread.id = id.clone();
        threads.push(thread);
        Ok(id)
    }

    fn update(&self, id: &str, change: &mut dyn FnMut(&mut Thread) -> bool) -> io::Result<Thread> {
        let mut threads = self.threads();
        let thread = threads
            .iter_mut()
            .find(|thread| thread.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no note {id}")))?;
        change(thread);
        Ok(thread.clone())
    }

    fn delete(&self, ids: &[String]) -> io::Result<usize> {
        let mut threads = self.threads();
        let before = threads.len();
        threads.retain(|thread| !ids.contains(&thread.id));
        Ok(before - threads.len())
    }

    fn watched(&self) -> Option<PathBuf> {
        None
    }

    fn subject(&self, change: &ChangeNow) -> Subject {
        Subject::Opened {
            key: change.change_id.clone(),
            title: change.title.clone(),
        }
    }
}

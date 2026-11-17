//! Watching the plugins folder, so an edit reloads its plugin.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use notify::Watcher as _;

use super::FromPlugin;

/// How long the folder must be quiet before a change is reported: a save is
/// several events, and an editor may write a file in two steps.
const QUIET: Duration = Duration::from_millis(300);

/// The watch on the plugins folder. Dropping it stops the watch.
pub(super) struct Watcher {
    _watcher: notify::RecommendedWatcher,
}

impl Watcher {
    /// Watch `dir` and tell the app which of its directories changed.
    /// `None` when there is no such folder yet, or the platform won't watch.
    pub(super) fn start(dir: &Path, to: UnboundedSender<FromPlugin>) -> Option<Self> {
        // The watcher reports paths as the system resolves them.
        let dir = dir.canonicalize().ok()?;
        let (events, received) = channel::<notify::Event>();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event {
                    let _ = events.send(event);
                }
            })
            .ok()?;
        watcher.watch(&dir, notify::RecursiveMode::Recursive).ok()?;
        std::thread::Builder::new()
            .name("plugin-watch".to_owned())
            .spawn(move || {
                let mut changed = BTreeSet::new();
                loop {
                    // Wait for a change, then for the folder to go quiet.
                    let event = if changed.is_empty() {
                        received.recv().map_err(|_| RecvTimeoutError::Disconnected)
                    } else {
                        received.recv_timeout(QUIET)
                    };
                    match event {
                        Ok(event) => changed.extend(plugins_of(&dir, &event)),
                        Err(RecvTimeoutError::Timeout) => {
                            let names = std::mem::take(&mut changed).into_iter().collect();
                            if to.unbounded_send(FromPlugin::Changed(names)).is_err() {
                                return;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .ok()?;
        Some(Self { _watcher: watcher })
    }
}

/// The directories of `dir` an event's paths are in. Reads, and files an
/// editor or a version control system keeps to itself, change no plugin.
fn plugins_of(dir: &Path, event: &notify::Event) -> Vec<String> {
    use notify::EventKind;
    use notify::event::ModifyKind;
    let changes = match event.kind {
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => true,
        _ => false,
    };
    if !changes {
        return Vec::new();
    }
    event
        .paths
        .iter()
        .filter_map(|path| {
            let inside = path.strip_prefix(dir).ok()?;
            let mut parts = inside
                .components()
                .map(|part| part.as_os_str().to_string_lossy());
            let plugin = parts.next()?;
            let hidden = parts.any(|part| part.starts_with('.') || part.ends_with('~'));
            (!hidden && !plugin.starts_with('.')).then(|| plugin.into_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind, ModifyKind};
    use notify::{Event, EventKind};

    fn event(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(path.into())
    }

    #[test]
    fn an_edit_names_its_plugin_and_noise_names_none() {
        let dir = Path::new("/config/plugins");
        let write = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        assert_eq!(
            plugins_of(dir, &event(write, "/config/plugins/forgejo/lib/api.js")),
            ["forgejo"]
        );
        assert_eq!(
            plugins_of(
                dir,
                &event(EventKind::Create(CreateKind::Folder), "/config/plugins/new")
            ),
            ["new"]
        );
        for (kind, path) in [
            (write, "/config/plugins/forgejo/.git/index"),
            (write, "/config/plugins/forgejo/main.js~"),
            (write, "/config/plugins/.DS_Store"),
            (write, "/config/elsewhere/main.js"),
            (
                EventKind::Access(AccessKind::Read),
                "/config/plugins/forgejo/main.js",
            ),
            (
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
                "/config/plugins/forgejo/main.js",
            ),
        ] {
            assert!(plugins_of(dir, &event(kind, path)).is_empty(), "{path}");
        }
    }
}

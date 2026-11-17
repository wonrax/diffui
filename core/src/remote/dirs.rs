//! Looking around the server's disk for the app's open prompt: the folders of
//! a folder, and what opening a path would open.

use std::path::{Path, PathBuf};

use super::wire::{DirEntry, FoundRepository, Probed};
use crate::repository::{Vcs, prepare_repository};

pub(super) fn home() -> Option<String> {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(name).ok().filter(|home| !home.is_empty())
}

pub(super) fn expand(path: &str) -> PathBuf {
    PathBuf::from(super::expand_home(path, home().as_deref()))
}

/// The folders in `path`, by name, repositories flagged.
pub(super) fn list(path: &str) -> Result<Vec<DirEntry>, String> {
    let dir = expand(path);
    let read = std::fs::read_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let mut entries: Vec<DirEntry> = read
        .flatten()
        // `metadata` follows a symlink, so a linked folder lists.
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let vcs = repository_kind(&entry.path());
            Some(DirEntry { name, vcs })
        })
        .collect();
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

/// The markers `prepare_repository` looks for, jj first since a colocated
/// repository has both.
fn repository_kind(dir: &Path) -> Option<Vcs> {
    if dir.join(".jj").is_dir() {
        Some(Vcs::Jj)
    } else if dir.join(".git").exists() {
        Some(Vcs::Git)
    } else {
        None
    }
}

/// What `path` is, the way opening it would find out.
pub(super) fn probe(path: &str) -> Probed {
    let target = expand(path);
    // The folder being listed: the path itself when it ends in a separator,
    // else the one the name being typed is in.
    let listed = if path.ends_with('/') || path == "~" {
        Some(target.as_path())
    } else {
        target.parent()
    };
    let repository = prepare_repository(&target)
        .ok()
        .map(|repository| FoundRepository {
            at_root: target
                .canonicalize()
                .is_ok_and(|target| target == repository.root),
            root: repository.root.to_string_lossy().into_owned(),
            vcs: repository.vcs,
            scope: repository.scope.to_string_lossy().into_owned(),
        });
    Probed {
        dir_exists: listed.is_some_and(Path::is_dir),
        repository,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("diffui-dirs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn a_listing_is_the_folders_with_repositories_flagged() {
        let root = scratch("list");
        std::fs::create_dir_all(root.join("plain")).unwrap();
        std::fs::create_dir_all(root.join("Jay/.jj")).unwrap();
        std::fs::create_dir_all(root.join("gee/.git")).unwrap();
        std::fs::write(root.join("file.txt"), "").unwrap();

        let listing = list(&format!("{}/", root.display())).unwrap();
        let rows: Vec<(&str, Option<Vcs>)> = listing
            .iter()
            .map(|entry| (entry.name.as_str(), entry.vcs))
            .collect();
        assert_eq!(
            rows,
            [
                ("gee", Some(Vcs::Git)),
                ("Jay", Some(Vcs::Jj)),
                ("plain", None)
            ]
        );
        assert!(list(&format!("{}/nope/", root.display())).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_probe_finds_the_repository_above_a_path() {
        let root = scratch("probe");
        std::fs::create_dir_all(root.join("repo/.jj")).unwrap();
        std::fs::create_dir_all(root.join("repo/src")).unwrap();

        let at_root = probe(&root.join("repo").to_string_lossy());
        let found = at_root.repository.unwrap();
        assert!(found.at_root);
        assert_eq!(found.vcs, Vcs::Jj);
        assert_eq!(found.root, root.join("repo").to_string_lossy());

        let inside = probe(&root.join("repo/src").to_string_lossy())
            .repository
            .unwrap();
        assert!(!inside.at_root);
        assert_eq!(inside.scope, "src");

        let typing = probe(&format!("{}/re", root.display()));
        assert!(typing.dir_exists);
        assert_eq!(typing.repository, None);

        let missing = probe(&format!("{}/nope/x", root.display()));
        assert!(!missing.dir_exists);
        let _ = std::fs::remove_dir_all(root);
    }
}

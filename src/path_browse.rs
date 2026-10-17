//! Directory completion for the open-repository prompt: the subdirectories of
//! the folder the typed path is in, narrowed by the name being typed.
//!
//! Only absolute and `~`-rooted input browses. A relative path would resolve
//! against the process cwd, which is `/` when launched from Finder, and
//! `owner/repo#123` must not be mistaken for one.

use std::path::{Path, PathBuf};

use diffui_core::Vcs;

/// Most rows the prompt lists; typing more of the name narrows past them.
pub(crate) const MAX_SUGGESTIONS: usize = 8;

/// The typed path, split where completion happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query {
    /// The directory being listed, `~` expanded.
    pub(crate) dir: PathBuf,
    /// That directory as typed, through its trailing `/`, so a completion
    /// keeps the user's `~` instead of spelling out `$HOME`.
    pub(crate) typed_dir: String,
    /// The partial name after the last `/`.
    pub(crate) partial: String,
}

impl Query {
    pub(crate) fn parse(input: &str) -> Option<Self> {
        // A bare `~` lists home, as a shell completes it to `~/`.
        let input = if input == "~" { "~/" } else { input };
        if !input.starts_with('/') && !input.starts_with("~/") {
            return None;
        }
        let split = input.rfind('/')? + 1;
        let typed_dir = &input[..split];
        Some(Self {
            dir: crate::expand_user_path(typed_dir),
            typed_dir: typed_dir.to_owned(),
            partial: input[split..].to_owned(),
        })
    }

    /// The input that browses into `entry`.
    pub(crate) fn complete(&self, entry: &Entry) -> String {
        format!("{}{}/", self.typed_dir, entry.name)
    }

    /// The input naming `entry` itself, for opening it.
    pub(crate) fn path_of(&self, entry: &Entry) -> String {
        format!("{}{}", self.typed_dir, entry.name)
    }

    /// The listed entries whose names start with the partial, case-insensitively.
    /// Dot-directories stay hidden until the partial asks for them.
    pub(crate) fn matches<'a>(&self, listing: &'a Listing) -> Vec<&'a Entry> {
        let partial = self.partial.to_lowercase();
        let show_hidden = partial.starts_with('.');
        listing
            .entries
            .iter()
            .filter(|entry| show_hidden || !entry.name.starts_with('.'))
            .filter(|entry| entry.folded.starts_with(&partial))
            .take(MAX_SUGGESTIONS)
            .collect()
    }
}

/// The subdirectories of one directory, sorted by name.
#[derive(Debug, Clone)]
pub(crate) struct Listing {
    pub(crate) dir: PathBuf,
    pub(crate) entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) name: String,
    /// `name` lowercased, so matching doesn't fold it on every keystroke.
    folded: String,
    /// Set when the directory is itself a repository root.
    pub(crate) vcs: Option<Vcs>,
}

/// Read `dir`'s subdirectories (following symlinks). Blocking: one stat per
/// entry to classify it, so callers run it off the UI thread. An unreadable
/// directory lists as empty.
pub(crate) fn list(dir: PathBuf) -> Listing {
    let mut entries: Vec<Entry> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let path = entry.path();
            let is_dir = match entry.file_type().ok()? {
                kind if kind.is_symlink() => path.is_dir(),
                kind => kind.is_dir(),
            };
            is_dir.then(|| Entry {
                folded: name.to_lowercase(),
                vcs: repository_kind(&path),
                name,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.folded.cmp(&b.folded).then_with(|| a.name.cmp(&b.name)));
    Listing { dir, entries }
}

/// The same markers `prepare_repository` looks for, jj first since a
/// colocated repo has both.
fn repository_kind(dir: &Path) -> Option<Vcs> {
    if dir.join(".jj").is_dir() {
        Some(Vcs::Jj)
    } else if dir.join(".git").exists() {
        Some(Vcs::Git)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            folded: name.to_lowercase(),
            vcs: None,
        }
    }

    #[test]
    fn only_absolute_and_home_paths_browse() {
        assert!(Query::parse("").is_none());
        assert!(Query::parse("code/repo").is_none());
        assert!(Query::parse("owner/repo#123").is_none());
        assert!(Query::parse("~user/code").is_none());
        assert!(Query::parse("https://github.com/o/r/pull/1").is_none());

        let root = Query::parse("/").unwrap();
        assert_eq!(root.dir, PathBuf::from("/"));
        assert_eq!(root.partial, "");

        let nested = Query::parse("/usr/lo").unwrap();
        assert_eq!(nested.dir, PathBuf::from("/usr/"));
        assert_eq!(nested.typed_dir, "/usr/");
        assert_eq!(nested.partial, "lo");
    }

    #[test]
    fn tilde_lists_home_and_completes_in_kind() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home = PathBuf::from(home);
        let bare = Query::parse("~").unwrap();
        assert_eq!(bare.dir, home);
        assert_eq!(bare.partial, "");
        assert_eq!(bare.complete(&entry("code")), "~/code/");

        let nested = Query::parse("~/code/dif").unwrap();
        assert_eq!(nested.dir, home.join("code"));
        assert_eq!(nested.partial, "dif");
        assert_eq!(nested.complete(&entry("diffui")), "~/code/diffui/");
        assert_eq!(nested.path_of(&entry("diffui")), "~/code/diffui");
    }

    #[test]
    fn matching_is_a_case_insensitive_prefix_that_hides_dot_dirs() {
        let listing = Listing {
            dir: PathBuf::from("/x"),
            entries: vec![
                entry(".config"),
                entry("Desktop"),
                entry("diffui"),
                entry("docs"),
            ],
        };
        let names = |input: &str| -> Vec<String> {
            Query::parse(input)
                .unwrap()
                .matches(&listing)
                .into_iter()
                .map(|entry| entry.name.clone())
                .collect()
        };
        assert_eq!(names("/x/"), ["Desktop", "diffui", "docs"]);
        assert_eq!(names("/x/d"), ["Desktop", "diffui", "docs"]);
        assert_eq!(names("/x/DI"), ["diffui"]);
        assert_eq!(names("/x/."), [".config"]);
        assert!(names("/x/zzz").is_empty());
    }

    #[test]
    fn recents_stay_up_until_the_first_folder_is_read() {
        let dialog = crate::OpenRepoDialog {
            path: "~/".to_owned(),
            ..Default::default()
        };
        assert!(dialog.suggestions().is_none());
    }

    #[test]
    fn rows_of_the_last_folder_stay_up_while_the_next_is_read() {
        let mut dialog = crate::OpenRepoDialog {
            path: "/x/di".to_owned(),
            listing: Some(Listing {
                dir: PathBuf::from("/x/"),
                entries: vec![entry("diffui"), entry("docs")],
            }),
            listed_query: Query::parse("/x/di"),
            ..Default::default()
        };
        dialog.path = "/x/diffui/".to_owned();
        let (query, rows) = dialog.suggestions().unwrap();
        assert_eq!(query.typed_dir, "/x/");
        assert_eq!(query.complete(rows[0]), "/x/diffui/");
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn listing_keeps_only_directories_and_flags_repositories() {
        let root = std::env::temp_dir().join(format!("diffui-browse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("plain")).unwrap();
        std::fs::create_dir_all(root.join("jjrepo/.jj")).unwrap();
        std::fs::create_dir_all(root.join("Gitrepo/.git")).unwrap();
        std::fs::write(root.join("file.txt"), "").unwrap();

        let listing = list(root.clone());
        let summary: Vec<(&str, Option<Vcs>)> = listing
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.vcs))
            .collect();
        assert_eq!(
            summary,
            [
                ("Gitrepo", Some(Vcs::Git)),
                ("jjrepo", Some(Vcs::Jj)),
                ("plain", None),
            ]
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}

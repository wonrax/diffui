//! Directory completion for the open-repository prompt: the subdirectories of
//! the folder the typed path is in, narrowed by the name being typed.
//!
//! Only absolute and `~`-rooted input browses. A relative path would resolve
//! against the process cwd, which is `/` when launched from Finder, and
//! `owner/repo#123` must not be mistaken for one.
//!
//! Alongside the listing, every edit sends the typed path off the UI thread to
//! be looked at ([`probe`]): whether it is a repository, inside one, or
//! neither, so the prompt can say what Enter will do before it's pressed
//! ([`Status`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use diffui_core::Vcs;
use diffui_core::forge::ForgePr;

use crate::{Cx, Diffui, TabSource};

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

    /// The listed entries whose names start with the partial, case-insensitively,
    /// all of them: the list scrolls, so a folder can be browsed for as well as
    /// typed. Dot-directories stay hidden until the partial asks for them.
    pub(crate) fn matches<'a>(&self, listing: &'a Listing) -> Vec<&'a Entry> {
        self.all_matches(listing).collect()
    }

    /// How many entries match, and how many of those are repositories.
    pub(crate) fn count(&self, listing: &Listing) -> (usize, usize) {
        self.all_matches(listing)
            .fold((0, 0), |(folders, repos), entry| {
                (folders + 1, repos + usize::from(entry.vcs.is_some()))
            })
    }

    fn all_matches<'a>(&self, listing: &'a Listing) -> impl Iterator<Item = &'a Entry> {
        let partial = self.partial.to_lowercase();
        let show_hidden = partial.starts_with('.');
        listing
            .entries
            .iter()
            .filter(move |entry| show_hidden || !entry.name.starts_with('.'))
            .filter(move |entry| entry.folded.starts_with(&partial))
    }

    /// The listed folder as breadcrumbs: each segment as typed, with the input
    /// that lists it. `/x/y/` is `/`, `x`, `y`; `~/x/` is `~`, `x`.
    pub(crate) fn crumbs(&self) -> Vec<(String, String)> {
        let mut crumbs = Vec::new();
        let mut at = String::new();
        let rest = if let Some(rest) = self.typed_dir.strip_prefix('/') {
            at.push('/');
            crumbs.push(("/".to_owned(), at.clone()));
            rest
        } else {
            self.typed_dir.as_str()
        };
        for segment in rest.split('/').filter(|segment| !segment.is_empty()) {
            at.push_str(segment);
            at.push('/');
            crumbs.push((segment.to_owned(), at.clone()));
        }
        crumbs
    }
}

/// The input one folder up from `input`, which ends in `/`: what Backspace
/// on that trailing `/` goes to. `None` where there's no folder to go up to
/// (`/`, `~/`), and Backspace just deletes.
pub(crate) fn up_a_folder(input: &str) -> Option<String> {
    let inner = input.strip_suffix('/')?;
    if inner.is_empty() || inner == "~" {
        return None;
    }
    let cut = inner.rfind('/')? + 1;
    Some(inner[..cut].to_owned())
}

/// What the typed path is on disk, found off the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probe {
    /// The input it was taken for.
    pub(crate) input: String,
    /// The folder the input lists exists.
    pub(crate) dir_exists: bool,
    /// The repository opening the input would open, as the open itself finds
    /// it.
    pub(crate) repo: Option<FoundRepo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoundRepo {
    pub(crate) root: PathBuf,
    pub(crate) vcs: Vcs,
    /// The input names the root itself, not a folder inside it.
    pub(crate) at_root: bool,
}

/// Look at `input` the way opening it would: the repository at it or above
/// it. Blocking (a few stats, a canonicalize), so callers run it off the UI
/// thread.
pub(crate) fn probe(input: String) -> Probe {
    let query = Query::parse(&input);
    let dir_exists = query.as_ref().is_some_and(|query| query.dir.is_dir());
    let target = crate::expand_user_path(&input);
    let repo = diffui_core::prepare_repository(&target)
        .ok()
        .map(|repository| FoundRepo {
            at_root: target
                .canonicalize()
                .is_ok_and(|target| target == repository.root),
            root: repository.root,
            vcs: repository.vcs,
        });
    Probe {
        input,
        dir_exists,
        repo,
    }
}

/// What the prompt says about its input, as it's typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    /// A pull request reference.
    PullRequest(ForgePr),
    /// Not a path the prompt browses, nor a pull request.
    Relative,
    /// A repository, or a folder inside one.
    Repository {
        /// Its root's folder name.
        name: String,
        vcs: Vcs,
        /// The input is the root itself.
        at_root: bool,
        /// A tab has it open already.
        open: bool,
    },
    /// A folder that is no repository.
    NotRepository { folder: String },
    /// Nothing in the folder starts with what's typed.
    NoMatch { folder: String, partial: String },
    /// The folder doesn't exist.
    Missing { folder: String },
    /// Opening failed, for a reason only the open itself found.
    Failed(String),
}

/// Whether a recent repository is a jj or git one. Recents only record the
/// path; the answer is stat'ed once per path and kept.
fn recent_kind(root: &str) -> Option<Vcs> {
    thread_local! {
        static KINDS: RefCell<HashMap<String, Option<Vcs>>> = RefCell::default();
    }
    KINDS.with(|kinds| {
        *kinds
            .borrow_mut()
            .entry(root.to_owned())
            .or_insert_with(|| repository_kind(Path::new(root)))
    })
}

/// One row of the prompt's recent list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recent {
    pub(crate) root: String,
    pub(crate) vcs: Option<Vcs>,
    /// A tab has it open: picking it switches there.
    pub(crate) open: bool,
}

impl Diffui {
    /// The recent repositories, most recent first, the open ones marked.
    pub(crate) fn open_repo_recents(&self) -> Vec<Recent> {
        self.recent_repos
            .iter()
            .map(|root| {
                let tab = self.tabs.iter().find_map(|tab| match &tab.source {
                    TabSource::Repo { root: open, vcs } if open.to_str() == Some(root) => {
                        Some(*vcs)
                    }
                    _ => None,
                });
                Recent {
                    root: root.clone(),
                    vcs: tab.or_else(|| recent_kind(root)),
                    open: tab.is_some(),
                }
            })
            .collect()
    }

    /// The rows the prompt's arrows move through: the folder suggestions while
    /// a path is typed, else the recents.
    pub(crate) fn open_repo_row_count(&self) -> usize {
        let Some(dialog) = self.open_repo_dialog() else {
            return 0;
        };
        match dialog.suggestions() {
            Some((_, rows)) => rows.len(),
            None if dialog.path.trim().is_empty() => self.recent_repos.len(),
            None => 0,
        }
    }

    /// The recent the arrows picked, while the recents show.
    pub(crate) fn open_repo_selected_recent(&self) -> Option<String> {
        let dialog = self.open_repo_dialog()?;
        if !dialog.path.trim().is_empty() {
            return None;
        }
        self.recent_repos.get(dialog.selected?).cloned()
    }

    /// Enter on row `row`: a folder suggestion, or a recent.
    pub(crate) fn open_repo_pick_row(&mut self, row: usize, cx: &mut Cx) {
        match self.open_repo_selected_recent() {
            Some(root) => self.open_repository(&root, cx),
            None => self.pick_open_repo_suggestion(row, cx),
        }
    }

    /// Tab: complete the path into the highlighted (else first) folder, or
    /// take the highlighted recent's path to browse from.
    pub(crate) fn open_repo_complete(&mut self, cx: &mut Cx) {
        if let Some(root) = self.open_repo_selected_recent() {
            return self.browse_open_repo_path(crate::contract_user_path(&root), cx);
        }
        let completed = self.open_repo_dialog().and_then(|dialog| {
            let (query, rows) = dialog.suggestions()?;
            let entry = rows.get(dialog.selected.unwrap_or(0))?;
            Some(query.complete(entry))
        });
        if let Some(path) = completed {
            self.browse_open_repo_path(path, cx)
        }
    }

    /// The prompt's field changed to `path`. Backspace on a path's trailing
    /// `/` goes up a whole folder rather than leaving a half-typed name.
    pub(crate) fn open_repo_typed(&mut self, path: String, cx: &mut Cx) {
        let up = self.open_repo_dialog().and_then(|dialog| {
            let old = dialog.path.as_str();
            (old.len() == path.len() + 1 && old.starts_with(path.as_str()) && old.ends_with('/'))
                .then(|| up_a_folder(old))
                .flatten()
        });
        match up {
            Some(parent) => {
                self.browse_open_repo_path(parent.clone(), cx);
                crate::input::set_caret(crate::tab_bar::OPEN_REPO_INPUT_ID, parent.len(), cx);
            }
            None => self.set_open_repo_path(path, cx),
        }
    }

    /// Look at the prompt's path off the UI thread, for its status line. A
    /// look that comes back after the path has moved on is dropped.
    pub(crate) fn probe_open_repo_path(&mut self, cx: &mut Cx) {
        let Some(input) = self
            .open_repo_dialog()
            .map(|dialog| dialog.path.trim().to_owned())
        else {
            return;
        };
        if Query::parse(&input).is_none() {
            return;
        }
        Self::run_async(
            async move { tokio::task::spawn_blocking(move || probe(input)).await.ok() },
            cx,
            |this, probe, _| {
                if let Some(probe) = probe
                    && let Some(dialog) = this.open_repo_dialog_mut()
                    && dialog.path.trim() == probe.input
                {
                    dialog.probe = Some(probe);
                }
            },
        );
    }

    /// What the prompt says about its input right now, or `None` while the
    /// recents show. A path whose look hasn't come back yet keeps the last
    /// one's words.
    pub(crate) fn open_repo_status(&self) -> Option<Status> {
        let dialog = self.open_repo_dialog()?;
        if let Some(error) = &dialog.error {
            return Some(Status::Failed(error.clone()));
        }
        let input = dialog.path.trim();
        if input.is_empty() {
            return None;
        }
        if let Some(spec) = ForgePr::parse(input) {
            return Some(Status::PullRequest(spec));
        }
        let Some(query) = Query::parse(input) else {
            return Some(Status::Relative);
        };
        let probe = dialog.probe.as_ref()?;
        if let Some(repo) = &probe.repo {
            let open = self
                .tabs
                .iter()
                .any(|tab| tab.root() == Some(repo.root.as_path()));
            let name = repo.root.file_name().map_or_else(
                || repo.root.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            return Some(Status::Repository {
                name,
                vcs: repo.vcs,
                at_root: repo.at_root,
                open,
            });
        }
        if !probe.dir_exists {
            return Some(Status::Missing {
                folder: query.typed_dir,
            });
        }
        let listed = dialog
            .listing
            .as_ref()
            .filter(|listing| listing.dir == query.dir);
        if let Some(listing) = listed
            && !query.partial.is_empty()
            && query.count(listing).0 == 0
        {
            return Some(Status::NoMatch {
                folder: query.typed_dir,
                partial: query.partial,
            });
        }
        Some(Status::NotRepository {
            folder: query.typed_dir,
        })
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
    /// A repository's resolved path, what an open tab's root is compared to.
    pub(crate) canonical: Option<PathBuf>,
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
            is_dir.then(|| {
                let vcs = repository_kind(&path);
                Entry {
                    folded: name.to_lowercase(),
                    canonical: vcs.and_then(|_| path.canonicalize().ok()),
                    vcs,
                    name,
                }
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
            canonical: None,
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
    fn backspace_on_a_trailing_slash_goes_up_a_whole_folder() {
        assert_eq!(up_a_folder("~/code/diffui/").as_deref(), Some("~/code/"));
        assert_eq!(up_a_folder("~/code/").as_deref(), Some("~/"));
        assert_eq!(up_a_folder("/usr/lib/").as_deref(), Some("/usr/"));
        assert_eq!(up_a_folder("/usr/").as_deref(), Some("/"));
        // Nowhere further up, and not a folder: Backspace just deletes.
        assert_eq!(up_a_folder("~/"), None);
        assert_eq!(up_a_folder("/"), None);
        assert_eq!(up_a_folder("~/code"), None);
    }

    #[test]
    fn the_listed_folder_reads_as_breadcrumbs_that_list_it() {
        let crumbs = Query::parse("/usr/local/b").unwrap().crumbs();
        assert_eq!(
            crumbs,
            [
                ("/".to_owned(), "/".to_owned()),
                ("usr".to_owned(), "/usr/".to_owned()),
                ("local".to_owned(), "/usr/local/".to_owned()),
            ]
        );
        let home = Query::parse("~/code/").unwrap().crumbs();
        assert_eq!(
            home,
            [
                ("~".to_owned(), "~/".to_owned()),
                ("code".to_owned(), "~/code/".to_owned()),
            ]
        );
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

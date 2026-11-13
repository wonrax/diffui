//! Every scene of `fixtures/scenes.md`, through the real root view, written to
//! `integrate/<scene-id>.png` under the shots directory (`DIFFUI_SHOTS_DIR`, else
//! `target/shots`) for the parity sweep. The steps are the baseline
//! capture's, so each scene reaches its state the way the iced one did.
//!
//! ```text
//! cargo test --bin diffui scene_suite -- --ignored --nocapture
//! DIFFUI_SCENES=diff-tabs,menu-file cargo test --bin diffui scene_suite::every -- --ignored --nocapture
//! ```
//!
//! Each scene gets a fresh copy of the fixture and a clean scratch `HOME`, as the
//! baseline's did. After its shot, it's drawn again with every cache dropped and the
//! two images have to agree: a difference is a cached pane showing something stale.

use std::path::{Path, PathBuf};
use std::process::Command;

use diffui_core::askpass::{AskpassEvent, Prompt, PromptKind, PromptRequest};
use gpui_kit::MouseButton;

use crate::commands::CommandArg;
use crate::geometry::{Point, Rectangle, Size};
use crate::harness::{Harness, at};
use crate::menu::{AnchorSpec, MenuEntry, OverlayMenu};
use crate::modes::Mode;
use crate::{Action, MainView};

const W: f32 = 1440.0;
const H: f32 = 900.0;
const LANE: &str = "integrate";

pub(crate) struct Scene {
    id: &'static str,
    size: (f32, f32),
    theme: &'static str,
    config: &'static str,
    /// Appended to the `settings.toml` that sets the theme.
    settings: &'static str,
    open_repo: bool,
    /// Mid-flight: no settling after the steps, and no stale check.
    raw: bool,
    /// Looks before the first load is done.
    cold: bool,
    /// Something on screen moves with time, so two draws can't be compared.
    animated: bool,
    prepare: fn(&Path),
    steps: fn(&mut Ctx),
}

impl Scene {
    fn dark(mut self) -> Self {
        self.theme = "Dark";
        self
    }
    fn contrast(mut self) -> Self {
        self.theme = "Contrast";
        self
    }
    fn size(mut self, width: f32, height: f32) -> Self {
        self.size = (width, height);
        self
    }
    fn config(mut self, config: &'static str) -> Self {
        self.config = config;
        self
    }
    fn settings(mut self, settings: &'static str) -> Self {
        self.settings = settings;
        self
    }
    fn no_repo(mut self) -> Self {
        self.open_repo = false;
        self
    }
    fn raw(mut self) -> Self {
        self.raw = true;
        self
    }
    fn cold(mut self) -> Self {
        self.raw = true;
        self.cold = true;
        self
    }
    fn animated(mut self) -> Self {
        self.animated = true;
        self
    }
    fn prepare(mut self, prepare: fn(&Path)) -> Self {
        self.prepare = prepare;
        self
    }
}

#[derive(Default)]
struct Table {
    scenes: Vec<Scene>,
}

impl Table {
    fn group(&mut self, _group: &'static str) {}
    fn add(&mut self, scene: Scene) {
        self.scenes.push(scene);
    }
}

fn s(id: &'static str, _state: &'static str, _steps: &'static str, steps: fn(&mut Ctx)) -> Scene {
    Scene {
        id,
        size: (W, H),
        theme: "Light",
        config: "",
        settings: "",
        open_repo: true,
        raw: false,
        cold: false,
        animated: false,
        prepare: |_| {},
        steps,
    }
}

/// One scene's run: the harness, and the baseline capture's verbs over it.
pub(crate) struct Ctx {
    h: Harness,
    manifest: Vec<(String, String)>,
    repo: PathBuf,
    held: Option<gpui_kit::Point<gpui_kit::Pixels>>,
}

impl Ctx {
    fn settle(&mut self) {
        self.h.settle();
        self.h.frames();
        self.h.settle();
    }

    fn pump_ms(&mut self, ms: u64) {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < until {
            self.h.pump();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn act(&mut self, action: Action) {
        self.h.perform(action);
        self.settle();
    }

    fn cmd_action(&mut self, action: Action) {
        self.act(action);
    }

    fn cmd(&mut self, id: &'static str) {
        self.cmd_arg(id, CommandArg::None);
    }

    fn cmd_arg(&mut self, id: &'static str, arg: CommandArg) {
        let action = self.h.act(|ui, _| {
            let registry = ui.registry.clone();
            let command = registry
                .get(id)
                .unwrap_or_else(|| panic!("no command {id}"));
            if let Err(reason) = command.enabled(ui, &arg) {
                panic!("command {id} is disabled: {reason:?}");
            }
            command
                .build(ui, arg)
                .unwrap_or_else(|| panic!("command {id} built nothing"))
        });
        self.act(action);
    }

    /// `cmd`, but a disabled command is only logged: the shot shows why.
    fn try_cmd(&mut self, id: &'static str) {
        let verdict = self.h.read(|ui| {
            ui.registry
                .get(id)
                .unwrap_or_else(|| panic!("no command {id}"))
                .enabled(ui, &CommandArg::None)
        });
        match verdict {
            Ok(()) => self.cmd(id),
            Err(reason) => eprintln!("{id}: disabled: {reason:?}"),
        }
    }

    fn key(&mut self, chord: &str) {
        self.h.press(&chord_to_keystroke(chord));
        self.settle();
    }

    fn typed(&mut self, text: &str) {
        for ch in text.chars() {
            self.h.type_text(&ch.to_string());
        }
        self.settle();
    }

    fn send_enter(&mut self) {
        self.key("enter");
    }

    fn focus(&mut self, id: &'static str) {
        self.h.act(|_, cx| crate::input::focus(id, cx));
        self.settle();
    }

    /// Focuses a field with its text selected, so what's typed next replaces it. The
    /// baseline typed into the text it found, which is why its shot reads `bookmpresent(@)`.
    fn focus_replacing(&mut self, id: &'static str) {
        self.h
            .act(|_, cx| crate::input::focus_and_select_all(id, cx));
        self.settle();
    }

    fn hover(&mut self, x: f32, y: f32) {
        self.h.hover_point(at(x, y));
        self.settle();
    }

    fn right_click(&mut self, x: f32, y: f32) {
        // macOS pops the native menu, so the overlay is raised the way the baseline's
        // capture build did: the row under the point, its menu at the point.
        self.h.hover_point(at(x, y));
        self.settle();
        let tree = if x < self.h.read(|ui| ui.sidebar_width) {
            let selection = self
                .h
                .read(|ui| ui.active().session.selected_revision.clone());
            self.h.read(|ui| ui.revision_menu_tree(&selection))
        } else {
            self.h.read(|ui| ui.file_context_menu_tree(1))
        };
        let glow = (x < self.h.read(|ui| ui.sidebar_width))
            .then(|| Rectangle::new(Point::new(8.0, y - 22.0), Size::new(330.0, 44.0)));
        self.raise(tree, AnchorSpec::At(Point::new(x, y)), glow);
    }

    fn raise(&mut self, tree: Vec<MenuEntry>, anchor: AnchorSpec, glow: Option<Rectangle>) {
        self.h.act(|ui, _| {
            let mut menu = OverlayMenu::new(tree, anchor, None);
            menu.glow = glow;
            ui.push_mode(Mode::Menu(menu));
        });
        self.settle();
    }

    fn right_click_selected(&mut self) {
        let selection = self
            .h
            .read(|ui| ui.active().session.selected_revision.clone());
        let tree = self.h.read(|ui| ui.revision_menu_tree(&selection));
        let glow = Rectangle::new(Point::new(8.0, 200.0), Size::new(300.0, 28.0));
        self.raise(tree, AnchorSpec::At(Point::new(180.0, 214.0)), Some(glow));
    }

    fn open_first_submenu(&mut self) {
        let index = self.h.read(|ui| {
            ui.menu().and_then(|menu| {
                menu.root
                    .iter()
                    .position(|entry| matches!(entry, MenuEntry::Submenu { .. }))
            })
        });
        if let Some(index) = index {
            self.h.act(|ui, _| ui.menu_on_hover(vec![index]));
            self.settle();
        }
    }

    fn open_toolbar_menu(&mut self, which: crate::ToolbarMenu) {
        let (tree, anchor) = match which {
            crate::ToolbarMenu::FetchBranches => (
                self.h.read(|ui| ui.fetch_menu_entries()),
                Rectangle::new(Point::new(300.0, 37.0), Size::new(120.0, 28.0)),
            ),
            crate::ToolbarMenu::RevsetPresets => (
                self.h.read(|ui| ui.revset_menu_entry_tree()),
                Rectangle::new(Point::new(8.0, 78.0), Size::new(300.0, 28.0)),
            ),
        };
        self.raise(tree, AnchorSpec::Below(anchor), None);
    }

    fn scroll(&mut self, x: f32, y: f32, dy: f32) {
        self.h.wheel(at(x, y), dy);
        self.settle();
    }

    /// Presses at `from` and moves to `to` in steps, the button left down.
    fn drag(&mut self, from: (f32, f32), to: (f32, f32)) {
        let start = at(from.0, from.1);
        self.h.hover_point(start);
        self.h.pointer_down(start, MouseButton::Left);
        for step in 1..=12 {
            let t = step as f32 / 12.0;
            let position = at(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            self.h.pointer_move(position, Some(MouseButton::Left));
            self.held = Some(position);
            self.pump_ms(16);
        }
    }

    fn release(&mut self) {
        if let Some(position) = self.held.take() {
            self.h.pointer_up(position, MouseButton::Left);
        }
    }

    fn commit_of(&self, name: &str) -> String {
        self.manifest
            .iter()
            .find(|(names, _)| names.split(',').any(|it| it == name))
            .map(|(_, commit)| commit.clone())
            .unwrap_or_else(|| panic!("no such fixture name: {name}"))
    }

    fn select(&mut self, name: &str) {
        let commit = self.commit_of(name);
        self.act(Action::SelectRevision(
            diffui_core::RevisionSelection::Commit(commit),
        ));
    }

    /// Selects a file of the shown revision and lets the diff scroll to it.
    fn file(&mut self, path: &str) {
        self.act(Action::JumpToFile(path.to_owned()));
    }

    fn source_file(&mut self, path: &str) {
        let revision = self
            .h
            .read(|ui| ui.active().session.selected_revision.clone());
        self.act(Action::BrowseSource {
            revision,
            path: Some(path.to_owned()),
        });
    }

    fn confirm(&mut self) {
        self.act(Action::ConfirmAccept);
    }

    fn act_find_regex(&mut self) {
        self.h.act(|ui, _| ui.find_on_toggle_regex());
        self.settle();
    }

    fn select_draft_candidate(&mut self, name: &str) {
        let commit = self.commit_of(name);
        self.h.act(|ui, cx| {
            let index = ui
                .active()
                .session
                .commits
                .iter()
                .position(|row| row.commit_id() == commit);
            ui.on_draft_hover_candidate(index, cx);
        });
        self.settle();
    }

    fn select_op(&mut self, nth: usize) {
        let id = self.h.read(|ui| ui.active().history.operation_id(nth));
        match id {
            Some(id) => self.act(Action::History(
                crate::history_panel::HistoryAction::SelectOperation(id),
            )),
            None => eprintln!("no operation {nth}"),
        }
    }

    fn expand_first_activity(&mut self) {
        let id = self.h.read(|ui| ui.active().activities.first_with_output());
        match id {
            Some(id) => {
                self.h.act(|ui, _| ui.on_activity_expand(id));
                self.settle();
            }
            None => eprintln!("no activity with output"),
        }
    }

    /// A ⌘- or ⇧-click on a row, by name.
    fn modified_row_click(&mut self, name: &str, modifiers: crate::keys::Modifiers) {
        let key = crate::revision_list::RowSelectionKey::Commit(self.commit_of(name));
        self.h.act(|ui, cx| {
            ui.modifiers = modifiers;
            ui.on_select_row_key(key, cx);
            ui.modifiers = crate::keys::Modifiers::default();
        });
        self.settle();
    }

    fn mark(&mut self, name: &str) {
        self.modified_row_click(name, crate::keys::Modifiers::COMMAND);
    }

    fn shift_mark(&mut self, name: &str) {
        self.modified_row_click(name, crate::keys::Modifiers::SHIFT);
    }
}

/// The baseline's chord spelling as GPUI's keystroke: `cmd+k` to `cmd-k`, `R` to
/// `shift-r`, `esc` to `escape`.
fn chord_to_keystroke(chord: &str) -> String {
    let mut parts: Vec<&str> = chord.split('+').collect();
    let last = parts.pop().unwrap_or(chord);
    let key = match last {
        "esc" => "escape".to_owned(),
        key if key.len() == 1 && key.chars().all(char::is_uppercase) => {
            parts.push("shift");
            key.to_lowercase()
        }
        key => key.to_owned(),
    };
    parts.push(&key);
    parts.join("-")
}

fn jj(repo: &Path, args: &[&str]) {
    let status = Command::new(crate::harness::fixture_jj())
        .current_dir(repo)
        .env("JJ_TIMESTAMP", "2026-03-20T10:00:00+00:00")
        .env("JJ_OP_TIMESTAMP", "2026-03-20T10:00:00+00:00")
        .args(["--quiet"])
        .args(args)
        .status()
        .expect("spawn jj");
    assert!(status.success(), "jj {args:?}");
}

/// The Refresh scenes show what they say: edits and staleness as the
/// session has them.
fn disk_shows(c: &mut Ctx, edits: bool, stale: bool) {
    let disk = c.h.read(|ui| ui.active().session.disk().clone());
    assert_eq!(
        (disk.edits, disk.stale.is_some()),
        (edits, stale),
        "{disk:?}"
    );
}

/// A new file on disk that `@` doesn't hold until something snapshots it.
fn edit_on_disk(repo: &Path) {
    std::fs::write(repo.join("NOTES.md"), "Draft notes\n").unwrap();
}

/// `@` moved to another tree without updating the files on disk, as another
/// workspace or a `--ignore-working-copy` command leaves it: the checkout is
/// stale.
fn stale_checkout(repo: &Path) {
    jj(repo, &["--ignore-working-copy", "new", "@-"]);
}

/// Drops jj from the copy, leaving the colocated git repository. `.jj` holds the
/// manifest, which steps still read, so that moves aside first.
fn plain_git(repo: &Path) {
    std::fs::rename(
        repo.join(".jj/fixture-manifest"),
        repo.join(".git/fixture-manifest"),
    )
    .unwrap();
    let remote = repo.join(".jj/fixture-remote.git");
    std::fs::rename(&remote, repo.join(".git/fixture-remote.git")).unwrap();
    std::fs::remove_dir_all(repo.join(".jj")).unwrap();
    let config = repo.join(".git/config");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace(".jj/fixture-remote.git", ".git/fixture-remote.git"),
    )
    .unwrap();
}

/// Fourteen heads off `main`, `@` on the twelfth: a wide graph.
fn wide_graph(repo: &Path) {
    for branch in 1..=14 {
        jj(
            repo,
            &[
                "new",
                "main",
                "-m",
                &format!("feat: parallel branch {branch}"),
            ],
        );
    }
    jj(
        repo,
        &["edit", "description(substring:\"parallel branch 12\")"],
    );
}

/// Everything's visible: the revset box says `all()`.
fn all_revs(c: &mut Ctx) {
    c.act(Action::SetRevset("all()".to_owned()));
}

fn prompt(c: &mut Ctx, kind: PromptKind, text: &str, subject: Option<&str>, detail: Option<&str>) {
    let event = AskpassEvent::Prompt(PromptRequest {
        id: 1,
        prompt: Prompt {
            text: text.to_owned(),
            kind,
            subject: subject.map(str::to_owned),
            detail: detail.map(str::to_owned),
        },
    });
    c.h.act(|ui, cx| ui.on_askpass_event(event, cx));
    c.settle();
}

/// Opens a second repository as another tab.
fn second_repo(c: &mut Ctx, name: &str) {
    let path = c.repo.parent().unwrap().join(name);
    let status = Command::new("cp")
        .arg("-a")
        .arg(&c.repo)
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    c.act(Action::OpenRepo(path.to_string_lossy().into_owned()));
}

fn all() -> Vec<Scene> {
    let mut t = Table::default();

    t.group("Welcome (no repository open)");
    t.add(s("welcome-empty", "Wordmark, inline open-repository card with no recents, tips below. No tab strip content.", "Launch with no path, from a directory that isn't a repository, with no saved session.", |_c| {}).no_repo());
    t.add(s(
        "welcome-recents",
        "Same screen with recent repositories listed in the open card.",
        "Open a repository, then close its tab (cmd+W).",
        |c| {
            c.key("cmd+w");
        },
    ));
    t.add(
        s(
            "welcome-typed-path",
            "Open card with a typed path and the folder suggestion list under it.",
            "Welcome screen, type a path prefix into the card.",
            |c| {
                let dir = c.repo.parent().unwrap().to_string_lossy().into_owned();
                c.typed(&format!("{dir}/"));
            },
        )
        .no_repo(),
    );
    t.add(s(
        "welcome-open-error",
        "Error line 'Couldn't open repository: ...' above the open card.",
        "Launch with --path naming something that isn't a repository.",
        |_c| {},
    ));
    t.add(
        s(
            "welcome-dark",
            "welcome-empty in dark.",
            "As welcome-empty, theme Dark.",
            |_c| {},
        )
        .no_repo()
        .dark(),
    );

    t.group("Shell: tabs and toolbar");
    t.add(s("base-working-copy", "The default layout: tab strip, toolbar, revision list with revset filter, changed-files pane, diff of @ (unified, wrap on). Working copy selected, activity indicator idle, status bar along the bottom.", "Open the fixture repo.", |_c| {}));
    t.add(
        s(
            "base-dark",
            "base-working-copy in dark.",
            "As base-working-copy, theme Dark.",
            |_c| {},
        )
        .dark(),
    );
    t.add(
        s(
            "base-contrast",
            "base-working-copy in the high-contrast theme.",
            "As base-working-copy, theme High contrast.",
            |_c| {},
        )
        .contrast(),
    );
    t.add(s("tabs-two-repos", "Two repository tabs, second active; first tab shows owner/name dimmed and the uncommitted-changes dot.", "Open a second repository (cmd+O, pick it).", |c| { second_repo(c, "second-repo"); }));
    t.add(s("tabs-many-long", "Many tabs with long directory names squeezing the strip: ellipsis in tab labels, + and the palette control stay pinned.", "Open six repositories with long directory names.", |c| {
        for name in ["a-rather-long-repository-name-one", "a-rather-long-repository-name-two", "a-rather-long-repository-name-three", "a-rather-long-repository-name-four", "a-rather-long-repository-name-five", "a-rather-long-repository-name-six"] { second_repo(c, name); }
    }));
    t.add(s("toolbar-running-activity", "Toolbar while work runs: spinner with the task's label, +N chip for a second running task, progress line along the toolbar's bottom edge.", "Start two long operations (a slow fetch and a refresh). The harness starts two activity rows directly, since real ones finish too fast to catch.", |c| {
        c.h.act(|ui, _| {
            let tab = ui.active_tab_id().unwrap();
            ui.begin_activity(tab, "Fetching from origin", false);
            ui.begin_activity(tab, "Refreshing", false);
        });
        c.pump_ms(300);
    }).raw());
    t.add(s("toolbar-source-view", "Toolbar in the Source view: view switcher plus the browsed revision's id and description instead of the repo actions.", "Press the Source segment of the view switcher.", |c| { c.cmd("view.source"); }));
    t.add(s("toolbar-plain-git", "A plain git repository: no jj-only actions (no Undo, no Push), revset placeholder and git graph.", "Open a repository that has .git and no .jj.", |_c| {}).prepare(plain_git));
    t.add(s("toolbar-refresh-edits", "Manual refresh with an edit on disk: an accent dot on Refresh's glyph; the diff of @ doesn't show the edit yet.", "Set refresh = \"manual\", edit a file, focus the window.", |c| disk_shows(c, true, false)).config("refresh = \"manual\"\n").prepare(edit_on_disk));
    t.add(s("toolbar-refresh-stale", "Manual refresh on a stale checkout: Refresh's glyph in the warning ink, the stale toast once.", "Set refresh = \"manual\", move @ with --ignore-working-copy, open the repository.", |c| disk_shows(c, false, true)).config("refresh = \"manual\"\n").prepare(stale_checkout));
    t.add(
        s(
            "toolbar-refresh-stale-dark",
            "toolbar-refresh-stale in dark.",
            "As toolbar-refresh-stale, theme Dark.",
            |c| disk_shows(c, false, true),
        )
        .config("refresh = \"manual\"\n")
        .prepare(stale_checkout)
        .dark(),
    );
    t.add(s("toolbar-refresh-stale-edits", "Manual refresh on a stale checkout with an edit on disk: the warning glyph and the accent dot together.", "As toolbar-refresh-stale, with a file edited too.", |c| disk_shows(c, true, true)).config("refresh = \"manual\"\n").prepare(|repo| { stale_checkout(repo); edit_on_disk(repo); }));
    t.add(s("load-cold", "A cold load mid-flight: the list growing or 'Loading revisions…', progress line on the toolbar.", "Open a repository and look immediately.", |_c| {}).cold());

    t.group("Revision list");
    t.add(s("graph-all", "Revset all(): branching graph, merges, the octopus merge, immutable diamonds, conflict marks, bookmarks, tags, remote bookmarks, divergent ids.", "Type all() in the revset field, Enter.", all_revs));
    t.add(s("graph-all-scrolled", "The same list scrolled to the middle: scrollbar thumb, gutter reserved, rows stop short of it.", "As graph-all, scroll the list down.", |c| { all_revs(c); c.scroll(150.0, 500.0, -900.0); }));
    t.add(s("graph-wide", "Fourteen parallel branches off main: each row's text starts just past its own lanes, so wide rows trade alignment for room.", "Open a repository with many heads off one commit.", |_c| {}).prepare(wide_graph));
    t.add(s("graph-bookmark-states", "Bookmark chips in every state: ahead (*), conflicted (??), remote-only, deleted, tracked; a very long bookmark ellipsised.", "Revset 'bookmarks() | remote_bookmarks()'.", |c| { c.act(Action::SetRevset("bookmarks() | remote_bookmarks()".to_owned())); }));
    t.add(s(
        "graph-marked-rows",
        "Three rows marked for a batch action, one selected; marks distinct from the selection.",
        "cmd-click rows (Space marks the selected one).",
        |c| {
            all_revs(c);
            c.select("long-description");
            c.mark("long-description");
            c.mark("empty-revision");
            c.mark("many-files");
        },
    ));
    t.add(s(
        "revset-empty-result",
        "List empty state: 'No revisions match' with the hint and the Reset to default button.",
        "Revset none().",
        |c| {
            c.act(Action::SetRevset("none()".to_owned()));
        },
    ));
    t.add(s(
        "revset-error",
        "List error state: 'Couldn't evaluate this revset' with jj's message and Reset to default.",
        "Revset 'foo(' and Enter.",
        |c| {
            c.act(Action::SetRevset("foo(".to_owned()));
        },
    ));
    t.add(s("revset-completion", "The revset field focused with a partial word: completion dropdown with kinds and live counts.", "Click the revset field, type 'bookm'.", |c| { c.focus_replacing(crate::sidebar::REVSET_INPUT_ID); c.typed("bookm"); }));
    t.add(s(
        "revset-completion-args",
        "Completion dropdown inside a function argument, offering bookmark names.",
        "Type 'ancestors(feat'.",
        |c| {
            c.focus_replacing(crate::sidebar::REVSET_INPUT_ID);
            c.typed("ancestors(feat");
        },
    ));
    t.add(s("selection-outside-revset", "The diff pane keeps showing a revision the revset no longer lists, with the note saying so.", "Select a revision, then change the revset so it drops out.", |c| { c.select("kitchen-sink"); c.act(Action::SetRevset("main".to_owned())); }));
    t.add(s(
        "panel-history-collapsed",
        "Revision list hidden: it takes no width, changed files and diff widen, its toolbar toggle is off and the ids in the diff header carry a chevron.",
        "Click the toolbar's revisions toggle.",
        |c| {
            c.cmd_action(Action::ToggleHistoryPanel);
        },
    ));
    t.add(s(
        "panel-files-collapsed",
        "Changed files hidden: no width, the diff widens, the file count in the diff header carries a chevron.",
        "Click the toolbar's changed-files toggle.",
        |c| {
            c.cmd_action(Action::ToggleFilesPanel);
        },
    ));
    t.add(s(
        "panel-both-collapsed",
        "Both panels hidden: the diff has the window.",
        "Hide both.",
        |c| {
            c.cmd_action(Action::ToggleHistoryPanel);
            c.cmd_action(Action::ToggleFilesPanel);
        },
    ));
    t.add(s(
        "panel-files-peek",
        "Changed files hidden, the header's file count pressed: the file tree floats under it, over the diff.",
        "Hide the changed files, click 'N files' in the diff header.",
        |c| {
            c.select("kitchen-sink");
            c.cmd_action(Action::ToggleFilesPanel);
            c.h.click("commit-files-trigger");
            c.settle();
        },
    ));
    t.add(s(
        "panel-revisions-peek",
        "Revisions hidden, the header's ids pressed: the revision list floats down the window's left edge, over the panes.",
        "Hide the revisions, click the ids in the diff header.",
        |c| {
            c.cmd_action(Action::ToggleHistoryPanel);
            c.h.click("commit-revisions-trigger");
            c.settle();
        },
    ));

    t.group("Diff: file kinds");
    t.add(s("diff-kitchen-sink", "A revision touching 20 files: commit header (title, body, author, date, ids, parents), file tree with stats, first file's diff.", "Select the 'rework the pipeline' revision.", |c| { c.select("kitchen-sink"); }));
    t.add(s(
        "diff-details-collapsed",
        "Commit details folded to title and one metadata line.",
        "Press the fold control in the diff header.",
        |c| {
            c.select("kitchen-sink");
            c.cmd("view.toggle-commit-details");
        },
    ));
    t.add(s("diff-rename", "A renamed file in a jj repo. The app passes jj no copy records (core/src/jj/diff.rs), so it reads as an add here and a delete at old_name.rs, not as a rename. Parity is this, not jj's own 'R'.", "Select src/renderer/frame_loop.rs in the tree.", |c| { c.select("kitchen-sink"); c.file("src/renderer/frame_loop.rs"); }));
    t.add(s("diff-rename-git", "A rename where the app does show one: a plain git repository's diff, 'R' status and old path in the file header.", "Open the repository as plain git, select the revision, select the renamed file.", |c| { c.act(Action::SetRevset("--all".to_owned())); c.select("kitchen-sink"); c.file("src/renderer/frame_loop.rs"); }).prepare(plain_git));
    t.add(s(
        "diff-deleted",
        "A deleted file: all removal rows.",
        "Select src/removed.rs.",
        |c| {
            c.select("kitchen-sink");
            c.file("src/removed.rs");
        },
    ));
    t.add(s(
        "diff-binary",
        "A modified binary file: the size summary card instead of text.",
        "Select vendor/blob.bin.",
        |c| {
            c.select("kitchen-sink");
            c.file("vendor/blob.bin");
        },
    ));
    t.add(s(
        "diff-binary-added",
        "An added binary file.",
        "Select vendor/new-blob.dat.",
        |c| {
            c.select("kitchen-sink");
            c.file("vendor/new-blob.dat");
        },
    ));
    t.add(s(
        "diff-image-modified",
        "A modified PNG: old and new side by side with dimensions and sizes.",
        "Select assets/logo.png.",
        |c| {
            c.select("kitchen-sink");
            c.file("assets/logo.png");
        },
    ));
    t.add(s(
        "diff-image-added",
        "An added PNG preview.",
        "Select assets/hero.png.",
        |c| {
            c.select("kitchen-sink");
            c.file("assets/hero.png");
        },
    ));
    t.add(s(
        "diff-svg-rendered",
        "A modified SVG rendered old and new, with the markup toggle.",
        "Select assets/logo.svg.",
        |c| {
            c.select("kitchen-sink");
            c.file("assets/logo.svg");
        },
    ));
    t.add(s(
        "diff-svg-markup",
        "The same SVG as a text diff.",
        "Toggle SVG markup.",
        |c| {
            c.select("kitchen-sink");
            c.file("assets/logo.svg");
            c.act(Action::SetSvgSource(true));
        },
    ));
    t.add(s(
        "diff-long-lines-wrap",
        "Very long lines, wrap on: rows grow, continuation rows align under the text.",
        "Select src/generated/minified.js, wrap on.",
        |c| {
            c.select("kitchen-sink");
            c.file("src/generated/minified.js");
        },
    ));
    t.add(s(
        "diff-long-lines-nowrap",
        "The same with wrap off: lines clip at the pane edge, horizontal scrollbar.",
        "Toggle wrap (alt+Z).",
        |c| {
            c.select("kitchen-sink");
            c.file("src/generated/minified.js");
            c.cmd("view.toggle-wrap");
        },
    ));
    t.add(s(
        "diff-tabs",
        "Tabs in code: Makefile with leading and inner tabs.",
        "Select Makefile.",
        |c| {
            c.select("kitchen-sink");
            c.file("Makefile");
        },
    ));
    t.add(s(
        "diff-cjk-wrap",
        "CJK and mixed-width text with intra-line highlights, wrapping a long Japanese line.",
        "Select i18n/ja.txt.",
        |c| {
            c.select("kitchen-sink");
            c.file("i18n/ja.txt");
        },
    ));
    t.add(s(
        "diff-cjk-emoji-nowrap",
        "CJK and emoji with wrap off.",
        "Select i18n/zh.txt, wrap off.",
        |c| {
            c.select("kitchen-sink");
            c.file("i18n/ja.txt");
            c.cmd("view.toggle-wrap");
        },
    ));
    t.add(s(
        "diff-mode-change",
        "A mode-only change: the file header and its note, no hunks.",
        "Select scripts/run.sh.",
        |c| {
            c.select("kitchen-sink");
            c.file("scripts/run.sh");
        },
    ));
    t.add(s(
        "diff-empty-file",
        "An added empty file.",
        "Select docs/empty-file.txt.",
        |c| {
            c.select("kitchen-sink");
            c.file("docs/empty-file.txt");
        },
    ));
    t.add(s("diff-deep-path", "A deeply nested file: tree indentation and the header path ellipsise.", "Select the deep file.", |c| { c.select("kitchen-sink"); c.scroll(600.0, 500.0, 0.0); c.file("src/this/path/is/deliberately/nested/very/deeply/so/that/the/file/row/has/to/ellipsize/in/the/changes/list/and/the/diff/header/a_module_with_a_rather_long_file_name_as_well.rs"); }));
    t.add(s("diff-large-collapsed", "A whole-file rewrite of 20k lines: the backend's collapsed placeholder with 'show full diff'.", "Select the 'regenerate the large table' revision.", |c| { c.select("large-diff"); }));
    t.add(s(
        "diff-large-expanded",
        "The same file expanded in full, scrolled into the middle.",
        "Press Show full diff, scroll.",
        |c| {
            c.select("large-diff");
            c.try_cmd("file.expand-diff");
            c.scroll(1000.0, 500.0, -60000.0);
        },
    ));
    t.add(s("diff-many-files", "240 added files: file tree overflows, scrollbar and gutter in the tree, stats column aligned.", "Select the 'generate 240 module stubs' revision.", |c| { c.select("many-files"); }));
    t.add(s(
        "diff-many-files-scrolled",
        "The same with the tree and diff scrolled.",
        "Scroll the tree and the diff.",
        |c| {
            c.select("many-files");
            c.scroll(1000.0, 500.0, -2000.0);
            c.scroll(450.0, 500.0, -1200.0);
        },
    ));
    t.add(s(
        "diff-merge-commit",
        "A merge revision: two parents as links in the header.",
        "Select the merge.",
        |c| {
            c.select("merge-commit");
        },
    ));
    t.add(s(
        "diff-empty-revision",
        "An empty revision: no changed-files pane, the empty diff state.",
        "Select the empty revision.",
        |c| {
            all_revs(c);
            c.select("empty-revision");
        },
    ));
    t.add(s(
        "diff-no-description",
        "A revision without a description: the '(no description)' title treatment.",
        "Select it.",
        |c| {
            all_revs(c);
            c.select("no-description");
        },
    ));
    t.add(s("diff-long-description", "An enormous first line and a multi-paragraph body: title ellipsises and expands, graph row ellipsises, gutter respected.", "Select it.", |c| { all_revs(c); c.select("long-description"); }));
    t.add(s(
        "diff-immutable",
        "An immutable trunk revision: immutable badge, edit control unavailable.",
        "Select main.",
        |c| {
            c.select("main");
        },
    ));
    t.add(s(
        "diff-divergent",
        "A divergent change: the divergent badge and suffixed id.",
        "Select one divergent copy.",
        |c| {
            all_revs(c);
            c.select("divergent/take-a");
        },
    ));
    t.add(s("diff-scrolled-sticky", "A diff scrolled past the header: sticky file header, hunk headers, scrollbar on the right.", "Select the rename and scroll.", |c| { c.select("kitchen-sink"); c.file("src/renderer/frame_loop.rs"); c.scroll(1000.0, 500.0, -300.0); }));

    t.group("Diff: layout modes");
    t.add(s(
        "split-kitchen-sink",
        "Side by side: two columns with their own line numbers.",
        "Toggle split.",
        |c| {
            c.select("kitchen-sink");
            c.file("src/main.rs");
            c.cmd("view.toggle-split");
        },
    ));
    t.add(s(
        "split-nowrap",
        "Side by side with wrap off.",
        "Toggle split, wrap off.",
        |c| {
            c.select("kitchen-sink");
            c.file("src/generated/minified.js");
            c.cmd("view.toggle-split");
            c.cmd("view.toggle-wrap");
        },
    ));
    t.add(s(
        "split-wrap-long-lines",
        "Side by side with wrap on, long lines: both sides stay row-aligned.",
        "Toggle split on the long-lines file.",
        |c| {
            c.select("kitchen-sink");
            c.file("src/generated/minified.js");
            c.cmd("view.toggle-split");
        },
    ));
    t.add(s(
        "split-cjk",
        "Side by side with CJK.",
        "Toggle split on i18n/ja.txt.",
        |c| {
            c.select("kitchen-sink");
            c.file("i18n/ja.txt");
            c.cmd("view.toggle-split");
        },
    ));
    t.add(s(
        "split-image",
        "Side by side on an image file.",
        "Toggle split on assets/logo.png.",
        |c| {
            c.select("kitchen-sink");
            c.file("assets/logo.png");
            c.cmd("view.toggle-split");
        },
    ));
    t.add(
        s(
            "split-dark",
            "Side by side in dark.",
            "As split-kitchen-sink, theme Dark.",
            |c| {
                c.select("kitchen-sink");
                c.file("src/main.rs");
                c.cmd("view.toggle-split");
            },
        )
        .dark(),
    );
    t.add(
        s(
            "diff-kitchen-sink-dark",
            "Unified diff with syntax colours in dark.",
            "As diff-rename, theme Dark.",
            |c| {
                c.select("kitchen-sink");
                c.file("src/main.rs");
            },
        )
        .dark(),
    );

    t.add(s(
        "diff-text-selection",
        "Text selected by dragging in the diff.",
        "Drag across a few lines of code.",
        |c| {
            c.file("src/main.rs");
            c.drag((800.0, 320.0), (1150.0, 420.0));
            c.release();
            c.pump_ms(100);
        },
    ));
    t.add(s(
        "diff-hunk-hover",
        "Pointer on a hunk header: its controls.",
        "Hover a hunk header.",
        |c| {
            c.file("src/main.rs");
            c.hover(1000.0, 250.0);
        },
    ));

    t.group("Find");
    t.add(s(
        "find-open",
        "Find bar pinned top-right of the diff: empty query, case and regex toggles.",
        "cmd+F.",
        |c| {
            c.select("kitchen-sink");
            c.key("cmd+f");
        },
    ));
    t.add(s(
        "find-matches",
        "Query with matches: count, current and other matches highlighted in the diff.",
        "cmd+F, type 'render', Enter.",
        |c| {
            c.select("kitchen-sink");
            c.key("cmd+f");
            c.typed("render");
            c.key("enter");
        },
    ));
    t.add(s(
        "find-no-matches",
        "Query with no matches: zero count state.",
        "cmd+F, type a miss.",
        |c| {
            c.select("kitchen-sink");
            c.key("cmd+f");
            c.typed("zzzqqq");
        },
    ));
    t.add(s(
        "find-regex-invalid",
        "Regex mode with a broken pattern.",
        "cmd+F, enable regex, type 'ren(der'.",
        |c| {
            c.select("kitchen-sink");
            c.key("cmd+f");
            c.act_find_regex();
            c.typed("ren(der");
        },
    ));

    t.group("Command palette and command line");
    t.add(s(
        "palette-empty",
        "Palette open, empty input, mixed list.",
        "cmd+K.",
        |c| {
            c.key("cmd+k");
        },
    ));
    t.add(s(
        "palette-mixed-query",
        "Mixed fuzzy search across revisions, files and commands.",
        "cmd+K, type 'render'.",
        |c| {
            c.key("cmd+k");
            c.typed("render");
        },
    ));
    t.add(s(
        "palette-commands",
        "Commands only: labels, chords on the right, disabled rows greyed with a reason.",
        "cmd+K, type '>'.",
        |c| {
            c.key("cmd+k");
            c.typed(">");
        },
    ));
    t.add(s(
        "palette-files",
        "Files of the current revision.",
        "cmd+K, type '@'.",
        |c| {
            c.select("kitchen-sink");
            c.key("cmd+k");
            c.typed("@");
        },
    ));
    t.add(s(
        "palette-actions-column",
        "A highlighted revision result with its actions column pushed to the right.",
        "cmd+K, type 'pipeline', Tab.",
        |c| {
            c.key("cmd+k");
            c.typed("pipeline");
            c.key("tab");
            c.pump_ms(400);
        },
    ));
    t.add(s(
        "palette-no-results",
        "A query that matches nothing.",
        "cmd+K, type a miss.",
        |c| {
            c.key("cmd+k");
            c.typed("zzzqqqxxx");
        },
    ));
    t.add(
        s(
            "palette-dark",
            "palette-commands in dark.",
            "As palette-commands, theme Dark.",
            |c| {
                c.key("cmd+k");
                c.typed(">");
            },
        )
        .dark(),
    );
    t.add(s(
        "cmdline-empty",
        "Command line: the palette input with the jj prompt and a command list.",
        "Press ':'.",
        |c| {
            c.key(":");
        },
    ));
    t.add(s("cmdline-typed-rebase", "A typed rebase: words coloured in place, revset arguments with live counts, footer saying what Enter does.", "':' then 'rebase -s @ -d main'.", |c| { c.key(":"); c.typed("rebase -s @ -d main"); }));
    t.add(
        s(
            "cmdline-typed-rebase-dark",
            "cmdline-typed-rebase in dark.",
            "As cmdline-typed-rebase, theme Dark.",
            |c| {
                c.key(":");
                c.typed("rebase -s @ -d main");
            },
        )
        .dark(),
    );
    t.add(s(
        "cmdline-revset-error",
        "A typed command with a broken revset argument: the error pointed at.",
        "':' then 'rebase -s foo( -d main'.",
        |c| {
            c.key(":");
            c.typed("rebase -s foo( -d main");
        },
    ));
    t.add(s(
        "cmdline-completion",
        "Completion list inside a command: subcommands or flags.",
        "':' then 'git pu'.",
        |c| {
            c.key(":");
            c.typed("git pu");
        },
    ));
    t.add(s(
        "cmdline-unknown",
        "A command the grammar doesn't know.",
        "':' then 'frobnicate'.",
        |c| {
            c.key(":");
            c.typed("frobnicate --now");
        },
    ));
    t.add(s(
        "cli-output-panel",
        "A passthrough jj run's output panel with its captured text.",
        "':' then 'status', Enter.",
        |c| {
            c.key(":");
            c.typed("status");
            c.key("enter");
            c.pump_ms(1500);
            c.settle();
            // A real run is this binary started as jj, which under test is the test
            // runner, so its lines go in by hand: one of each stream.
            c.h.act(|ui, _| {
                use diffui_core::cli::{
                    CliLine,
                    CliStream::{Stderr, Stdout},
                };
                let line = |stream, text: &str| CliLine {
                    stream,
                    text: text.to_owned(),
                };
                ui.active_mut().cmdline.runs.last_mut().unwrap().lines = vec![
                    line(Stdout, "Working copy changes:"),
                    line(Stdout, "A assets/working-copy.png"),
                    line(Stdout, "D docs/empty-file.txt"),
                    line(Stdout, "M src/main.rs"),
                    line(
                        Stdout,
                        "Working copy  (@) : lvtsvnkk fa52156f (no description set)",
                    ),
                    line(Stderr, "Warning: These bookmarks have conflicts:"),
                    line(Stderr, "  release/conflicted"),
                ];
            });
            c.settle();
        },
    ));

    t.group("Target mode (drafts)");
    t.add(s("draft-rebase-start", "Rebase target mode: op bar with source, placement segments, hint strip under the diff, no candidate yet.", "Select a revision, press r.", |c| { c.select("cache-bindings"); c.key("r"); }));
    t.add(s("draft-rebase-candidate", "A destination armed: graph preview of the result, verdict in the op bar, target row styled.", "r, then j/k to a destination.", |c| { c.select("cache-bindings"); c.key("r"); c.key("j"); c.key("j"); c.key("j"); c.pump_ms(600); c.settle(); }));
    t.add(s(
        "draft-rebase-after",
        "Placement set to After.",
        "r, j, a.",
        |c| {
            c.select("cache-bindings");
            c.key("r");
            c.key("j");
            c.key("j");
            // With a candidate armed the key applies the draft, which is what the
            // baseline's shot shows; the scene is about the placement.
            c.act(Action::DraftPlacement(diffui_core::PlacementKind::After));
            c.pump_ms(600);
            c.settle();
        },
    ));
    t.add(s(
        "draft-rebase-before",
        "Placement set to Before.",
        "r, j, b.",
        |c| {
            c.select("cache-bindings");
            c.key("r");
            c.key("j");
            c.key("j");
            // With a candidate armed the key applies the draft, which is what the
            // baseline's shot shows; the scene is about the placement.
            c.act(Action::DraftPlacement(diffui_core::PlacementKind::Before));
            c.pump_ms(600);
            c.settle();
        },
    ));
    t.add(s(
        "draft-rebase-descendants",
        "Rebase with descendants.",
        "Select, R.",
        |c| {
            c.select("kitchen-sink");
            c.key("R");
            c.key("j");
            c.pump_ms(600);
            c.settle();
        },
    ));
    t.add(s(
        "draft-rebase-branch",
        "Rebase whole branch.",
        "Select, b.",
        |c| {
            c.select("cache-bindings");
            c.key("b");
        },
    ));
    t.add(s("draft-rebase-conflict-verdict", "A preview that forecasts conflicts: the conflict verdict and conflicted rows.", "Rebase the 'lower the sample count' stack's parent side onto a destination that conflicts.", |c| { all_revs(c); c.select("auth-refresh"); c.key("r"); c.select_draft_candidate("conflicted"); c.pump_ms(800); c.settle(); }));
    t.add(s(
        "draft-squash",
        "Squash target mode.",
        "Select, s.",
        |c| {
            c.select("cache-bindings");
            c.key("s");
            c.key("k");
            c.pump_ms(600);
            c.settle();
        },
    ));
    t.add(s(
        "draft-merge",
        "Merge target mode with an added parent.",
        "Select, m, j, Space.",
        |c| {
            c.select("cache-bindings");
            c.key("m");
            c.key("j");
            c.key("space");
            c.pump_ms(600);
            c.settle();
        },
    ));
    t.add(s(
        "draft-new",
        "New change at a picked spot.",
        "Select, N.",
        |c| {
            c.select("cache-bindings");
            c.key("N");
            c.key("j");
            c.pump_ms(600);
            c.settle();
        },
    ));
    t.add(s("draft-duplicate", "Duplicate onto.", "Select, Y.", |c| {
        c.select("cache-bindings");
        c.key("Y");
        c.key("j");
        c.pump_ms(600);
        c.settle();
    }));
    t.add(s("draft-parallelize", "Parallelize.", "Select, p.", |c| {
        c.select("cache-bindings");
        c.key("p");
        c.key("j");
        c.pump_ms(600);
        c.settle();
    }));
    t.add(s(
        "draft-batch-sources",
        "Batch: several marked revisions as sources; op bar lists them.",
        "Mark two rows, palette 'Rebase marked revisions onto…'.",
        |c| {
            c.select("cache-bindings");
            c.mark("cache-bindings");
            c.mark("kitchen-sink");
            c.cmd("selection.rebase.start");
            c.key("j");
            c.pump_ms(600);
            c.settle();
        },
    ));
    t.add(s("draft-drag", "Dragging a revision row: the drag has started a rebase draft, the drop spot under the cursor is armed.", "Press a row and drag it onto another.", |c| { c.drag((150.0, 200.0), (150.0, 330.0)); c.pump_ms(700); }).raw());
    t.add(
        s(
            "draft-dark",
            "draft-rebase-candidate in dark.",
            "As draft-rebase-candidate, theme Dark.",
            |c| {
                c.select("cache-bindings");
                c.key("r");
                c.key("j");
                c.key("j");
                c.key("j");
                c.pump_ms(600);
                c.settle();
            },
        )
        .dark(),
    );

    t.add(s(
        "draft-command-line",
        "Target mode turned into its jj command in the command line.",
        "r, j, then ':'.",
        |c| {
            c.select("cache-bindings");
            c.key("r");
            c.key("j");
            c.key("j");
            c.pump_ms(600);
            c.settle();
            c.key(":");
        },
    ));
    t.add(s(
        "draft-move-changes",
        "Move picked changes into a target: target mode with the changes named.",
        "Pick a file with x, run Move selected changes into….",
        |c| {
            c.key("x");
            c.try_cmd("changes.move-into");
            c.key("j");
            c.pump_ms(600);
            c.settle();
        },
    ));

    t.group("Partial changes and line selection");
    t.add(s("picks-files-hunks", "File and hunk checkboxes in use: some files picked, the action strip with split, squash, move, discard.", "Press x on a file; click a hunk checkbox.", |c| { c.key("x"); c.key("j"); c.key("x"); }));
    t.add(s(
        "lines-select-mode",
        "Line-select mode: changed lines pickable, hint strip with its keys.",
        "Press v.",
        |c| {
            c.file("src/main.rs");
            c.key("v");
        },
    ));
    t.add(s(
        "lines-picked",
        "Some lines picked and a range extended.",
        "v, j, Space, J, J, Space.",
        |c| {
            c.file("src/main.rs");
            c.key("v");
            c.key("j");
            c.key("space");
            c.key("J");
            c.key("J");
        },
    ));
    t.add(s(
        "lines-split",
        "Line select in side by side.",
        "Toggle split, v, pick lines.",
        |c| {
            c.file("src/main.rs");
            c.cmd("view.toggle-split");
            c.key("v");
            c.key("j");
            c.key("space");
        },
    ));
    t.add(s(
        "split-modal",
        "The split dialog: the two description editors.",
        "Pick files, press X.",
        |c| {
            c.key("x");
            c.key("X");
        },
    ));
    t.add(s(
        "discard-confirm",
        "Confirm dialog for discarding picked changes.",
        "Pick a file, run Discard selected changes.",
        |c| {
            c.key("x");
            c.cmd("changes.discard");
        },
    ));

    t.group("Description editing");
    t.add(s(
        "description-editor",
        "Inline description editor above the diff with Save and Cancel.",
        "Press d.",
        |c| {
            c.select("cache-bindings");
            c.key("d");
        },
    ));
    t.add(s("description-editor-multiline", "Editor grown to fit a typed multi-line message.", "d, then type.", |c| { c.key("d"); c.typed("A new description for the working copy"); c.send_enter(); c.send_enter(); c.typed("With a second paragraph that runs on for a while so the editor shows its wrapping behaviour on long lines of prose."); }));
    t.add(s(
        "commit-editor",
        "The same editor in commit mode.",
        "Press c on @.",
        |c| {
            c.key("c");
        },
    ));

    t.group("Dialogs");
    t.add(s(
        "confirm-abandon",
        "Confirm dialog: abandon a revision.",
        "Select, a.",
        |c| {
            c.select("cache-bindings");
            c.key("a");
        },
    ));
    t.add(s(
        "confirm-abandon-many",
        "Confirm dialog naming several marked revisions.",
        "Mark rows, a.",
        |c| {
            c.select("cache-bindings");
            c.mark("cache-bindings");
            c.mark("many-files");
            c.key("a");
        },
    ));
    t.add(s(
        "confirm-immutable",
        "Guarded edit of an immutable revision: the confirm that offers to ignore immutability.",
        "Select main, abandon or edit it.",
        |c| {
            c.select("main");
            c.key("e");
        },
    ));
    t.add(s(
        "confirm-push-tracked",
        "Push confirm listing what goes out.",
        "Press P.",
        |c| {
            c.key("P");
        },
    ));
    t.add(
        s(
            "confirm-push-tracked-dark",
            "confirm-push-tracked in dark.",
            "As confirm-push-tracked, theme Dark.",
            |c| {
                c.key("P");
            },
        )
        .dark(),
    );
    t.add(s(
        "confirm-push-working-copy",
        "The warning for pushing the working-copy commit.",
        "Palette 'Push as new bookmark' on @.",
        |c| {
            c.cmd("revision.push-change");
        },
    ));
    t.add(s(
        "confirm-rebase-trunk-conflicts",
        "Rebase onto trunk's forecast confirm naming the changes that end up conflicted.",
        "Run 'Whole branch onto trunk' on the docs(changelog) revision.",
        |c| {
            all_revs(c);
            c.select("trunk-conflict");
            c.try_cmd("revision.rebase.onto-trunk");
            c.pump_ms(1200);
            c.settle();
        },
    ));
    t.add(s(
        "confirm-restore-operation",
        "Confirm restoring the repo to an operation.",
        "History view, select an op, Restore.",
        |c| {
            c.key("O");
            c.settle();
            c.select_op(3);
            c.try_cmd("history.restore-operation");
        },
    ));
    t.add(s(
        "confirm-push-refused",
        "The dialog for a push jj's checks refused (conflicted commit).",
        "Push a bookmark on a conflicted revision.",
        |c| {
            all_revs(c);
            c.cmd_arg(
                "bookmark.push",
                CommandArg::Bookmark {
                    name: "conflict/settings".to_owned(),
                    remote: Some("origin".to_owned()),
                    to: None,
                },
            );
            c.pump_ms(800);
            c.settle();
        },
    ));
    t.add(s(
        "confirm-push-new",
        "Push of a new local bookmark naming the remote.",
        "Bookmark row's Push.",
        |c| {
            c.cmd_arg(
                "bookmark.push",
                CommandArg::Bookmark {
                    name: "fix/parser-crash".to_owned(),
                    remote: Some("origin".to_owned()),
                    to: None,
                },
            );
            c.pump_ms(800);
            c.settle();
        },
    ));
    t.add(s(
        "confirm-restore-file",
        "Discard a file's changes.",
        "File row's Restore from parent.",
        |c| {
            c.file("src/main.rs");
            c.try_cmd("file.restore");
        },
    ));
    t.add(
        s(
            "confirm-dark",
            "confirm-abandon in dark.",
            "As confirm-abandon, theme Dark.",
            |c| {
                c.select("cache-bindings");
                c.key("a");
            },
        )
        .dark(),
    );
    t.add(s(
        "name-prompt-create",
        "Bookmark name prompt, empty.",
        "Press B.",
        |c| {
            c.select("cache-bindings");
            c.key("B");
        },
    ));
    t.add(s(
        "name-prompt-invalid",
        "Prompt with an invalid name: the verdict line.",
        "B, type 'bad name'.",
        |c| {
            c.select("cache-bindings");
            c.key("B");
            c.typed("bad name");
        },
    ));
    t.add(s(
        "name-prompt-exists",
        "Prompt with an existing name: move instead wording.",
        "B, type 'main'.",
        |c| {
            c.select("cache-bindings");
            c.key("B");
            c.typed("main");
        },
    ));
    t.add(s(
        "name-prompt-rename",
        "Prompt in rename mode.",
        "Bookmark row's Rename….",
        |c| {
            c.cmd_arg(
                "bookmark.rename",
                CommandArg::Bookmark {
                    name: "fix/parser-crash".to_owned(),
                    remote: None,
                    to: None,
                },
            );
        },
    ));
    t.add(s(
        "open-repo-dialog",
        "Open-repository dialog over a tab, recents listed.",
        "Have opened another repository before, cmd+O.",
        |c| {
            second_repo(c, "older-repo");
            c.key("cmd+w");
            c.key("cmd+o");
        },
    ));
    t.add(s(
        "open-repo-typed",
        "Dialog with a typed path and folder suggestions highlighted.",
        "cmd+O, type a path, down.",
        |c| {
            c.key("cmd+o");
            let dir = c.repo.parent().unwrap().to_string_lossy().into_owned();
            c.typed(&format!("{dir}/"));
            c.key("down");
        },
    ));
    t.add(s(
        "open-repo-not-repo",
        "Dialog after submitting a path that isn't a repository: the open's own error in the status line.",
        "cmd+O, type /usr/bin, Enter.",
        |c| {
            c.key("cmd+o");
            c.typed("/usr/bin");
            c.key("enter");
        },
    ));
    t.add(s(
        "open-repo-repository",
        "A typed path that is a repository: the status line says so and what Enter does.",
        "cmd+O, type the fixture repository's path.",
        |c| {
            c.key("cmd+o");
            let path = c.repo.to_string_lossy().into_owned();
            c.typed(&path);
            c.pump_ms(300);
        },
    ));
    t.add(s(
        "open-repo-no-match",
        "A typed name no folder starts with: the warning status line.",
        "cmd+O, type a folder and a name nothing matches.",
        |c| {
            c.key("cmd+o");
            let dir = c.repo.parent().unwrap().to_string_lossy().into_owned();
            c.typed(&format!("{dir}/zzq"));
            c.pump_ms(300);
        },
    ));
    t.add(s(
        "open-repo-pull-request",
        "A pull request reference: its status line and the card saying how it opens.",
        "cmd+O, type acme/render-toolkit#42.",
        |c| {
            c.key("cmd+o");
            c.typed("acme/render-toolkit#42");
        },
    ));
    t.add(
        s(
            "open-repo-typed-dark",
            "open-repo-typed in dark.",
            "As open-repo-typed, theme Dark.",
            |c| {
                c.key("cmd+o");
                let dir = c.repo.parent().unwrap().to_string_lossy().into_owned();
                c.typed(&format!("{dir}/"));
                c.key("down");
            },
        )
        .dark(),
    );
    t.add(s(
        "shortcuts-sheet",
        "The keyboard-shortcut cheat sheet over the window.",
        "Press ?.",
        |c| {
            c.key("?");
        },
    ));
    t.add(
        s(
            "shortcuts-dark",
            "The cheat sheet in dark.",
            "Press ?, theme Dark.",
            |c| {
                c.key("?");
            },
        )
        .dark(),
    );
    t.add(
        s(
            "shortcuts-welcome",
            "The cheat sheet over the welcome screen.",
            "Welcome, click Keyboard shortcuts (the card's input has focus, so ? types).",
            |c| {
                c.act(Action::ToggleShortcuts);
            },
        )
        .no_repo(),
    );

    t.group("Settings");
    t.add(s(
        "settings-dialog",
        "The settings dialog over the window, every row editable.",
        "Press cmd+,.",
        |c| {
            c.key("cmd+,");
        },
    ));
    t.add(
        s(
            "settings-dialog-dark",
            "The settings dialog in dark.",
            "Press cmd+,, theme Dark.",
            |c| {
                c.key("cmd+,");
            },
        )
        .dark(),
    );
    t.add(
        s(
            "settings-locked-broken",
            "Rows config.toml sets are locked; a settings.toml that doesn't parse shows its banner.",
            "Launch with config.toml setting theme, refresh and code_font_size, and a broken settings.toml; press cmd+,.",
            |c| {
                c.key("cmd+,");
            },
        )
        .config("theme = \"Light\"\nrefresh = \"Manual\"\ncode_font_size = 14\n")
        .settings("multi_click_ms = = 400\n"),
    );

    t.group("Credential prompt");
    t.add(s(
        "credential-password",
        "git asking for a password: masked field, subject in mono.",
        "Push over https to a remote that needs auth. The harness injects the prompt event.",
        |c| {
            prompt(
                c,
                PromptKind::Secret,
                "Password for 'https://ada@example.com':",
                Some("https://ada@example.com"),
                None,
            );
        },
    ));
    t.add(s(
        "credential-username",
        "git asking for a username: plain field.",
        "As above, first prompt.",
        |c| {
            prompt(
                c,
                PromptKind::Text,
                "Username for 'https://example.com':",
                Some("https://example.com"),
                None,
            );
        },
    ));
    t.add(s("credential-host-key", "ssh's unknown host key question: fingerprint and Yes/Cancel.", "First ssh to a new host.", |c| { prompt(c, PromptKind::YesNo, "The authenticity of host 'git.example.com (203.0.113.7)' can't be established.\nED25519 key fingerprint is SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG.\nAre you sure you want to continue connecting (yes/no/[fingerprint])?", Some("git.example.com"), Some("SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG")); }));
    t.add(s(
        "credential-passphrase",
        "ssh key passphrase.",
        "Push over ssh with an encrypted key.",
        |c| {
            prompt(
                c,
                PromptKind::Secret,
                "Enter passphrase for key '/Users/ada/.ssh/id_ed25519':",
                Some("/Users/ada/.ssh/id_ed25519"),
                None,
            );
        },
    ));
    t.add(s(
        "credential-notice",
        "A notice with nothing to answer (touch your security key).",
        "ssh with a hardware key.",
        |c| {
            prompt(
                c,
                PromptKind::Notice,
                "Confirm user presence for key ED25519-SK SHA256:abc",
                None,
                None,
            );
        },
    ));
    t.add(
        s(
            "credential-dark",
            "Password prompt in dark.",
            "As credential-password, theme Dark.",
            |c| {
                prompt(
                    c,
                    PromptKind::Secret,
                    "Password for 'https://ada@example.com':",
                    Some("https://ada@example.com"),
                    None,
                );
            },
        )
        .dark(),
    );

    t.group("Activity and toasts");
    t.add(s(
        "activity-popover",
        "Activity popover: rows with status icons, titles, results, elapsed times.",
        "Refresh and fetch, click the activity indicator.",
        |c| {
            c.act(Action::Refresh);
            c.key("f");
            c.settle();
            c.act(Action::ToggleActivityPopover);
        },
    ));
    t.add(s(
        "activity-popover-expanded",
        "A row expanded: captured output in a code block.",
        "Push, open the popover, expand the push row.",
        |c| {
            c.act(Action::Bookmark(
                crate::bookmarks::BookmarkAction::PushTracked,
            ));
            c.settle();
            c.confirm();
            c.settle();
            c.act(Action::ToggleActivityPopover);
            c.expand_first_activity();
        },
    ));
    t.add(s(
        "activity-popover-failed",
        "A failed row: its error text.",
        "Fail a fetch, open the popover.",
        |c| {
            let git = c.repo.join(".git/config");
            let config = std::fs::read_to_string(&git).unwrap();
            let mut lines = String::new();
            for line in config.lines() {
                if line.trim_start().starts_with("url") {
                    lines.push_str("\turl = /nonexistent/remote.git\n");
                } else {
                    lines.push_str(line);
                    lines.push('\n');
                }
            }
            std::fs::write(&git, lines).unwrap();
            c.key("f");
            c.settle();
            c.act(Action::ToggleActivityPopover);
        },
    ));
    t.add(
        s(
            "activity-popover-dark",
            "The popover in dark.",
            "As activity-popover, theme Dark.",
            |c| {
                c.act(Action::Refresh);
                c.key("f");
                c.settle();
                c.act(Action::ToggleActivityPopover);
            },
        )
        .dark(),
    );
    t.add(
        s(
            "toast-config-error",
            "Error toast at startup for a config file that doesn't parse.",
            "Launch with a broken config.toml.",
            |c| {
                c.pump_ms(100);
            },
        )
        .config("this is = = not toml\n"),
    );
    t.add(s(
        "toast-operation-failed",
        "Error toast from a failed operation.",
        "Fail a fetch.",
        |c| {
            let git = c.repo.join(".git/config");
            let config = std::fs::read_to_string(&git).unwrap();
            let mut lines = String::new();
            for line in config.lines() {
                if line.trim_start().starts_with("url") {
                    lines.push_str("\turl = /nonexistent/remote.git\n");
                } else {
                    lines.push_str(line);
                    lines.push('\n');
                }
            }
            std::fs::write(&git, lines).unwrap();
            c.key("f");
            c.settle();
        },
    ));

    t.group("Source view");
    t.add(s(
        "source-file",
        "Source view: full tree on the left, selected file's code with line numbers.",
        "Source segment, click a file.",
        |c| {
            c.cmd("view.source");
            c.source_file("src/main.rs");
        },
    ));
    t.add(s(
        "source-no-file",
        "Source view with no file to open first: 'Select a file to view its source'.",
        "Select an empty revision, Source segment.",
        |c| {
            all_revs(c);
            c.select("empty-revision");
            c.cmd("view.source");
        },
    ));
    t.add(s("source-opened", "Source view as it opens from the working copy: the first changed file is picked, here a PNG, which the source view doesn't preview.", "Source segment.", |c| { c.cmd("view.source"); }));
    t.add(s(
        "source-filter",
        "Fuzzy file filter in the tree's search box.",
        "Type in the filter.",
        |c| {
            c.cmd("view.source");
            c.focus(crate::source_panel::SOURCE_FILTER_INPUT_ID);
            c.typed("frame");
        },
    ));
    t.add(s(
        "source-annotate",
        "Annotate on: blame column beside the code.",
        "Press A.",
        |c| {
            c.cmd("view.source");
            c.source_file("src/main.rs");
            c.key("A");
            c.pump_ms(600);
            c.settle();
        },
    ));
    t.add(s(
        "source-long-lines",
        "A file of very long lines.",
        "Open src/generated/minified.js.",
        |c| {
            c.cmd("view.source");
            c.source_file("src/generated/minified.js");
        },
    ));
    t.add(s(
        "source-image",
        "An image in the source view: 'Binary file, not shown', no preview.",
        "Open assets/hero.png.",
        |c| {
            c.cmd("view.source");
            c.source_file("assets/hero.png");
        },
    ));
    t.add(s(
        "source-binary",
        "A binary file in the source view.",
        "Open vendor/blob.bin.",
        |c| {
            c.cmd("view.source");
            c.source_file("vendor/blob.bin");
        },
    ));
    t.add(s(
        "source-cjk",
        "CJK text in the source view.",
        "Open i18n/ja.txt.",
        |c| {
            c.cmd("view.source");
            c.source_file("i18n/ja.txt");
        },
    ));
    t.add(s(
        "source-find",
        "Find bar over the source view.",
        "cmd+F, type 'render'.",
        |c| {
            c.cmd("view.source");
            c.source_file("src/main.rs");
            c.key("cmd+f");
            c.typed("render");
        },
    ));
    t.add(
        s(
            "source-dark",
            "Source view in dark.",
            "As source-file, theme Dark.",
            |c| {
                c.cmd("view.source");
                c.source_file("src/main.rs");
            },
        )
        .dark(),
    );

    t.group("History view");
    t.add(s("history-oplog", "History view: operation log on the left (snapshot runs folded), selected operation's changes on the right.", "Press O.", |c| { c.key("O"); c.settle(); }));
    t.add(s("history-op-selected", "An older operation selected: what it changed, changed-commit list, a commit's diff, restore and revert.", "Press O, click an operation.", |c| { c.key("O"); c.settle(); c.select_op(4); }));
    t.add(s(
        "history-evolog",
        "Evolution log of a revision that was rewritten, with interdiffs.",
        "Select a revision, press E.",
        |c| {
            c.select("feature/auth");
            c.key("E");
            c.settle();
        },
    ));
    t.add(s(
        "history-evolog-selected",
        "An older evolog version selected.",
        "Select a version.",
        |c| {
            c.select("feature/auth");
            c.key("E");
            c.settle();
            c.act(Action::History(
                crate::history_panel::HistoryAction::SelectVersion(1),
            ));
        },
    ));
    t.add(
        s(
            "history-dark",
            "The operation log in dark.",
            "As history-oplog, theme Dark.",
            |c| {
                c.key("O");
                c.settle();
            },
        )
        .dark(),
    );
    t.add(s(
        "file-history",
        "File history filter: list narrowed to revisions touching one path, banner above it.",
        "Select a file, press H.",
        |c| {
            c.select("kitchen-sink");
            c.file("src/main.rs");
            c.key("H");
            c.settle();
        },
    ));

    t.group("Compare and range");
    t.add(s("compare-with-wc", "Comparing a revision with the working copy: the 'Comparing A → B' banner with swap, interdiff, exit; hint strip.", "Palette 'Compare with working copy'.", |c| { c.select("cache-bindings"); c.cmd("compare.with-working-copy"); }));
    t.add(s(
        "compare-picking",
        "Compare with… armed: the banner asks to click another revision.",
        "Press C.",
        |c| {
            c.select("cache-bindings");
            c.key("C");
        },
    ));
    t.add(s(
        "compare-interdiff",
        "Interdiff mode of a comparison.",
        "Comparing, press Interdiff.",
        |c| {
            c.select("cache-bindings");
            c.cmd("compare.with-working-copy");
            c.try_cmd("compare.interdiff");
        },
    ));
    t.add(s(
        "range-since-trunk",
        "Diff since trunk: range banner and the commit list standing in for commit info.",
        "Palette 'Diff since trunk'.",
        |c| {
            c.select("cache-bindings");
            c.cmd("diff.since-trunk");
        },
    ));
    t.add(s(
        "range-typed",
        "A typed range diff from the command line.",
        "':' then \"diff -r 'main..feature/render-pipeline'\".",
        |c| {
            c.key(":");
            c.typed("diff -r main..feature/render-pipeline");
            c.key("enter");
            c.settle();
        },
    ));
    t.add(s(
        "range-empty",
        "A range with no commits: its empty state.",
        "Diff since trunk on a revision already in trunk.",
        |c| {
            c.select("main");
            c.try_cmd("diff.since-trunk");
        },
    ));
    t.add(s(
        "range-span-marked",
        "shift-click span: two connected marked rows shown as one diff.",
        "Click a row, shift-click another.",
        |c| {
            c.select("cache-bindings");
            c.shift_mark("kitchen-sink");
        },
    ));
    t.add(s(
        "compare-marked",
        "Two marked rows compared against each other.",
        "cmd-click two rows, palette 'Compare marked revisions'.",
        |c| {
            c.select("many-files");
            c.mark("many-files");
            c.mark("empty-revision");
            c.try_cmd("compare.marked");
        },
    ));
    t.add(s(
        "range-disconnected",
        "A span over rows that aren't connected: 'Not a connected range'.",
        "Click a row, shift-click rows on other branches.",
        |c| {
            // The baseline's rows turned out connected. These two skip the one between.
            let marked = vec![
                c.commit_of("feature/render-pipeline"),
                c.commit_of("kitchen-sink"),
            ];
            c.h.act(|ui, _| {
                ui.active_mut().revision_multi_selection = marked;
                ui.sync_span(true);
            });
            c.settle();
        },
    ));

    t.group("Conflicts");
    t.add(s("conflict-revision", "A conflicted revision selected: conflict banner, regions in the diff with take-side controls, file marked in the tree.", "Select the conflicted revision.", |c| { all_revs(c); c.select("conflicted"); }));
    t.add(s(
        "conflict-config-file",
        "The conflicted src/config.rs: region controls and hint strip keys.",
        "Select src/config.rs.",
        |c| {
            all_revs(c);
            c.select("conflicted");
            c.file("src/config.rs");
        },
    ));
    t.add(s(
        "conflict-modify-delete",
        "A modify/delete conflict: no regions to pick, take-a-side only.",
        "Select src/removed.rs.",
        |c| {
            all_revs(c);
            c.select("conflicted");
            c.file("src/removed.rs");
        },
    ));
    t.add(s(
        "conflict-octopus",
        "A three-sided conflict from the octopus merge.",
        "Select the octopus merge.",
        |c| {
            all_revs(c);
            c.select("conflict/octopus");
            c.file("docs/guide.md");
        },
    ));
    t.add(s(
        "conflict-marker-editor",
        "Marker editor open on a conflicted file with Save and Cancel.",
        "Palette 'Edit conflict markers'.",
        |c| {
            all_revs(c);
            c.select("conflicted");
            c.file("src/config.rs");
            c.try_cmd("conflict.edit-markers");
        },
    ));
    t.add(s(
        "conflict-split",
        "Conflict regions in side by side.",
        "Toggle split.",
        |c| {
            all_revs(c);
            c.select("conflicted");
            c.file("src/config.rs");
            c.cmd("view.toggle-split");
        },
    ));
    t.add(
        s(
            "conflict-dark",
            "A conflicted file in dark.",
            "As conflict-config-file, theme Dark.",
            |c| {
                all_revs(c);
                c.select("conflicted");
                c.file("src/config.rs");
            },
        )
        .dark(),
    );
    t.add(
        s(
            "conflict-working-copy",
            "The working copy itself conflicted.",
            "jj new on the conflicted revision, then open.",
            |c| {
                c.file("src/config.rs");
            },
        )
        .prepare(|repo| {
            jj(repo, &["new", "conflict/settings"]);
        }),
    );
    t.add(s(
        "graph-conflict-preset",
        "The Conflicts preset revset: only conflicted rows.",
        "Revset conflicts().",
        |c| {
            c.act(Action::SetRevset("conflicts()".to_owned()));
        },
    ));

    t.group("Menus (the overlay; macOS shows native NSMenus)");
    t.add(s("menu-revision", "Revision context menu at a row, row glow. Content and order match the native menu; the look is the non-macOS overlay.", "Right-click a revision row.", |c| { c.right_click(180.0, 180.0); }).animated());
    t.add(
        s(
            "menu-revision-submenu",
            "The same menu with a flyout open.",
            "Right-click, hover a submenu row.",
            |c| {
                c.right_click(180.0, 180.0);
                c.open_first_submenu();
            },
        )
        .animated(),
    );
    t.add(
        s(
            "menu-revision-immutable",
            "The menu on an immutable revision. The glow's position is nominal here.",
            "Right-click main's row.",
            |c| {
                c.select("main");
                c.right_click_selected();
            },
        )
        .animated(),
    );
    t.add(
        s(
            "menu-revision-marked",
            "The menu with several rows marked: batch verbs.",
            "Mark rows, right-click one.",
            |c| {
                c.select("cache-bindings");
                c.mark("cache-bindings");
                c.mark("many-files");
                c.right_click_selected();
            },
        )
        .animated(),
    );
    t.add(s(
        "menu-file",
        "File context menu in the tree.",
        "Right-click a file row.",
        |c| {
            c.select("kitchen-sink");
            c.right_click(460.0, 180.0);
        },
    ));
    t.add(s(
        "menu-fetch",
        "Fetch dropdown.",
        "Click Fetch's caret.",
        |c| {
            c.open_toolbar_menu(crate::ToolbarMenu::FetchBranches);
        },
    ));
    t.add(s(
        "menu-revset-presets",
        "Revset presets dropdown.",
        "Click the revset field's caret.",
        |c| {
            c.open_toolbar_menu(crate::ToolbarMenu::RevsetPresets);
        },
    ));

    t.group("Narrow and long-text stress");
    t.add(
        s(
            "narrow-base",
            "Narrow window (900×620): panes clamp, split unavailable, toolbar trims.",
            "Resize the window.",
            |_c| {},
        )
        .size(900.0, 620.0),
    );
    t.add(
        s(
            "narrow-very",
            "Very narrow window (640×480).",
            "Resize smaller.",
            |_c| {},
        )
        .size(640.0, 480.0),
    );
    t.add(
        s(
            "narrow-split-fallback",
            "Split on but the pane too narrow: falls back to unified.",
            "Toggle split in a narrow window.",
            |c| {
                c.select("kitchen-sink");
                c.cmd("view.toggle-split");
            },
        )
        .size(900.0, 620.0),
    );
    t.add(
        s(
            "narrow-palette",
            "Palette in a narrow window.",
            "cmd+K.",
            |c| {
                c.key("cmd+k");
                c.typed("render");
            },
        )
        .size(640.0, 480.0),
    );
    t.add(
        s(
            "narrow-confirm",
            "Confirm dialog in a narrow window.",
            "a.",
            |c| {
                c.select("cache-bindings");
                c.key("a");
            },
        )
        .size(640.0, 480.0),
    );
    t.add(
        s(
            "narrow-find",
            "Find bar in a narrow window.",
            "cmd+F.",
            |c| {
                c.select("kitchen-sink");
                c.key("cmd+f");
                c.typed("render");
            },
        )
        .size(900.0, 620.0),
    );
    t.add(
        s(
            "narrow-dark",
            "Narrow window in dark.",
            "Resize, theme Dark.",
            |c| {
                c.select("kitchen-sink");
            },
        )
        .size(900.0, 620.0)
        .dark(),
    );
    t.add(
        s(
            "large-window",
            "A large window (2200×1300): widths don't stretch dialogs, panes keep their sizes.",
            "Maximize.",
            |c| {
                c.select("kitchen-sink");
            },
        )
        .size(2200.0, 1300.0),
    );
    t.add(s("longtext-graph-narrow-sidebar", "Long bookmark and long title in the graph at the sidebar's minimum width: chips and subject ellipsise before the gutter.", "Drag the sidebar handle to its minimum.", |c| { all_revs(c); c.select("long-description"); c.h.act(|ui, _| { let width = ui.sidebar_min_width + 1.0; ui.on_sidebar_width_changed(width); }); c.settle(); }));
    t.add(s(
        "longtext-tree-narrow-files",
        "Deep paths in a narrow file tree.",
        "Drag the files handle to its minimum.",
        |c| {
            c.select("kitchen-sink");
            c.h.act(|ui, _| {
                let position = ui.sidebar_width + 2.0 + crate::diff_panel::MIN_FILE_NAV_WIDTH;
                ui.on_changed_files_width_changed(position);
            });
            c.settle();
        },
    ));
    t.add(s(
        "longtext-header-hover",
        "Pointer resting on an ellipsised commit title. This title has no expand-on-hover; the full text is in the body and the palette.",
        "Hover the title.",
        |c| {
            all_revs(c);
            c.select("long-description");
            c.hover(900.0, 97.0);
        },
    ));
    t.add(s(
        "hover-row",
        "Pointer resting on a graph row: hover treatment; the action buttons appear.",
        "Hover a row.",
        |c| {
            c.hover(180.0, 240.0);
        },
    ));
    t.add(s(
        "view-switcher-main-view",
        "History view's segment selected via the switcher.",
        "Click the History segment.",
        |c| {
            c.act(Action::SetMainView(MainView::History));
        },
    ));

    t.group("Declared commands");
    t.add(
        s(
            "commands-palette",
            "Declared commands under their own heading: one runs, one that needs a bookmark is greyed with why, one whose when-clause doesn't hold says so.",
            "Declare them in config.toml, select a revision without a bookmark, cmd+K, type >staging.",
            |c| {
                c.select("no-description");
                c.key("cmd+k");
                c.typed(">staging");
            },
        )
        .config(DECLARED),
    );
    t.add(
        s(
            "commands-choose",
            "A declared command's choose prompt: its choices in a palette column, the prompt's label in the footer.",
            "Run a declared command whose prompt is a choose.",
            |c| {
                c.select("no-description");
                c.cmd("user.rebase-staging");
            },
        )
        .config(DECLARED),
    );
    t.add(s(
        "commands-generate",
        "The description editor with Generate beside Cancel and Save.",
        "d on a revision.",
        |c| {
            c.select("no-description");
            c.key("d");
        },
    ));
    t.add(
        s(
            "commands-generate-dark",
            "commands-generate in dark.",
            "d on a revision, theme Dark.",
            |c| {
                c.select("no-description");
                c.key("d");
            },
        )
        .dark(),
    );

    t.scenes
}

/// The `config.toml` the declared-command scenes run with.
const DECLARED: &str = r#"
[[commands]]
id = "user.deploy-staging"
label = "Deploy to staging"
exec = ["true"]

[[commands]]
id = "user.push-staging"
label = "Push bookmark to staging"
run = "jj git push -b {bookmark} --remote staging"

[[commands]]
id = "user.open-staging-pr"
label = "Open staging PR"
when = "bookmark.has_remote"
exec = ["gh", "pr", "view", "--web", "{bookmark}"]

[[commands]]
id = "user.rebase-staging"
label = "Rebase onto a staging branch"
run = "jj rebase -r {change_id} -o {prompt.target}"

[[commands.prompts]]
name = "target"
kind = "choose"
label = "Rebase onto"
choices = ["staging", "staging-next", "release/1.0"]
"#;

/// The ids of `fixtures/scenes.md`, in order.
fn baseline_ids() -> Vec<String> {
    let md =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/scenes.md"))
            .expect("fixtures/scenes.md");
    md.lines()
        .filter_map(|line| line.strip_prefix("### `")?.strip_suffix('`'))
        .map(str::to_owned)
        .collect()
}

fn read_manifest(repo: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(repo.join(".jj/fixture-manifest"))
        .or_else(|_| std::fs::read_to_string(repo.join(".git/fixture-manifest")))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(' ');
            let names = parts.next()?.to_owned();
            parts.next()?;
            Some((names, parts.next()?.to_owned()))
        })
        .collect()
}

fn run(scene: &Scene) -> Result<(), String> {
    let repo = crate::harness::fresh_repo_named("acme/render-toolkit");
    (scene.prepare)(&repo);
    let manifest = read_manifest(&repo);
    let open = if scene.id == "welcome-open-error" {
        Some(repo.join("does/not/exist"))
    } else {
        scene.open_repo.then(|| repo.clone())
    };
    // `DIFFUI_SCENE_THEME=Dark` reshoots light scenes in another theme, as
    // `<id>-dark.png`.
    let theme_override = std::env::var("DIFFUI_SCENE_THEME").ok();
    let theme = theme_override.as_deref().unwrap_or(scene.theme);
    // In `settings.toml`, where the dialog would put it, so a scene's
    // `config.toml` holds only what the scene sets.
    let settings = format!("theme = \"{theme}\"\n{}", scene.settings);
    let h = Harness::open_fresh(open.as_deref(), scene.size, scene.config, &settings);
    let mut c = Ctx {
        h,
        manifest,
        repo,
        held: None,
    };
    if !scene.cold {
        c.wait_for_load();
    }
    (scene.steps)(&mut c);
    if !scene.raw {
        c.settle();
    }
    match &theme_override {
        Some(theme) => {
            c.h.shot_now(LANE, &format!("{}-{}", scene.id, theme.to_lowercase()))
        }
        None => c.h.shot_now(LANE, scene.id),
    };
    if scene.raw || scene.animated {
        return Ok(());
    }
    c.h.same_after_a_refresh()
        .map_err(|diff| format!("stale: {diff}"))
}

impl Ctx {
    fn wait_for_load(&mut self) {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(30) {
            let done = self.h.read(|ui| {
                ui.tabs.is_empty()
                    || matches!(
                        ui.active().session.status,
                        crate::LoadStatus::Loaded | crate::LoadStatus::Failed(_)
                    )
            });
            if done {
                break;
            }
            self.pump_ms(10);
        }
        self.settle();
    }
}

/// The table covers `scenes.md` exactly.
#[test]
fn the_table_is_the_baselines() {
    let mut mine: Vec<String> = all().iter().map(|scene| scene.id.to_owned()).collect();
    let mut baseline = baseline_ids();
    mine.sort();
    baseline.sort();
    assert_eq!(mine, baseline);
}

/// Shoots every scene, or the ones `DIFFUI_SCENES` names.
#[test]
#[ignore = "shoots the whole baseline: run on demand"]
fn every() {
    let only: Option<Vec<String>> = std::env::var("DIFFUI_SCENES")
        .ok()
        .map(|list| list.split(',').map(str::to_owned).collect());
    let mut failed = Vec::new();
    for scene in all().iter().filter(|scene| {
        only.as_ref()
            .is_none_or(|only| only.iter().any(|id| id == scene.id))
    }) {
        let started = std::time::Instant::now();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(scene)))
            .unwrap_or_else(|panic| {
                Err(panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                    .unwrap_or_else(|| "panicked".to_owned()))
            });
        match outcome {
            Ok(()) => eprintln!("ok   {} ({:.1?})", scene.id, started.elapsed()),
            Err(error) => {
                eprintln!("FAIL {} ({:.1?}): {error}", scene.id, started.elapsed());
                failed.push(format!("{}: {error}", scene.id));
            }
        }
    }
    assert!(
        failed.is_empty(),
        "{} scenes failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

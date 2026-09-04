//! The mode stack: what owns the keyboard, and what is on screen because of
//! it.
//!
//! An overlay used to be a field of its own — `palette: Option<…>`,
//! `confirm: Option<…>`, `find: Option<…>` — and "who owns the keyboard" was a
//! tuple of eight booleans read in a hardcoded order inside `subscription`.
//! Every bug that model produced was the same bug: a flag that was set while
//! its overlay wasn't visible (the description editor eating keys after a view
//! switch), or one that wasn't checked at all (the popup menu letting keys fall
//! through to file navigation).
//!
//! Now there are two stacks and the top of them decides. Global overlays live
//! on [`crate::Diffui`]; the ones bound to one tab's document live on
//! [`crate::TabState`], and the keyboard reads the *active* tab's — so a tab
//! switch can never leave an off-screen mode holding the keys.
//!
//! Layering is deliberate: [`Mode::Confirm`] stacks (a second guarded mutation
//! arriving while one dialog is open queues behind it and is raised when that
//! one resolves), every other kind replaces its own peer.

use iced::keyboard;

use crate::commands::Context;
use crate::find::FindState;
use crate::keymap::{Chord, ChordKey};
use crate::menu::OverlayMenu;
use crate::palette::PaletteState;
use crate::{ConfirmDialog, DescriptionEditor, Diffui, DraftUi, OpenRepoDialog, TabState};

/// A window-level mode. These sit above every tab, so they live on the app.
#[derive(Debug)]
pub(crate) enum Mode {
    Palette(PaletteState),
    /// A popup menu (toolbar dropdown or right-click). Non-macOS only; macOS
    /// pops a native `NSMenu`, which owns the keyboard itself while it runs,
    /// so nothing there constructs this variant and the dead-code lint would
    /// otherwise fail a macOS build with warnings denied.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Menu(OverlayMenu),
    Confirm(ConfirmDialog),
    OpenRepo(OpenRepoDialog),
    ActivityPopover,
}

/// A mode bound to one tab: it names rows, files, or text that belong to that
/// tab's document and would point at the wrong thing anywhere else.
#[derive(Debug)]
pub(crate) enum TabMode {
    Find(FindState),
    Description(DescriptionEditor),
    /// Target mode — an in-progress rebase/squash/merge destination pick.
    Draft(DraftUi),
}

/// What a mode is, independent of what it holds. Used to decide whether a push
/// replaces an existing mode or stacks on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeKind {
    Palette,
    Menu,
    Confirm,
    OpenRepo,
    ActivityPopover,
    Find,
    Description,
    Draft,
}

impl Mode {
    pub(crate) fn kind(&self) -> ModeKind {
        match self {
            Mode::Palette(_) => ModeKind::Palette,
            Mode::Menu(_) => ModeKind::Menu,
            Mode::Confirm(_) => ModeKind::Confirm,
            Mode::OpenRepo(_) => ModeKind::OpenRepo,
            Mode::ActivityPopover => ModeKind::ActivityPopover,
        }
    }

    pub(crate) fn context(&self) -> Context {
        match self {
            Mode::Palette(_) => Context::Palette,
            Mode::Menu(_) => Context::Menu,
            Mode::Confirm(_) => Context::Confirm,
            Mode::OpenRepo(_) => Context::OpenRepo,
            Mode::ActivityPopover => Context::ActivityPopover,
        }
    }
}

impl TabMode {
    pub(crate) fn kind(&self) -> ModeKind {
        match self {
            TabMode::Find(_) => ModeKind::Find,
            TabMode::Description(_) => ModeKind::Description,
            TabMode::Draft(_) => ModeKind::Draft,
        }
    }

    pub(crate) fn context(&self) -> Context {
        match self {
            TabMode::Find(_) => Context::Find,
            TabMode::Description(_) => Context::Description,
            TabMode::Draft(_) => Context::Draft,
        }
    }

    /// Where this mode sits in the tab's stack, regardless of the order the
    /// two were opened in.
    ///
    /// The find bar is a strip laid over whatever else is on screen, and the
    /// key it owns is Escape. Push order alone put a draft started after it on
    /// top, so Escape cancelled the draft and left the find bar sitting there
    /// — the opposite of what the visible layering says.
    fn layer(&self) -> u8 {
        match self {
            TabMode::Description(_) | TabMode::Draft(_) => 0,
            TabMode::Find(_) => 1,
        }
    }
}

/// Whether a chord unclaimed by `context` may be re-resolved against
/// [`Context::Base`].
///
/// Target mode is the only one that lets anything past: its plain keys are the
/// destination picker, but ⌘/⌥/⌃ combos (tab switching, the wrap toggle) still
/// belong to the window. Every other mode is modal — a confirmation dialog in
/// particular passes nothing, because the mutation behind it is one the jj CLI
/// itself refuses.
pub(crate) fn passes_through(context: Context, chord: &Chord) -> bool {
    matches!(context, Context::Draft) && chord.mods.any_command_like()
}

/// Whether a chord in `context` requires that no focused widget consumed the
/// key. The revset box is an inline text input with no overlay of its own, so
/// without this its keystrokes would also drive file navigation. Escape is
/// exempt in target mode: leaving the mode has to work from inside that box.
pub(crate) fn needs_unconsumed(context: Context, chord: &Chord) -> bool {
    match context {
        Context::Base => !chord.mods.any_command_like(),
        Context::Draft => {
            !chord.mods.any_command_like()
                && !matches!(chord.key, ChordKey::Named(keyboard::key::Named::Escape))
        }
        _ => false,
    }
}

impl Diffui {
    /// The context the keyboard is in: the top of the global stack, else the
    /// top of the *active* tab's stack, else the base UI.
    pub(crate) fn key_context(&self) -> Context {
        match self.modes.last() {
            Some(mode) => mode.context(),
            None => match self.active().modes.last() {
                Some(mode) => mode.context(),
                None => Context::Base,
            },
        }
    }

    /// Push a global mode. Everything but a confirmation replaces its own kind,
    /// so two palettes (or two menus) cannot coexist.
    pub(crate) fn push_mode(&mut self, mode: Mode) {
        if mode.kind() != ModeKind::Confirm {
            self.modes.retain(|open| open.kind() != mode.kind());
        }
        self.modes.push(mode);
    }

    /// Drop the topmost open mode of `kind` and hand it back. Only
    /// [`ModeKind::Confirm`] ever has more than one open at a time, and a
    /// queued confirmation is meant to survive the one above it resolving —
    /// so this removes one, never the whole kind.
    pub(crate) fn pop_mode(&mut self, kind: ModeKind) -> Option<Mode> {
        let index = self.modes.iter().rposition(|mode| mode.kind() == kind)?;
        Some(self.modes.remove(index))
    }

    pub(crate) fn mode(&self, kind: ModeKind) -> Option<&Mode> {
        self.modes.iter().rev().find(|mode| mode.kind() == kind)
    }

    pub(crate) fn mode_mut(&mut self, kind: ModeKind) -> Option<&mut Mode> {
        self.modes.iter_mut().rev().find(|mode| mode.kind() == kind)
    }

    pub(crate) fn palette(&self) -> Option<&PaletteState> {
        match self.mode(ModeKind::Palette) {
            Some(Mode::Palette(state)) => Some(state),
            _ => None,
        }
    }

    pub(crate) fn palette_mut(&mut self) -> Option<&mut PaletteState> {
        match self.mode_mut(ModeKind::Palette) {
            Some(Mode::Palette(state)) => Some(state),
            _ => None,
        }
    }

    pub(crate) fn menu(&self) -> Option<&OverlayMenu> {
        match self.mode(ModeKind::Menu) {
            Some(Mode::Menu(menu)) => Some(menu),
            _ => None,
        }
    }

    pub(crate) fn menu_mut(&mut self) -> Option<&mut OverlayMenu> {
        match self.mode_mut(ModeKind::Menu) {
            Some(Mode::Menu(menu)) => Some(menu),
            _ => None,
        }
    }

    /// The confirmation on top. A second one raised while this is open sits
    /// under it and surfaces when it resolves.
    pub(crate) fn confirm(&self) -> Option<&ConfirmDialog> {
        match self.mode(ModeKind::Confirm) {
            Some(Mode::Confirm(dialog)) => Some(dialog),
            _ => None,
        }
    }

    pub(crate) fn open_repo_dialog(&self) -> Option<&OpenRepoDialog> {
        match self.mode(ModeKind::OpenRepo) {
            Some(Mode::OpenRepo(dialog)) => Some(dialog),
            _ => None,
        }
    }

    pub(crate) fn open_repo_dialog_mut(&mut self) -> Option<&mut OpenRepoDialog> {
        match self.mode_mut(ModeKind::OpenRepo) {
            Some(Mode::OpenRepo(dialog)) => Some(dialog),
            _ => None,
        }
    }

    pub(crate) fn activity_popover_open(&self) -> bool {
        self.mode(ModeKind::ActivityPopover).is_some()
    }
}

impl TabState {
    /// Push a per-tab mode, replacing any open mode of the same kind and
    /// slotting it in by [layer](TabMode::layer) rather than always on top —
    /// so a find bar opened before a draft starts still owns Escape.
    pub(crate) fn push_mode(&mut self, mode: TabMode) {
        self.modes.retain(|open| open.kind() != mode.kind());
        let at = self
            .modes
            .iter()
            .position(|open| open.layer() > mode.layer())
            .unwrap_or(self.modes.len());
        self.modes.insert(at, mode);
    }

    pub(crate) fn pop_mode(&mut self, kind: ModeKind) -> Option<TabMode> {
        let index = self.modes.iter().rposition(|mode| mode.kind() == kind)?;
        Some(self.modes.remove(index))
    }

    fn mode(&self, kind: ModeKind) -> Option<&TabMode> {
        self.modes.iter().rev().find(|mode| mode.kind() == kind)
    }

    fn mode_mut(&mut self, kind: ModeKind) -> Option<&mut TabMode> {
        self.modes.iter_mut().rev().find(|mode| mode.kind() == kind)
    }

    pub(crate) fn find(&self) -> Option<&FindState> {
        match self.mode(ModeKind::Find) {
            Some(TabMode::Find(state)) => Some(state),
            _ => None,
        }
    }

    pub(crate) fn find_mut(&mut self) -> Option<&mut FindState> {
        match self.mode_mut(ModeKind::Find) {
            Some(TabMode::Find(state)) => Some(state),
            _ => None,
        }
    }

    pub(crate) fn description_editor(&self) -> Option<&DescriptionEditor> {
        match self.mode(ModeKind::Description) {
            Some(TabMode::Description(editor)) => Some(editor),
            _ => None,
        }
    }

    pub(crate) fn description_editor_mut(&mut self) -> Option<&mut DescriptionEditor> {
        match self.mode_mut(ModeKind::Description) {
            Some(TabMode::Description(editor)) => Some(editor),
            _ => None,
        }
    }

    pub(crate) fn op_draft(&self) -> Option<&DraftUi> {
        match self.mode(ModeKind::Draft) {
            Some(TabMode::Draft(draft)) => Some(draft),
            _ => None,
        }
    }

    pub(crate) fn op_draft_mut(&mut self) -> Option<&mut DraftUi> {
        match self.mode_mut(ModeKind::Draft) {
            Some(TabMode::Draft(draft)) => Some(draft),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Chord;

    fn chord(raw: &str) -> Chord {
        Chord::parse(raw).expect("a parsable chord")
    }

    /// Escape belongs to whatever is visually on top, and the find bar is a
    /// strip over the sidebar. Opened before a draft starts, push order alone
    /// buried it — Escape cancelled the draft and left the find bar open.
    #[test]
    fn a_find_bar_stays_above_a_draft_started_after_it() {
        let mut tab = TabState::empty();
        tab.push_mode(TabMode::Find(FindState::default()));
        tab.push_mode(TabMode::Draft(DraftUi::new(diffui_core::OpDraft::squash(
            diffui_core::DraftSource {
                selection: diffui_core::RevisionSelection::WorkingCopy,
                commit_id: "abc".to_owned(),
                label: "abc".to_owned(),
            },
        ))));

        assert_eq!(
            tab.modes.last().map(TabMode::context),
            Some(Context::Find),
            "the find bar keeps the keyboard"
        );
        // …and closing it hands the keyboard back to the draft underneath.
        tab.pop_mode(ModeKind::Find);
        assert_eq!(tab.modes.last().map(TabMode::context), Some(Context::Draft));
    }

    #[test]
    fn a_confirmation_passes_nothing_through() {
        for raw in ["cmd+k", "esc", "j", "alt+z"] {
            assert!(!passes_through(Context::Confirm, &chord(raw)));
        }
    }

    #[test]
    fn target_mode_passes_modifier_combos_but_keeps_the_plain_keys() {
        assert!(passes_through(Context::Draft, &chord("cmd+w")));
        assert!(passes_through(Context::Draft, &chord("alt+z")));
        assert!(!passes_through(Context::Draft, &chord("j")));
        assert!(!passes_through(Context::Draft, &chord("esc")));
    }

    #[test]
    fn a_focused_input_keeps_the_plain_keys_it_is_typing() {
        // Plain keys belong to whatever has focus; combos never do.
        assert!(needs_unconsumed(Context::Base, &chord("j")));
        assert!(needs_unconsumed(Context::Base, &chord("esc")));
        assert!(!needs_unconsumed(Context::Base, &chord("cmd+w")));
        assert!(!needs_unconsumed(Context::Base, &chord("alt+z")));
        // Leaving target mode has to work from inside the revset box.
        assert!(needs_unconsumed(Context::Draft, &chord("j")));
        assert!(!needs_unconsumed(Context::Draft, &chord("esc")));
        // An overlay owns its keys outright; its own input is the focused one.
        assert!(!needs_unconsumed(Context::Palette, &chord("esc")));
        assert!(!needs_unconsumed(Context::Find, &chord("enter")));
    }
}

#[cfg(test)]
mod stack_tests {
    use super::*;
    use crate::tabs::tests::{app, push_tab};
    use crate::{ConfirmDialog, OpenRepoDialog, PendingMutation, TabId, activity, mutations};

    fn confirm(title: &str) -> ConfirmDialog {
        ConfirmDialog {
            title: title.to_owned(),
            body: String::new(),
            confirm_label: "Do it".to_owned(),
            pending: PendingMutation {
                op: mutations::MutationOp::Undo { operation_id: None },
                tab_id: TabId(0),
                activity_id: activity::ActivityId(0),
                allow_immutable: false,
            },
        }
    }

    #[test]
    fn the_top_of_the_stack_owns_the_keyboard() {
        let mut ui = app();
        assert_eq!(ui.key_context(), Context::Base);
        ui.push_mode(Mode::OpenRepo(OpenRepoDialog::default()));
        assert_eq!(ui.key_context(), Context::OpenRepo);
        ui.push_mode(Mode::Confirm(confirm("first")));
        assert_eq!(ui.key_context(), Context::Confirm);
        ui.pop_mode(ModeKind::Confirm);
        assert_eq!(ui.key_context(), Context::OpenRepo);
        ui.pop_mode(ModeKind::OpenRepo);
        assert_eq!(ui.key_context(), Context::Base);
    }

    #[test]
    fn a_second_mode_of_a_kind_replaces_the_first_but_confirmations_queue() {
        let mut ui = app();
        ui.push_mode(Mode::OpenRepo(OpenRepoDialog::default()));
        ui.push_mode(Mode::OpenRepo(OpenRepoDialog::default()));
        assert_eq!(ui.modes.len(), 1, "two dialogs would fight over one card");

        // A confirmation raised while one is open waits under it: dismissing
        // the top one surfaces the next rather than dropping its mutation.
        ui.push_mode(Mode::Confirm(confirm("first")));
        ui.push_mode(Mode::Confirm(confirm("second")));
        assert_eq!(ui.confirm().map(|d| d.title.as_str()), Some("second"));
        ui.pop_mode(ModeKind::Confirm);
        assert_eq!(ui.confirm().map(|d| d.title.as_str()), Some("first"));
    }

    #[test]
    fn a_per_tab_mode_only_owns_the_keyboard_while_its_tab_is_on_screen() {
        let mut ui = app();
        let first = push_tab(&mut ui, "/tmp/first");
        let _second = push_tab(&mut ui, "/tmp/second");
        ui.active = 0;
        ui.tab_mut(first)
            .expect("the tab is open")
            .push_mode(TabMode::Find(crate::find::FindState::default()));
        assert_eq!(ui.key_context(), Context::Find);

        // The find bar belongs to the tab it was opened on; switching away
        // must not leave it holding keys for a document it isn't showing.
        ui.active = 1;
        assert_eq!(ui.key_context(), Context::Base);
        ui.active = 0;
        assert_eq!(ui.key_context(), Context::Find);
    }

    #[test]
    fn switching_tabs_closes_the_description_editor() {
        let mut ui = app();
        let first = push_tab(&mut ui, "/tmp/first");
        let second = push_tab(&mut ui, "/tmp/second");
        ui.active = 0;
        ui.tab_mut(first)
            .expect("the tab is open")
            .push_mode(TabMode::Description(crate::DescriptionEditor {
                target: diffui_core::RevisionSelection::WorkingCopy,
                original: String::new(),
                content: iced::widget::text_editor::Content::new(),
                saving_activity: None,
                switch_blocked: false,
            }));

        let _ = ui.activate_tab(second);

        assert_eq!(ui.active_tab_id(), Some(second));
        assert!(
            ui.tab_mut(first)
                .expect("the tab is open")
                .description_editor()
                .is_none()
        );
    }
}

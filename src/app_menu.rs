//! The application menus: File / Edit / View / Revision / Help, built from the
//! command registry so every item runs the same command a key or the palette
//! would, and shows the chord the keymap actually binds.
//!
//! On macOS they fill the global menu bar, between the App and Window menus
//! only AppKit has; the AppKit side (and why command items never fire on their
//! own key equivalents) is in [`crate::macos_native`]. Elsewhere there is no
//! global menu bar, so the same tree opens as an overlay menu from the tab
//! strip's ☰ button ([`overlay`]).

use crate::Cx;
use crate::Diffui;
use crate::commands::CommandArg;
use crate::commands::CommandId;

/// One row of an application menu, before it's lowered to an AppKit item or
/// an overlay [`crate::menu::MenuEntry`].
enum Row {
    /// A command under its registry label, or under `title` where a menu
    /// words it differently.
    Command {
        id: &'static str,
        title: Option<&'static str>,
    },
    Separator,
    Submenu {
        title: &'static str,
        rows: Vec<Row>,
    },
    /// Cut / Copy / Paste / Select All. AppKit needs them in the menu to hand
    /// the keys to the focused field; elsewhere the field hears them itself.
    Clipboard,
}

struct Menu {
    title: &'static str,
    rows: Vec<Row>,
}

fn registered(id: &'static str) -> Row {
    Row::Command { id, title: None }
}

fn titled(title: &'static str, id: &'static str) -> Row {
    Row::Command {
        id,
        title: Some(title),
    }
}

/// The menus every platform shows, in order, Help aside: macOS puts its
/// Window menu between them and [`help_menu`].
fn document_menus() -> Vec<Menu> {
    use Row::Separator as SEP;
    vec![
        Menu {
            title: "File",
            rows: vec![
                registered("repo.open-dialog"),
                registered("tab.close"),
                SEP,
                registered("repo.refresh"),
                registered("repo.fetch"),
                registered("repo.push"),
            ],
        },
        Menu {
            title: "Edit",
            rows: vec![
                titled("Undo Last Operation", "repo.undo"),
                titled("Redo Last Undone Operation", "repo.redo"),
                SEP,
                Row::Clipboard,
                SEP,
                titled("Find in Diff\u{2026}", "find.open"),
                titled("Command Palette\u{2026}", "palette.toggle"),
            ],
        },
        Menu {
            title: "View",
            rows: vec![
                registered("view.toggle-wrap"),
                registered("view.toggle-split"),
                SEP,
                registered("view.diff"),
                registered("view.source"),
                registered("activity.toggle"),
                SEP,
                registered("history.op-log"),
                registered("history.evolog"),
                registered("file.history.current"),
                registered("source.toggle-annotate"),
                SEP,
                Row::Submenu {
                    title: "Theme",
                    rows: vec![
                        titled("System", "theme.system"),
                        titled("Light", "theme.light"),
                        titled("Dark", "theme.dark"),
                        titled("High Contrast", "theme.contrast"),
                    ],
                },
            ],
        },
        Menu {
            title: "Revision",
            rows: vec![
                registered("revision.commit"),
                registered("revision.new-child"),
                registered("revision.new.start"),
                registered("revision.edit"),
                registered("revision.edit-description"),
                SEP,
                registered("revision.rebase.start"),
                registered("revision.rebase.descendants.start"),
                registered("revision.rebase.branch.start"),
                registered("revision.rebase.onto-trunk"),
                registered("revision.squash.start"),
                registered("revision.squash-into-parent"),
                registered("revision.merge.start"),
                registered("revision.duplicate"),
                registered("revision.duplicate.start"),
                registered("revision.parallelize.start"),
                registered("revision.absorb"),
                SEP,
                registered("revision.abandon"),
                SEP,
                registered("bookmark.create"),
                registered("revision.push-change"),
                SEP,
                registered("compare.with"),
                registered("revision.browse-source"),
            ],
        },
    ]
}

fn help_menu() -> Menu {
    Menu {
        title: "Help",
        rows: vec![registered("help.shortcuts")],
    }
}

/// The ☰ menu: one flyout per application menu, then Settings, which macOS
/// keeps in its App menu.
pub(crate) fn overlay(ui: &Diffui) -> Vec<crate::menu::MenuEntry> {
    use crate::menu::MenuEntry;
    let mut root: Vec<MenuEntry> = document_menus()
        .into_iter()
        .chain([help_menu()])
        .map(|menu| MenuEntry::Submenu {
            label: menu.title.to_owned(),
            items: overlay_rows(ui, &menu.rows),
        })
        .collect();
    root.push(MenuEntry::Separator);
    root.extend(overlay_rows(
        ui,
        &[titled("Settings\u{2026}", "app.settings")],
    ));
    root
}

fn overlay_rows(ui: &Diffui, rows: &[Row]) -> Vec<crate::menu::MenuEntry> {
    use crate::menu::MenuEntry;
    let mut entries = Vec::new();
    for row in rows {
        match row {
            Row::Command { id, title } => {
                let mut entry = ui.menu_item(id, CommandArg::None);
                if let (
                    Some(title),
                    MenuEntry::Item { label, .. } | MenuEntry::Disabled { label, .. },
                ) = (title, &mut entry)
                {
                    *label = (*title).to_owned();
                }
                entries.push(entry);
            }
            // A row left out (the clipboard's) mustn't leave two in a row.
            Row::Separator => {
                if !matches!(entries.last(), None | Some(MenuEntry::Separator)) {
                    entries.push(MenuEntry::Separator);
                }
            }
            Row::Submenu { title, rows } => entries.push(MenuEntry::Submenu {
                label: (*title).to_owned(),
                items: overlay_rows(ui, rows),
            }),
            Row::Clipboard => {}
        }
    }
    entries
}

impl Diffui {
    /// A menu-bar pick: `tag` names the command through [`tag_of`].
    #[cfg(target_os = "macos")]
    pub(crate) fn on_menu_bar_pick(&mut self, tag: u32, cx: &mut Cx) {
        if let Some(id) = command_of(tag) {
            self.run_command(&id, CommandArg::None, cx)
        }
    }
}

/// Starts the loop that turns menu-bar clicks into commands.
pub(crate) fn start(cx: &mut Cx) {
    #[cfg(target_os = "macos")]
    {
        use futures::StreamExt as _;
        let mut picks = crate::macos_native::menu_bar_picks();
        cx.spawn(async move |this, cx| {
            while let Some(tag) = picks.next().await {
                let updated = this.update(cx, |this, cx| {
                    this.act(cx, |this, cx| this.on_menu_bar_pick(tag, cx));
                });
                if updated.is_err() {
                    return;
                }
            }
        })
        .detach();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

/// Install the menu bar. Called once the window is open. Why this isn't GPUI's
/// `cx.set_menus` is in [`crate::macos_native`].
pub(crate) fn install(ui: &Diffui) {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        let menus = mac::menus(ui);
        mac::record_bar_tags(&menus);
        crate::macos_native::install_menu_bar(&menus);
    }
    #[cfg(any(not(target_os = "macos"), test))]
    let _ = ui;
}

/// Tell the menu bar which of its commands can't run in the state `ui` is in
/// now, so their items are greyed when a menu opens. Cheap enough to run
/// after every update: a predicate per bar command, no repository reads.
#[cfg_attr(test, allow(unused_variables))]
pub(crate) fn refresh_availability(ui: &Diffui) {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        let unavailable = mac::bar_tags()
            .iter()
            .copied()
            .filter(|tag| {
                command_of(*tag).is_none_or(|id| {
                    ui.registry
                        .get(&id)
                        .is_none_or(|command| command.enabled(ui, &CommandArg::None).is_err())
                })
            })
            .collect();
        crate::macos_native::set_unavailable_bar_commands(unavailable);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = ui;
}

/// Every id a menu item's tag has stood for, the tag being the position.
/// Handed out on first use and never reused, so a tag the bar was built with
/// still names its command after the registry is rebuilt.
static TAG_IDS: std::sync::Mutex<Vec<CommandId>> = std::sync::Mutex::new(Vec::new());

/// The tag a menu item for `id` carries.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn tag_of(id: &str) -> u32 {
    let mut tags = TAG_IDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = match tags.iter().position(|tagged| *tagged == id) {
        Some(index) => index,
        None => {
            tags.push(id.into());
            tags.len() - 1
        }
    };
    index as u32
}

/// The command `tag` stands for.
#[cfg(target_os = "macos")]
fn command_of(tag: u32) -> Option<CommandId> {
    let tags = TAG_IDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tags.get(tag as usize).cloned()
}

#[cfg(target_os = "macos")]
// Tests never install the bar, so nothing there asks for its tags.
#[cfg_attr(test, allow(dead_code))]
mod mac {
    use objc2::sel;

    use super::{Menu, Row, document_menus, help_menu, tag_of};
    use crate::Diffui;
    use crate::commands::Context;
    use crate::keymap::{Chord, ChordKey};
    use crate::macos_native::{BarItem, BarMenu, BarRole, KeyEquivalent};

    /// The chord the keymap binds `id` to in the base context, as an AppKit
    /// key equivalent, when AppKit can spell it.
    fn key_for(ui: &Diffui, id: &str) -> Option<KeyEquivalent> {
        let chord = ui.keymap.chord_for(Context::Base, id)?;
        key_equivalent(chord)
    }

    pub(super) fn key_equivalent(chord: &Chord) -> Option<KeyEquivalent> {
        let key = match &chord.key {
            ChordKey::Char(c) => c.clone(),
            ChordKey::Named(named) => match named {
                crate::keys::Named::Enter => "\r".to_owned(),
                crate::keys::Named::Escape => "\u{1b}".to_owned(),
                crate::keys::Named::Tab => "\t".to_owned(),
                crate::keys::Named::Space => " ".to_owned(),
                crate::keys::Named::ArrowUp => "\u{f700}".to_owned(),
                crate::keys::Named::ArrowDown => "\u{f701}".to_owned(),
                crate::keys::Named::ArrowLeft => "\u{f702}".to_owned(),
                crate::keys::Named::ArrowRight => "\u{f703}".to_owned(),
                _ => return None,
            },
        };
        Some(KeyEquivalent {
            key,
            command: chord.mods.command,
            alt: chord.mods.alt,
            control: chord.mods.control,
            shift: chord.mods.shift,
        })
    }

    fn command(ui: &Diffui, title: &str, id: &str) -> BarItem {
        BarItem::Command {
            title: title.to_owned(),
            tag: tag_of(id),
            key: key_for(ui, id),
        }
    }

    /// A command under its registry (palette) label.
    fn registered(ui: &Diffui, id: &str) -> BarItem {
        let title = ui.registry.get(id).map_or_else(
            || id.to_owned(),
            |command| command.palette_label.to_string(),
        );
        command(ui, &title, id)
    }

    fn standard(title: &str, action: objc2::runtime::Sel, key: Option<(&str, bool)>) -> BarItem {
        BarItem::Standard {
            title: title.to_owned(),
            action,
            key: key.map(|(key, alt)| KeyEquivalent {
                key: key.to_owned(),
                command: true,
                alt,
                control: false,
                shift: false,
            }),
        }
    }

    fn edit_key(title: &str, key: &str) -> BarItem {
        BarItem::EditKey {
            title: title.to_owned(),
            key: KeyEquivalent {
                key: key.to_owned(),
                command: true,
                alt: false,
                control: false,
                shift: false,
            },
        }
    }

    static TAGS: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();

    /// Remember the tags of every command `menus` shows, at install.
    pub(super) fn record_bar_tags(menus: &[BarMenu]) {
        let mut tags = Vec::new();
        for menu in menus {
            walk(&menu.items, &mut tags);
        }
        let _ = TAGS.set(tags);
    }

    /// The tags of every command the bar shows; empty before install.
    pub(super) fn bar_tags() -> &'static [u32] {
        TAGS.get().map_or(&[], Vec::as_slice)
    }

    fn walk(items: &[BarItem], out: &mut Vec<u32>) {
        for item in items {
            match item {
                BarItem::Command { tag, .. } => out.push(*tag),
                BarItem::Submenu { items, .. } => walk(items, out),
                _ => {}
            }
        }
    }

    /// The shared [`super::Row`]s as AppKit items.
    fn lower(ui: &Diffui, rows: &[Row]) -> Vec<BarItem> {
        let mut items = Vec::new();
        for row in rows {
            match row {
                Row::Command {
                    id,
                    title: Some(title),
                } => items.push(command(ui, title, id)),
                Row::Command { id, title: None } => items.push(registered(ui, id)),
                Row::Separator => items.push(BarItem::Separator),
                Row::Submenu { title, rows } => items.push(BarItem::Submenu {
                    title: (*title).to_owned(),
                    items: lower(ui, rows),
                }),
                Row::Clipboard => items.extend([
                    edit_key("Cut", "x"),
                    edit_key("Copy", "c"),
                    edit_key("Paste", "v"),
                    edit_key("Select All", "a"),
                ]),
            }
        }
        items
    }

    pub(super) fn menus(ui: &Diffui) -> Vec<BarMenu> {
        let app_name = "Diffui";
        let sep = || BarItem::Separator;
        let bar_menu = |menu: Menu, role: BarRole| BarMenu {
            title: menu.title.to_owned(),
            role,
            items: lower(ui, &menu.rows),
        };
        let mut menus = vec![BarMenu {
            title: app_name.to_owned(),
            role: BarRole::App,
            items: vec![
                standard(
                    &format!("About {app_name}"),
                    sel!(orderFrontStandardAboutPanel:),
                    None,
                ),
                sep(),
                command(ui, "Settings\u{2026}", "app.settings"),
                sep(),
                standard(&format!("Hide {app_name}"), sel!(hide:), Some(("h", false))),
                standard(
                    "Hide Others",
                    sel!(hideOtherApplications:),
                    Some(("h", true)),
                ),
                standard("Show All", sel!(unhideAllApplications:), None),
                sep(),
                standard(
                    &format!("Quit {app_name}"),
                    sel!(terminate:),
                    Some(("q", false)),
                ),
            ],
        }];
        menus.extend(
            document_menus()
                .into_iter()
                .map(|menu| bar_menu(menu, BarRole::Plain)),
        );
        menus.push(BarMenu {
            title: "Window".to_owned(),
            role: BarRole::Windows,
            items: vec![
                standard("Minimize", sel!(performMiniaturize:), Some(("m", false))),
                standard("Zoom", sel!(performZoom:), None),
                sep(),
                standard("Bring All to Front", sel!(arrangeInFront:), None),
            ],
        });
        menus.push(bar_menu(help_menu(), BarRole::Help));
        menus
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Every command the menu bar names is in the registry, so no item is
    /// built with a dead tag.
    #[test]
    fn every_menu_bar_command_is_registered() {
        let ui = crate::tabs::tests::app();
        fn walk(items: &[crate::macos_native::BarItem], out: &mut Vec<u32>) {
            for item in items {
                match item {
                    crate::macos_native::BarItem::Command { tag, .. } => out.push(*tag),
                    crate::macos_native::BarItem::Submenu { items, .. } => walk(items, out),
                    _ => {}
                }
            }
        }
        let mut tags = Vec::new();
        for menu in mac::menus(&ui) {
            walk(&menu.items, &mut tags);
        }
        assert!(tags.len() > 20);
        assert!(tags.iter().all(|tag| {
            super::command_of(*tag).is_some_and(|id| ui.registry.get(&id).is_some())
        }));
    }

    #[test]
    fn chords_become_appkit_key_equivalents() {
        let chord = |raw: &str| crate::keymap::Chord::parse(raw).expect("parses");
        let palette = mac::key_equivalent(&chord("cmd+k")).expect("spelled");
        assert_eq!((palette.key.as_str(), palette.command), ("k", true));
        // A shifted letter is spelled upper-case, which AppKit reads as ⇧.
        assert_eq!(mac::key_equivalent(&chord("S")).expect("spelled").key, "S");
        assert!(mac::key_equivalent(&chord("f5")).is_none());
    }
}

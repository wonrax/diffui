//! The application menu bar (macOS): App / File / Edit / View / Revision /
//! Window / Help, built from the command registry so every item runs the same
//! command a key or the palette would, and shows the chord the keymap
//! actually binds. The AppKit side (and why command items never fire on
//! their own key equivalents) is in [`crate::macos_native`].
//!
//! Elsewhere this is a no-op: there is no global menu bar to fill.

use iced::{Subscription, Task};

use crate::commands::{CommandArg, CommandId, REGISTRY};
use crate::{Diffui, Message};

impl Diffui {
    /// A menu-bar pick: `tag` is the command's position in the registry.
    pub(crate) fn on_menu_bar_pick(&mut self, tag: u32) -> Task<Message> {
        match REGISTRY.get(tag as usize) {
            Some(command) => self.run_command(command.id, CommandArg::None),
            None => Task::none(),
        }
    }
}

/// The menu bar's clicks, as messages.
pub(crate) fn subscription() -> Subscription<Message> {
    #[cfg(target_os = "macos")]
    {
        Subscription::run(crate::macos_native::menu_bar_picks).map(Message::MenuBar)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Subscription::none()
    }
}

/// Install the menu bar. Called once the window is open, after winit has put
/// up its default one.
pub(crate) fn install(ui: &Diffui) {
    #[cfg(target_os = "macos")]
    crate::macos_native::install_menu_bar(&mac::menus(ui));
    #[cfg(not(target_os = "macos"))]
    let _ = ui;
}

/// Open `config.toml` in the user's text editor, creating it (with a pointer
/// to what can go in it) when there isn't one yet. Behind Settings… in the
/// app menu and "Open settings file" in the palette.
pub(crate) fn open_config_file() -> Task<Message> {
    let Some(path) = crate::config::config_path() else {
        return Task::none();
    };
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, CONFIG_TEMPLATE);
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-t")
        .arg(&path)
        .spawn();
    #[cfg(not(target_os = "macos"))]
    let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
    Task::none()
}

const CONFIG_TEMPLATE: &str = "\
# diffui settings. Read at launch; restart diffui after editing.
#
# theme = \"System\"        # System | Light | Dark | Contrast
# ui_font = \"Inter\"
# mono_font = \"JetBrains Mono\"
# code_font_size = 12
#
# [keys.base]
# \"file.next\" = \"n\"      # rebind; \"none\" unbinds. Ids are listed by `?`.
#
# [[revset_presets]]
# name = \"My stack\"
# revset = \"trunk()..@\"
";

/// The registry position of `id`, which is what a menu item's tag carries.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn tag_of(id: CommandId) -> Option<u32> {
    REGISTRY
        .iter()
        .position(|command| command.id == id)
        .map(|index| index as u32)
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::sel;

    use super::tag_of;
    use crate::Diffui;
    use crate::commands::{self, CommandId, Context};
    use crate::keymap::{Chord, ChordKey};
    use crate::macos_native::{BarItem, BarMenu, BarRole, KeyEquivalent};
    use iced::keyboard::key::Named;

    /// The chord the keymap binds `id` to in the base context, as an AppKit
    /// key equivalent, when AppKit can spell it.
    fn key_for(ui: &Diffui, id: CommandId) -> Option<KeyEquivalent> {
        let chord = ui.keymap.chord_for(Context::Base, id)?;
        key_equivalent(chord)
    }

    pub(super) fn key_equivalent(chord: &Chord) -> Option<KeyEquivalent> {
        let key = match &chord.key {
            ChordKey::Char(c) => c.clone(),
            ChordKey::Named(named) => match named {
                Named::Enter => "\r".to_owned(),
                Named::Escape => "\u{1b}".to_owned(),
                Named::Tab => "\t".to_owned(),
                Named::Space => " ".to_owned(),
                Named::ArrowUp => "\u{f700}".to_owned(),
                Named::ArrowDown => "\u{f701}".to_owned(),
                Named::ArrowLeft => "\u{f702}".to_owned(),
                Named::ArrowRight => "\u{f703}".to_owned(),
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

    fn command(ui: &Diffui, title: &str, id: CommandId) -> BarItem {
        BarItem::Command {
            title: title.to_owned(),
            tag: tag_of(id).unwrap_or(u32::MAX),
            key: key_for(ui, id),
        }
    }

    /// A command under its registry (palette) label.
    fn registered(ui: &Diffui, id: CommandId) -> BarItem {
        let title = commands::command(id)
            .map(|command| command.palette_label())
            .unwrap_or(id);
        command(ui, title, id)
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

    pub(super) fn menus(ui: &Diffui) -> Vec<BarMenu> {
        let app_name = "Diffui";
        let sep = || BarItem::Separator;
        vec![
            BarMenu {
                title: app_name.to_owned(),
                role: BarRole::App,
                items: vec![
                    standard(
                        &format!("About {app_name}"),
                        sel!(orderFrontStandardAboutPanel:),
                        None,
                    ),
                    sep(),
                    command(ui, "Settings\u{2026}", "app.open-config"),
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
            },
            BarMenu {
                title: "File".to_owned(),
                role: BarRole::Plain,
                items: vec![
                    registered(ui, "repo.open-dialog"),
                    registered(ui, "tab.close"),
                    sep(),
                    registered(ui, "repo.refresh"),
                    registered(ui, "repo.fetch"),
                    registered(ui, "repo.push"),
                ],
            },
            BarMenu {
                title: "Edit".to_owned(),
                role: BarRole::Plain,
                items: vec![
                    command(ui, "Undo Last Operation", "repo.undo"),
                    command(ui, "Redo Last Undone Operation", "repo.redo"),
                    sep(),
                    edit_key("Cut", "x"),
                    edit_key("Copy", "c"),
                    edit_key("Paste", "v"),
                    edit_key("Select All", "a"),
                    sep(),
                    command(ui, "Find in Diff\u{2026}", "find.open"),
                    command(ui, "Command Palette\u{2026}", "palette.toggle"),
                ],
            },
            BarMenu {
                title: "View".to_owned(),
                role: BarRole::Plain,
                items: vec![
                    registered(ui, "view.toggle-wrap"),
                    registered(ui, "view.toggle-split"),
                    sep(),
                    registered(ui, "view.diff"),
                    registered(ui, "view.source"),
                    registered(ui, "activity.toggle"),
                    sep(),
                    registered(ui, "history.op-log"),
                    registered(ui, "history.evolog"),
                    registered(ui, "file.history.current"),
                    registered(ui, "source.toggle-annotate"),
                    sep(),
                    BarItem::Submenu {
                        title: "Theme".to_owned(),
                        items: vec![
                            command(ui, "System", "theme.system"),
                            command(ui, "Light", "theme.light"),
                            command(ui, "Dark", "theme.dark"),
                            command(ui, "High Contrast", "theme.contrast"),
                        ],
                    },
                ],
            },
            BarMenu {
                title: "Revision".to_owned(),
                role: BarRole::Plain,
                items: vec![
                    registered(ui, "revision.commit"),
                    registered(ui, "revision.new-child"),
                    registered(ui, "revision.new.start"),
                    registered(ui, "revision.edit"),
                    registered(ui, "revision.edit-description"),
                    sep(),
                    registered(ui, "revision.rebase.start"),
                    registered(ui, "revision.rebase.descendants.start"),
                    registered(ui, "revision.rebase.branch.start"),
                    registered(ui, "revision.squash.start"),
                    registered(ui, "revision.squash-into-parent"),
                    registered(ui, "revision.merge.start"),
                    registered(ui, "revision.duplicate"),
                    registered(ui, "revision.duplicate.start"),
                    registered(ui, "revision.parallelize.start"),
                    registered(ui, "revision.absorb"),
                    sep(),
                    registered(ui, "revision.abandon"),
                    sep(),
                    registered(ui, "bookmark.create"),
                    registered(ui, "revision.push-change"),
                    sep(),
                    registered(ui, "compare.with"),
                    registered(ui, "revision.browse-source"),
                ],
            },
            BarMenu {
                title: "Window".to_owned(),
                role: BarRole::Windows,
                items: vec![
                    standard("Minimize", sel!(performMiniaturize:), Some(("m", false))),
                    standard("Zoom", sel!(performZoom:), None),
                    sep(),
                    standard("Bring All to Front", sel!(arrangeInFront:), None),
                ],
            },
            BarMenu {
                title: "Help".to_owned(),
                role: BarRole::Help,
                items: vec![registered(ui, "help.shortcuts")],
            },
        ]
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
        assert!(tags.iter().all(|tag| REGISTRY.get(*tag as usize).is_some()));
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

//! What plugins look like in the app: the dialog that asks whether one may
//! run, the Plugins group of the settings dialog, and the status bar's items.
//! Plugins draw nothing themselves.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Window, div, px};

use super::{Found, PluginAction, State, trust};
use crate::commands::Context;
use crate::config::display_path;
use crate::modes::{Mode, ModeKind};
use crate::overlay_kit;
use crate::prose::Prose;
use crate::theme::{self, text_size};
use crate::typography::{self, Aligned};
use crate::ui::{self, Look, Segment, Segmented, Tip as _};
use crate::{Action, Cx, Diffui, icons};

/// The open permission dialog: which plugin it asks about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionDialog {
    pub(crate) plugin: String,
}

/// The longest a plugin's status-bar item gets before it is cut.
const STATUS_ITEM_MAX: f32 = 220.0;

fn send(
    action: PluginAction,
    cx: &mut Cx,
) -> impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static {
    Diffui::on_action(Action::Plugin(action), cx)
}

/// The dialog that asks whether a plugin may run: who it is, where it lives
/// and each thing it could do, with Allow and Don't allow. Nothing when
/// closed.
///
/// Esc is Don't allow. Allow has no key: the dialog can come up by itself at
/// launch, and a stray Enter must not let a script run.
pub(crate) fn build_permission_dialog(
    ui: &Diffui,
    look: Look,
    _window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    let Some(Mode::PluginPermission(dialog)) = ui.mode(ModeKind::PluginPermission) else {
        return div().into_any_element();
    };
    let Some(found) = ui.plugins.get(&dialog.plugin) else {
        return div().into_any_element();
    };
    let Some(manifest) = found.manifest.as_deref() else {
        return div().into_any_element();
    };
    let Look { theme, .. } = look;
    let record = ui.config_files.plugins.get(&found.id);
    // What it was allowed before, when it now asks for more.
    let before = match trust::verdict(record, &manifest.permissions) {
        trust::Verdict::Ask { before } => before,
        trust::Verdict::Allowed | trust::Verdict::Refused => None,
    };
    let title = match before {
        Some(_) => format!("Allow {} to do more?", found.name),
        None => format!("Allow {} to run?", found.name),
    };
    let mut lede = Prose::from("The plugin ").code(found.id.clone());
    if !found.version.is_empty() {
        lede = lede.text(format!(" {}", found.version));
    }
    let lede = lede
        .text(" in ")
        .path(display_path(&found.dir))
        .text(" could:");

    let mut list = div().flex().flex_col().w_full().gap(px(theme::space::MD));
    for line in trust::lines(manifest, before.as_ref()) {
        let mut words = Prose::from(line.text.clone());
        if let Some(code) = &line.code {
            words = words.text(" ").code(code.clone()).text(line.tail);
        }
        let mut row = div()
            .flex()
            .flex_row()
            .items_start()
            .w_full()
            .gap(px(theme::space::MD))
            .child(
                div()
                    .flex_none()
                    .pt(px(theme::space::XXS))
                    .child(icons::icon(line.glyph, theme::icon::SM, theme.muted_text)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(text_size::BODY))
                    .line_height(px(text_size::BODY * 1.5))
                    .child(words.view(look, theme.text)),
            );
        if line.new {
            row = row.child(
                div()
                    .flex_none()
                    .child(ui::tag("New", theme.accent, ui::TagSize::Small, look.font).view()),
            );
        }
        list = list.child(row);
    }

    let deny = look
        .button("plugin-dont-allow", "Don't allow")
        .on_click(send(PluginAction::Answer(false), cx));
    let allow = look
        .button("plugin-allow", "Allow")
        .primary()
        .on_click(send(PluginAction::Answer(true), cx));
    let card = overlay_kit::Dialog::new(&Prose::from(title), look)
        .lede(&lede)
        .child(list)
        .bound_hint(
            ui,
            Context::PluginPermission,
            &["plugin.dont-allow"],
            "Don't allow",
        )
        .buttons([deny.into_any_element(), allow.into_any_element()])
        .build();
    // A click outside decides nothing: the dialog stays until it is answered.
    ui::modal(
        "plugin-permission-dialog",
        card,
        crate::toolbar::modal_top(),
        |_, _| {},
    )
    .into_any_element()
}

/// How a plugin's state reads in Settings: the word, and the first line of
/// why when it failed.
fn status(found: &Found) -> (&'static str, Option<&str>) {
    match &found.state {
        State::Running(_) => ("Running", None),
        State::Loading => ("Loading", None),
        State::NotAllowed => ("Not allowed", None),
        State::Disabled => ("Off", None),
        State::Failed(why) => ("Failed", Some(why.lines().next().unwrap_or_default())),
    }
}

/// The settings dialog's Plugins group: a row per plugin found, then the
/// folder they live in, and how tall the rows are together.
pub(crate) fn settings_group(ui: &Diffui, look: Look, cx: &mut Cx) -> (Vec<AnyElement>, f32) {
    let mut height = ROW_HEIGHT;
    let mut rows: Vec<AnyElement> = ui
        .plugins
        .found
        .iter()
        .enumerate()
        .map(|(index, found)| {
            height += row_height(found);
            plugin_row(ui, look, index, found, cx)
        })
        .collect();
    rows.push(folder_row(ui, look, cx));
    (rows, height)
}

use crate::settings::{HINT_ROW_HEIGHT as ROW_HEIGHT, ROW_INSET};

/// What a failed plugin's row adds: the line that says why.
const FAILURE_LINE: f32 = 18.0;

fn row_height(found: &Found) -> f32 {
    match found.state {
        State::Failed(_) => ROW_HEIGHT + FAILURE_LINE,
        _ => ROW_HEIGHT,
    }
}

fn row_shell(height: f32, label: impl IntoElement, controls: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(height))
        .px(px(ROW_INSET))
        .gap(px(theme::space::XL))
        .child(label)
        .child(div().flex().flex_none().child(controls))
        .into_any_element()
}

/// One plugin: its name, version and state over its folder (and, when it
/// failed, the first line of why), then what can be done with it and the
/// toggle.
fn plugin_row(ui: &Diffui, look: Look, index: usize, found: &Found, cx: &mut Cx) -> AnyElement {
    let Look {
        theme, font, mono, ..
    } = look;
    let (state, failure) = status(found);
    let state_ink = match found.state {
        State::Running(_) => theme.added_text,
        State::Failed(_) => theme.removed_text,
        State::Loading | State::NotAllowed | State::Disabled => theme.muted_text,
    };
    let mut name = vec![Aligned::text(
        typography::line(found.name.clone(), font, text_size::BODY, theme.text),
        font,
        text_size::BODY,
    )];
    if !found.version.is_empty() {
        name.push(Aligned::text(
            typography::line(
                found.version.clone(),
                font,
                text_size::CAPTION,
                theme.subtle_text,
            ),
            font,
            text_size::CAPTION,
        ));
    }
    name.push(ui::tag(state, state_ink, ui::TagSize::Small, font).aligned());
    // Clipped at the column's edge: a long name must not run under the
    // controls.
    let mut label = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .gap(px(theme::space::XXS))
        .child(typography::baseline_row(name, theme::space::MD))
        .child(
            typography::line(
                display_path(&found.dir),
                mono,
                text_size::CAPTION,
                theme.muted_text,
            )
            .overflow_hidden()
            .text_ellipsis(),
        );
    if let Some(why) = failure {
        // One line, cut to fit; the whole of it is a hover away, and in the
        // log.
        label = label.child(
            div()
                .id(("settings-plugin-failure", index))
                .w_full()
                .min_w_0()
                .tip(why.to_owned(), look)
                .child(
                    typography::line(why.to_owned(), mono, text_size::CAPTION, theme.removed_text)
                        .overflow_hidden()
                        .text_ellipsis(),
                ),
        );
    }

    let face = theme::radius::inner(theme::radius::CARD, theme::space::XXS);
    let side = theme::control::COMPACT - 2.0 * theme::space::XXS;
    let id = &found.id;
    let has_manifest = found.manifest.is_some();
    let has_tab = !ui.tabs.is_empty();
    let button = |name: &'static str, glyph: &'static str, tip: &'static str| {
        look.icon_button((name, index), glyph, theme.muted_text)
            .size(px(side))
            .corner_radius(face)
            .tip(tip)
    };
    let actions = ui::segmented(theme)
        .child(
            button(
                "settings-plugin-reveal",
                icons::FOLDER_OPEN,
                "Reveal folder",
            )
            .on_click(send(PluginAction::Reveal(Some(id.clone())), cx)),
        )
        .child(
            button(
                "settings-plugin-log",
                icons::SCROLL_TEXT,
                if has_tab {
                    "Show log"
                } else {
                    "Open a repository to show the log"
                },
            )
            .disabled(!has_tab)
            .when(has_tab, |button| {
                button.on_click(send(PluginAction::ShowLog(id.clone()), cx))
            }),
        )
        .child(
            button(
                "settings-plugin-review",
                overlay_kit::glyph::LOCK,
                "Review permissions",
            )
            .disabled(!has_manifest)
            .when(has_manifest, |button| {
                button.on_click(send(PluginAction::Review(id.clone()), cx))
            }),
        );
    let on = found.state != State::Disabled;
    let toggle =
        ui::segmented(theme).children([(true, "On"), (false, "Off")].map(|(value, label)| {
            ui::segment(
                (
                    if value {
                        "settings-plugin-on"
                    } else {
                        "settings-plugin-off"
                    },
                    index,
                ),
                Segment {
                    icon: None,
                    label: label.into(),
                    count: None,
                    active: on == value,
                    enabled: has_manifest,
                },
                Segmented::Compact,
                look,
            )
            .on_click(send(PluginAction::Enable(id.clone(), value), cx))
        }));
    row_shell(
        row_height(found),
        label,
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(theme::space::MD))
            .child(actions)
            .child(toggle),
    )
}

/// Where plugins live, and a button that shows the folder.
fn folder_row(ui: &Diffui, look: Look, cx: &mut Cx) -> AnyElement {
    let Look {
        theme, font, mono, ..
    } = look;
    let path = ui
        .plugins
        .dir
        .as_deref()
        .map(display_path)
        .unwrap_or_default();
    let hint = if ui.plugins.found.is_empty() {
        "No plugins yet. Each one is a folder here with a plugin.toml."
    } else {
        "Each plugin is a folder here with a plugin.toml."
    };
    let label = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(px(theme::space::XXS))
        .child(typography::line(
            "Plugins folder",
            font,
            text_size::BODY,
            theme.text,
        ))
        .child(
            typography::line(hint, font, text_size::CAPTION, theme.subtle_text)
                .overflow_hidden()
                .text_ellipsis(),
        );
    let reveal = look
        .bare_button("settings-plugins-folder")
        .raised()
        .disabled(path.is_empty())
        .tip(REVEAL_TIP)
        .child(typography::centered(
            typography::line(path, mono, text_size::BODY, theme.text),
            mono,
            text_size::BODY,
        ))
        .on_click(send(PluginAction::Reveal(None), cx));
    row_shell(ROW_HEIGHT, label, reveal)
}

const REVEAL_TIP: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else {
    "Show in folder"
};

/// The status bar's right-hand items: what each plugin set with
/// `ui.status.set`, in plugin order, each cut to a width.
pub(crate) fn status_items(ui: &Diffui, look: Look) -> Vec<Aligned> {
    let Look { theme, font, .. } = look;
    let size = theme::text_size::CAPTION;
    ui.plugins
        .status
        .values()
        .map(|text| {
            let line = text.lines().next().unwrap_or_default();
            let cut = crate::ellipsis::cut_end(line, STATUS_ITEM_MAX, size, font).into_owned();
            Aligned::text(
                typography::line(cut, font, size, theme.muted_text),
                font,
                size,
            )
        })
        .collect()
}

impl Diffui {
    /// Show what the plugin `id` logged, in the output panel.
    pub(super) fn show_plugin_log(&mut self, id: &str, _cx: &mut Cx) {
        let Some(found) = self.plugins.get(id) else {
            return;
        };
        let title = format!("{} log", found.name);
        let mut lines: Vec<diffui_core::cli::CliLine> = Vec::new();
        if let State::Failed(why) = &found.state {
            lines.extend(why.lines().map(|line| diffui_core::cli::CliLine {
                stream: diffui_core::cli::CliStream::Stderr,
                text: line.to_owned(),
            }));
        }
        for problem in &found.problems {
            lines.push(diffui_core::cli::CliLine {
                stream: diffui_core::cli::CliStream::Stderr,
                text: problem.clone(),
            });
        }
        for entry in found.log() {
            use diffui_plugins::LogLevel;
            let stream = match entry.level {
                LogLevel::Debug | LogLevel::Info => diffui_core::cli::CliStream::Stdout,
                LogLevel::Warn | LogLevel::Error => diffui_core::cli::CliStream::Stderr,
            };
            lines.extend(entry.message.lines().map(|line| diffui_core::cli::CliLine {
                stream,
                text: line.to_owned(),
            }));
        }
        self.pop_mode(ModeKind::Settings);
        self.show_transcript(title, lines);
    }
}

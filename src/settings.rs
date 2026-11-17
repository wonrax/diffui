//! The settings dialog (⌘, or the palette's "Settings…"), and saving a
//! setting.
//!
//! What the dialog changes applies at once and goes to `settings.toml`. A key
//! `config.toml` sets is shown but locked there; the palette's theme and
//! refresh commands still change it for the session, and say so.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, MouseButton, MouseDownEvent, Window, div, px};

use std::path::Path;

use diffui_core::RefreshMode;

use crate::commands::CommandArg;
use crate::config::{
    CODE_BASELINE_OFFSET, CODE_FONT_SIZE, CODE_LINE_HEIGHT, ConfigFile, Key, Setting,
};
use crate::geometry::Rectangle;
use crate::modes::{Mode, ModeKind};
use crate::overlay_kit;
use crate::prose::Prose;
use crate::theme::{self, Font, ThemePreference, text_size};
use crate::typography;
use crate::ui::{self, Look, Segment, Segmented, Tip as _};
use crate::{Action, Cx, Diffui, icons, menu};

/// The dialog: a sidebar of sections and the pane of the one selected. As
/// large as the window allows, up to this.
const CARD_WIDTH: f32 = 980.0;
const CARD_HEIGHT: f32 = 760.0;
const SIDEBAR_WIDTH: f32 = 212.0;
const SECTION_ROW: f32 = 32.0;
/// The pane's inset, and the room above the sidebar's title.
const PANE_PAD: f32 = 32.0;
/// The pane's title: the section, or the page open in it.
const PAGE_TITLE: f32 = 20.0;
/// Between a group's heading and its card, on top of the heading's own.
pub(crate) const HEADING_GAP: f32 = theme::space::SM;
const ROW_HEIGHT: f32 = 44.0;
/// A row with a line of explanation under its name, wrapped to two lines.
pub(crate) const HINT_ROW_HEIGHT: f32 = 64.0;
/// A row's inset inside its group's card, and its divider's.
pub(crate) const ROW_INSET: f32 = theme::space::LG;
/// Between one group's card and the next group's heading.
const GROUP_GAP: f32 = theme::space::XXL;
/// A pop-up button's label is cut with an ellipsis past this.
const SELECT_TEXT_MAX: f32 = 180.0;
const REVEAL_TIP: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else {
    "Show in folder"
};
/// The double-click window the dialog steps through, ms. Loading doesn't
/// clamp it; the dialog keeps to what a hand can tell apart.
const MULTI_CLICK_MS: std::ops::RangeInclusive<f32> = 150.0..=1000.0;

impl Diffui {
    /// Show `section`, leaving whatever page was open in the pane.
    pub(crate) fn show_settings_section(&mut self, section: Section) {
        self.close_host_page();
        self.plugins.page = None;
        self.settings_section = section;
    }

    pub(crate) fn open_settings(&mut self) {
        self.refresh_ai_keys();
        // It replaces the transient overlays rather than stacking over them:
        // they share one keyboard.
        self.pop_mode(ModeKind::Palette);
        self.pop_mode(ModeKind::Menu);
        self.pop_mode(ModeKind::ActivityPopover);
        self.pop_mode(ModeKind::Shortcuts);
        self.push_mode(Mode::Settings);
    }

    /// Apply `setting` now, then save it. Theme and refresh have actions of
    /// their own ([`Action::SetTheme`], [`Action::SetRefreshMode`]).
    pub(crate) fn change_setting(&mut self, setting: Setting) {
        setting.apply(&mut self.config);
        match setting.key() {
            Key::UiFont | Key::MonoFont => {
                crate::typography::set_ui_font(self.config.ui_font);
                // Its ids and chips are measured in these fonts.
                self.sidebar_min_width = crate::sidebar::min_width(self.config);
                if self.sidebar_width < self.sidebar_min_width {
                    self.sidebar_width = self.sidebar_min_width;
                    self.mark_geometry_dirty();
                }
                // The diff's shaped lines are cached per document.
                self.document_version = self.document_version.wrapping_add(1);
            }
            Key::CodeFontSize | Key::CodeLineHeight | Key::CodeBaselineOffset => {
                self.document_version = self.document_version.wrapping_add(1);
            }
            Key::Theme | Key::Refresh | Key::MultiClickMs => {}
        }
        self.persist_setting(setting);
    }

    /// Save `setting`, already applied, for the next launch.
    pub(crate) fn persist_setting(&mut self, setting: Setting) {
        let key = setting.key();
        if self.config.locked.contains(key) {
            self.push_error_toast(
                Prose::from(format!("{} is set in ", key.label())).path("config.toml"),
                "Changed for this session only",
            );
            return;
        }
        if let Err(error) = self.config_files.write(&setting) {
            self.push_error_toast("Couldn't save settings", &error);
        }
    }

    /// The installed families to pick a font from, under the button at
    /// `anchor`.
    fn open_font_menu(&mut self, key: Key, anchor: Rectangle, cx: &mut Cx) {
        let (command, current, default) = match key {
            Key::MonoFont => ("settings.mono-font", self.config.mono_font, Font::MONOSPACE),
            _ => ("settings.ui-font", self.config.ui_font, Font::DEFAULT),
        };
        let item = |label: String, family: &str| menu::MenuEntry::Item {
            label,
            detail: None,
            emphasized: current.family == family,
            command: command.into(),
            arg: CommandArg::Text(family.to_owned()),
            icon: None,
        };
        let mut tree = vec![
            item(font_label(default), default.family),
            menu::MenuEntry::Separator,
        ];
        tree.extend(
            crate::config::font_families()
                .into_iter()
                .filter(|family| family != default.family)
                .map(|family| item(family.clone(), &family)),
        );
        #[cfg(target_os = "macos")]
        {
            let _ = anchor;
            Self::popup_tree(&tree, None, cx);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cx;
            self.push_mode(Mode::Menu(menu::OverlayMenu::new(
                tree,
                menu::AnchorSpec::Below(anchor),
                Some(MouseButton::Left),
            )));
        }
    }

    /// Show `file` in the file manager. Creates nothing: a file that
    /// isn't there isn't shown.
    pub(crate) fn reveal_config_file(&self, file: ConfigFile) {
        if let Some(path) = self.config_files.path(file).filter(|path| path.exists()) {
            reveal_path(path);
        }
    }
}

/// Show `path` in the file manager, selected in its folder where the
/// platform can.
pub(crate) fn reveal_path(path: &Path) {
    let mut command = if cfg!(target_os = "macos") {
        let mut open = std::process::Command::new("open");
        open.arg("-R").arg(path);
        open
    } else {
        let mut open = std::process::Command::new(if cfg!(windows) {
            "explorer"
        } else {
            "xdg-open"
        });
        open.arg(path.parent().unwrap_or(path));
        open
    };
    if let Err(error) = command.spawn() {
        tracing::warn!(%error, path = %path.display(), "failed to reveal a path");
    }
}

/// How a font setting reads: the platform's UI font by what it is, any
/// other by its family.
fn font_label(font: Font) -> String {
    if font == Font::DEFAULT {
        "System font".to_owned()
    } else if font == Font::MONOSPACE {
        format!("System monospace ({})", font.resolved_family())
    } else {
        font.family.to_owned()
    }
}

/// `value` moved `steps` grid steps of `step` from where it is, snapped to the
/// grid and kept in `range`.
fn stepped(value: f32, step: f32, steps: f32, range: &std::ops::RangeInclusive<f32>) -> f32 {
    let snapped = ((value / step).round() + steps) * step;
    // The grid's own float error (0.05 × 37 = 1.8500001) shouldn't be saved.
    ((snapped * 100.0).round() / 100.0).clamp(*range.start(), *range.end())
}

/// The dialog over a scrim, or nothing when it is closed.
pub(crate) fn build_overlay(
    ui: &Diffui,
    look: Look,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    if ui.mode(ModeKind::Settings).is_none() {
        return div().into_any_element();
    }
    let Look { theme, font, .. } = look;
    // A page opened from a section (a host's, a plugin's) takes the pane.
    let pane = match crate::host_page::page_pane(ui, look, window, cx) {
        Some(pane) => pane,
        None => match crate::plugins::page_pane(ui, look, window, cx) {
            Some(pane) => pane,
            None => section_pane(ui, look, window, cx),
        },
    };

    let title_font = theme::emphasis_font(font, gpui_kit::FontWeight::SEMIBOLD);
    // The sidebar is a panel of its own down the card's left side: the
    // dialog's name, its sections, and at the bottom the two files.
    let corner = theme::radius::inner(theme::radius::MODAL, theme::HAIRLINE);
    let sections = div()
        .flex()
        .flex_col()
        .w_full()
        .px(px(theme::space::MD))
        .gap(px(theme::space::XXS))
        .children(Section::ALL.into_iter().map(|section| {
            let selected = ui.settings_section == section;
            let ink = if selected {
                theme.text
            } else {
                theme.muted_text
            };
            ui::list_row(
                ui::ListRow::new(SECTION_ROW, theme::space::MD),
                selected,
                theme,
            )
            .id(("settings-section", section as usize))
            .gap(px(theme::space::MD))
            .child(icons::icon(section.icon(), ui::ICON_SIZE, ink))
            .child(typography::line(
                section.label(),
                font,
                text_size::BODY,
                ink,
            ))
            .on_click(Diffui::on_action(Action::SettingsSection(section), cx))
        }));
    let files = div()
        .flex()
        .flex_col()
        .items_start()
        .w_full()
        .px(px(theme::space::LG))
        .pb(px(theme::space::LG))
        .gap(px(theme::space::SM))
        .child(file_button(ui, look, ConfigFile::Settings, cx))
        .child(file_button(ui, look, ConfigFile::Config, cx));
    let sidebar = div()
        .relative()
        .flex()
        .flex_col()
        .flex_none()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .overflow_hidden()
        .border_r_1()
        .border_color(theme.border)
        .child(crate::squircle::background(
            crate::squircle::Fill::new(crate::squircle::Corners {
                top_left: corner,
                top_right: 0.0,
                bottom_right: 0.0,
                bottom_left: corner,
            })
            .background(overlay_kit::inset_fill(theme)),
            0.0,
        ))
        .child(
            div()
                .px(px(theme::space::XL))
                .pt(px(PANE_PAD))
                .pb(px(theme::space::LG))
                .child(typography::line(
                    "Settings",
                    title_font,
                    text_size::TITLE,
                    theme.text,
                )),
        )
        .child(sections)
        .child(div().flex_1())
        .child(files);

    // The pane: what it is in large type (a section, or a page opened from
    // one with the way back), then its rows.
    let (title, back) = match pane.page {
        Some((title, back)) => (title, Some(back)),
        None => (Prose::from(ui.settings_section.label()), None),
    };
    let close = look
        .icon_button("settings-close", icons::CLOSE, theme.muted_text)
        .tip("Close")
        .on_click(Diffui::on_action(Action::CloseSettings, cx));
    let header = div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .w_full()
        .gap(px(theme::space::MD))
        .pl(px(PANE_PAD))
        .pr(px(theme::space::XL))
        .pt(px(theme::space::XL))
        .pb(px(theme::space::LG))
        .children(back.map(|back| {
            look.icon_button("settings-page-back", icons::ARROW_LEFT, theme.muted_text)
                .tip("Back")
                .on_click(Diffui::on_action(back, cx))
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(PAGE_TITLE))
                .child(title.view(
                    Look {
                        font: title_font,
                        ..look
                    },
                    theme.text,
                )),
        )
        .children(pane.actions)
        .child(close);
    // The pane scrolls out to the card's right edge, so the bar sits where
    // every other scroll view's does; the inset is inside its body.
    let content = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
        .child(header)
        .child(
            div()
                .flex_1()
                .min_h_0()
                .child(ui::scrollable(pane.scroll, pane.body)),
        );
    // A card this tall sits in the middle of the window: hung from the top
    // like the short dialogs, it would leave all its spare room below.
    let room = ui.window_size.height - 4.0 * theme::space::XXL;
    let height = room.clamp(360.0, CARD_HEIGHT);
    let top = ((ui.window_size.height - height) / 2.0).max(theme::space::XXL);
    let width = (ui.window_size.width - 4.0 * theme::space::XXL).clamp(640.0, CARD_WIDTH);
    let card = theme::surface(div(), theme::Elevation::Modal, theme)
        .w(px(width))
        .h(px(height))
        .flex()
        .flex_row()
        .child(sidebar)
        .child(content);
    let dismiss = Diffui::on_action::<()>(Action::CloseSettings, cx);
    ui::modal("settings-dialog", card, top, move |window, cx| {
        dismiss(&(), window, cx)
    })
    .into_any_element()
}

/// What the dialog's right side shows: a section's groups, or a page
/// opened from one.
pub(crate) struct Pane {
    /// A page's name and the action that leaves it. `None` for a section.
    pub(crate) page: Option<(Prose, Action)>,
    /// Buttons at the end of a page's header.
    pub(crate) actions: Vec<AnyElement>,
    /// The scroll view's id: each pane keeps its own place.
    pub(crate) scroll: &'static str,
    pub(crate) body: gpui_kit::Div,
}

/// A pane's body: a column out to the scroll bar's inset.
pub(crate) fn pane_body() -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .w_full()
        .px(px(PANE_PAD))
        .pb(px(PANE_PAD))
}

/// A sentence at the top of a section, saying what its rows are.
fn lede(look: Look, text: &'static str) -> impl IntoElement {
    let Look { theme, font, .. } = look;
    div()
        .w_full()
        .pb(px(theme::space::LG))
        .font(font.gpui())
        .text_size(px(text_size::BODY))
        .line_height(px(text_size::BODY * 1.5))
        .text_color(theme.muted_text)
        .child(text)
}

/// The pane of the section the sidebar has selected.
fn section_pane(ui: &Diffui, look: Look, window: &mut Window, cx: &mut Cx) -> Pane {
    let mut body = pane_body();
    for file in [ConfigFile::Config, ConfigFile::Settings] {
        if let Some(error) = ui.config_files.error(file) {
            body = body.child(
                div().pb(px(theme::space::LG)).child(overlay_kit::callout(
                    &Prose::new()
                        .path(file.name())
                        .text(" has an error and isn't used"),
                    Some(&Prose::new().path(error)),
                    look,
                )),
            );
        }
    }
    let section = ui.settings_section;
    let body = match section {
        Section::General => general(ui, look, body, cx),
        Section::Hosts => body
            .child(lede(
                look,
                "Machines you open repositories on. Each runs diffui-serve, which diffui never \
                 installs for you.",
            ))
            .child(card(
                look,
                crate::host_page::settings_group(ui, look, window, cx),
            )),
        Section::Accounts => body
            .child(lede(
                look,
                "Who you are on each forge, for pull requests. A host can have more than one \
                 account: a repository uses whichever of them can see it.",
            ))
            .child(card(look, crate::plugins::accounts_group(ui, look, cx))),
        Section::Ai => body
            .child(lede(
                look,
                "What a command that asks an agent sends its prompt to.",
            ))
            .child(card(look, crate::ai::settings_group(ui, look, cx))),
        Section::Plugins => body
            .child(lede(
                look,
                "Scripts that add forges, AI providers, transports and commands.",
            ))
            .child(card(look, crate::plugins::settings_group(ui, look, cx))),
    };
    Pane {
        page: None,
        actions: Vec::new(),
        scroll: section.scroll_id(),
        body,
    }
}

/// The General section: how the app looks, how code is set, how it behaves.
fn general(ui: &Diffui, look: Look, body: gpui_kit::Div, cx: &mut Cx) -> gpui_kit::Div {
    let Look { theme, .. } = look;
    let config = ui.config;
    let locked = |key: Key| config.locked.contains(key);

    let themes = [
        (ThemePreference::System, "System"),
        (ThemePreference::Light, "Light"),
        (ThemePreference::Dark, "Dark"),
        (ThemePreference::HighContrast, "High contrast"),
    ];
    let theme_control = ui::segmented(theme).children(themes.into_iter().map(|(choice, label)| {
        ui::segment(
            ("settings-theme", choice as usize),
            Segment {
                icon: None,
                label: label.into(),
                count: None,
                active: ui.selected_theme == choice,
                enabled: !locked(Key::Theme),
            },
            Segmented::Compact,
            look,
        )
        .on_click(Diffui::on_action(Action::SetTheme(choice), cx))
    }));
    let appearance = vec![
        row(look, "Theme", locked(Key::Theme), theme_control),
        font_row(ui, look, "UI font", Key::UiFont, config.ui_font, cx),
        font_row(ui, look, "Code font", Key::MonoFont, config.mono_font, cx),
    ];

    let code = config.code_type;
    let number_row = |name, hint, number: Numeric, cx: &mut Cx| {
        let key = number.key;
        hinted_row(
            look,
            name,
            hint,
            locked(key),
            stepper(look, number, locked(key), cx),
        )
    };
    let code_rows = vec![
        number_row(
            "Size",
            None,
            Numeric {
                key: Key::CodeFontSize,
                shown: format!("{} px", code.size),
                value: code.size,
                step: 1.0,
                range: CODE_FONT_SIZE,
                setting: Setting::CodeFontSize,
            },
            cx,
        ),
        number_row(
            "Line height",
            None,
            Numeric {
                key: Key::CodeLineHeight,
                shown: format!("{:.2}", code.line_height),
                value: code.line_height,
                step: 0.05,
                range: CODE_LINE_HEIGHT,
                setting: Setting::CodeLineHeight,
            },
            cx,
        ),
        number_row(
            "Baseline offset",
            Some("Moves code text up or down within its line, for fonts that sit too high or low."),
            Numeric {
                key: Key::CodeBaselineOffset,
                shown: match code.baseline_offset {
                    offset if offset > 0.0 => format!("+{offset} px"),
                    offset => format!("{offset} px"),
                },
                value: code.baseline_offset,
                step: 0.5,
                range: CODE_BASELINE_OFFSET,
                setting: Setting::CodeBaselineOffset,
            },
            cx,
        ),
    ];

    let modes = [
        (
            RefreshMode::Auto,
            "Automatic",
            "Picks up edits and stale checkouts by itself",
        ),
        (
            RefreshMode::Manual,
            "Manual",
            "Waits for you to press Refresh",
        ),
    ];
    let refresh_control =
        ui::segmented(theme).children(modes.into_iter().map(|(mode, label, tip)| {
            ui::segment(
                ("settings-refresh", mode as usize),
                Segment {
                    icon: None,
                    label: label.into(),
                    count: None,
                    active: config.refresh == mode,
                    enabled: !locked(Key::Refresh),
                },
                Segmented::Compact,
                look,
            )
            .tip(tip)
            .on_click(Diffui::on_action(Action::SetRefreshMode(mode), cx))
        }));
    let behaviour = vec![
        hinted_row(
            look,
            "Refresh",
            Some(
                "Automatic snapshots your edits and updates a stale working copy as it happens. \
                 Manual leaves both to the Refresh button.",
            ),
            locked(Key::Refresh),
            refresh_control,
        ),
        number_row(
            "Double-click speed",
            None,
            Numeric {
                key: Key::MultiClickMs,
                shown: format!("{} ms", config.multi_click_ms),
                value: config.multi_click_ms as f32,
                step: 50.0,
                range: MULTI_CLICK_MS,
                setting: |ms| Setting::MultiClickMs(ms as u64),
            },
            cx,
        ),
    ];

    body.child(group(look, "Appearance", false, appearance))
        .child(group(look, "Code", true, code_rows))
        .child(group(look, "Behaviour", true, behaviour))
}

/// A section of the dialog, by its row in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Section {
    #[default]
    General,
    Accounts,
    Hosts,
    Ai,
    Plugins,
}

impl Section {
    const ALL: [Section; 5] = [
        Section::General,
        Section::Accounts,
        Section::Hosts,
        Section::Ai,
        Section::Plugins,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Accounts => "Accounts",
            Section::Hosts => "Hosts",
            Section::Ai => "AI",
            Section::Plugins => "Plugins",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Section::General => icons::SETTINGS,
            Section::Accounts => icons::CIRCLE_USER,
            Section::Hosts => icons::SERVER,
            Section::Ai => icons::SPARKLES,
            Section::Plugins => icons::PUZZLE,
        }
    }

    fn scroll_id(self) -> &'static str {
        match self {
            Section::General => "settings-general",
            Section::Accounts => "settings-accounts",
            Section::Hosts => "settings-hosts",
            Section::Ai => "settings-ai",
            Section::Plugins => "settings-plugins",
        }
    }
}

/// Rows split by inset hairlines in one card, with no heading: for a
/// section that is one list.
fn card(look: Look, rows: Vec<AnyElement>) -> impl IntoElement {
    let Look { theme, .. } = look;
    let count = rows.len();
    let mut card = div()
        .flex()
        .flex_col()
        .w_full()
        .border_1()
        .border_color(theme::TRANSPARENT);
    for (index, row) in rows.into_iter().enumerate() {
        card = card.child(row);
        if index + 1 < count {
            card = card.child(
                div()
                    .mx(px(ROW_INSET))
                    .h(px(theme::HAIRLINE))
                    .bg(theme.border),
            );
        }
    }
    theme::squircle(card, theme::radius::CARD, None, Some(theme.border))
}

/// A titled card of rows split by inset hairlines, set off from the group
/// above it.
fn group(look: Look, title: &'static str, after: bool, rows: Vec<AnyElement>) -> impl IntoElement {
    let Look { theme, .. } = look;
    let count = rows.len();
    let mut card = div()
        .flex()
        .flex_col()
        .w_full()
        .border_1()
        .border_color(theme::TRANSPARENT);
    for (index, row) in rows.into_iter().enumerate() {
        card = card.child(row);
        if index + 1 < count {
            card = card.child(
                div()
                    .mx(px(ROW_INSET))
                    .h(px(theme::HAIRLINE))
                    .bg(theme.border),
            );
        }
    }
    let card = theme::squircle(card, theme::radius::CARD, None, Some(theme.border));
    div()
        .flex()
        .flex_col()
        .w_full()
        .when(after, |group| group.pt(px(GROUP_GAP)))
        .child(
            div()
                .pb(px(HEADING_GAP))
                .child(overlay_kit::group_heading(title, look)),
        )
        .child(card)
}

/// One setting: its name, a lock when `config.toml` sets it, and its control
/// at the end.
fn row(look: Look, name: &'static str, locked: bool, control: impl IntoElement) -> AnyElement {
    hinted_row(look, name, None, locked, control)
}

/// [`row`], with `hint` wrapped under the name when the name alone doesn't
/// say what the setting does.
pub(crate) fn hinted_row(
    look: Look,
    name: &'static str,
    hint: Option<&'static str>,
    locked: bool,
    control: impl IntoElement,
) -> AnyElement {
    let Look { theme, font, .. } = look;
    let mut label = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .min_w_0()
        .gap(px(theme::space::SM))
        .child(
            typography::line(
                name,
                theme::emphasis_font(font, gpui_kit::FontWeight::MEDIUM),
                text_size::BODY,
                theme.text,
            )
            .text_ellipsis(),
        );
    if locked {
        label = label.child(
            div()
                .id(gpui_kit::ElementId::Name(
                    format!("settings-locked-{name}").into(),
                ))
                .flex_none()
                .tip("Set in config.toml", look)
                .child(icons::icon(
                    overlay_kit::glyph::LOCK,
                    theme::icon::SM,
                    theme.subtle_text,
                )),
        );
    }
    let label = match hint {
        Some(hint) => div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap(px(theme::space::XXS))
            .child(label)
            .child(
                div()
                    .font(font.gpui())
                    .text_size(px(text_size::CAPTION))
                    .text_color(theme.subtle_text)
                    .child(hint),
            ),
        None => label,
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(if hint.is_some() {
            HINT_ROW_HEIGHT
        } else {
            ROW_HEIGHT
        }))
        .px(px(ROW_INSET))
        .gap(px(theme::space::XL))
        .child(label)
        .child(div().flex().flex_none().child(control))
        .into_any_element()
}

/// A font setting: a pop-up button naming the family, as wide as the name
/// up to a cap, which lists the installed ones.
fn font_row(
    ui: &Diffui,
    look: Look,
    name: &'static str,
    key: Key,
    current: Font,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, font, .. } = look;
    let locked = ui.config.locked.contains(key);
    let (anchor, tracker) = menu::anchor_tracker();
    // Opens on the press, as a native pop-up button does.
    let open = Diffui::listen(cx, move |this, _: &MouseDownEvent, _, cx| {
        this.open_font_menu(key, anchor.get(), cx);
        cx.stop_propagation();
    });
    let ink = if locked {
        theme.subtle_text
    } else {
        theme.text
    };
    let face = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::SM))
        .child(
            typography::line(font_label(current), font, text_size::BODY, ink)
                .max_w(px(SELECT_TEXT_MAX))
                .overflow_hidden()
                .text_ellipsis(),
        )
        .child(icons::icon(
            icons::CHEVRON_DOWN,
            theme::icon::SM,
            theme.muted_text,
        ));
    let button = look
        .bare_button(("settings-font", key as usize))
        .raised()
        .disabled(locked)
        .child(face);
    let button = div()
        .relative()
        .child(tracker)
        .when(!locked, |wrap| wrap.on_mouse_down(MouseButton::Left, open))
        .child(button);
    row(look, name, locked, button)
}

/// `settings.toml` or `config.toml`, in mono: the name reveals the file,
/// the glyph beside it copies its path. A file that isn't there can't be
/// revealed.
fn file_button(ui: &Diffui, look: Look, file: ConfigFile, cx: &mut Cx) -> AnyElement {
    let Look { theme, mono, .. } = look;
    let path = ui.config_files.path(file).map(Path::to_path_buf);
    let exists = path.as_deref().is_some_and(Path::exists);
    let face = theme::radius::inner(theme::radius::CARD, theme::space::XXS);
    let side = theme::control::COMPACT - 2.0 * theme::space::XXS;
    let reveal = look
        .bare_button(("settings-reveal", file as usize))
        .h(px(side))
        .px(px(theme::space::MD))
        .corner_radius(face)
        .disabled(!exists)
        .tip(if exists { REVEAL_TIP } else { "Doesn't exist" })
        .child(typography::centered(
            typography::line(
                file.name(),
                mono,
                text_size::BODY,
                if exists {
                    theme.text
                } else {
                    theme.subtle_text
                },
            ),
            mono,
            text_size::BODY,
        ))
        .when(exists, |button| {
            button.on_click(Diffui::on_action(Action::RevealConfigFile(file), cx))
        });
    let copy = look
        .icon_button(
            ("settings-copy", file as usize),
            icons::COPY,
            theme.muted_text,
        )
        .size(px(side))
        .corner_radius(face)
        .tip("Copy path")
        .disabled(path.is_none())
        .when_some(path, |button, path| {
            button.on_click(Diffui::on_action(
                Action::Copy(path.to_string_lossy().into_owned()),
                cx,
            ))
        });
    ui::segmented(theme)
        .child(reveal)
        .child(copy)
        .into_any_element()
}

/// A number setting the dialog steps through.
struct Numeric {
    key: Key,
    /// The value as the row shows it, with its unit.
    shown: String,
    value: f32,
    step: f32,
    range: std::ops::RangeInclusive<f32>,
    setting: fn(f32) -> Setting,
}

impl Numeric {
    /// The change `steps` steps away, or `None` where the range ends.
    fn at(&self, steps: f32) -> Option<Action> {
        let next = stepped(self.value, self.step, steps, &self.range);
        (next != self.value).then(|| Action::ChangeSetting((self.setting)(next)))
    }
}

/// `−  value  +`: the number in a well, stepped by the buttons either side,
/// each disabled where the range ends.
fn stepper(look: Look, number: Numeric, locked: bool, cx: &mut Cx) -> impl IntoElement {
    let Look { theme, font, .. } = look;
    let key = number.key;
    let face = theme::radius::inner(theme::radius::CARD, theme::space::XXS);
    let side = theme::control::COMPACT - 2.0 * theme::space::XXS;
    let mut button = |id: &'static str, glyph: &'static str, action: Option<Action>| {
        let action = action.filter(|_| !locked);
        let button = look
            .icon_button((id, key as usize), glyph, theme.muted_text)
            .size(px(side))
            .corner_radius(face)
            .disabled(action.is_none());
        match action {
            Some(action) => button.on_click(Diffui::on_action(action, cx)),
            None => button,
        }
    };
    let down = button("settings-step-down", icons::MINUS, number.at(-1.0));
    let up = button("settings-step-up", icons::PLUS, number.at(1.0));
    let ink = if locked {
        theme.subtle_text
    } else {
        theme.text
    };
    ui::segmented(theme)
        .child(down)
        .child(
            div()
                .w(px(72.0))
                .flex()
                .justify_center()
                .child(typography::centered(
                    typography::line(number.shown, font, text_size::BODY, ink),
                    font,
                    text_size::BODY,
                )),
        )
        .child(up)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_snaps_to_the_grid_and_stays_in_range() {
        assert_eq!(stepped(1.85, 0.05, 1.0, &CODE_LINE_HEIGHT), 1.9);
        assert_eq!(stepped(1.87, 0.05, -1.0, &CODE_LINE_HEIGHT), 1.8);
        assert_eq!(stepped(3.0, 0.05, 1.0, &CODE_LINE_HEIGHT), 3.0);
        assert_eq!(stepped(12.0, 1.0, -1.0, &CODE_FONT_SIZE), 11.0);
        assert_eq!(stepped(-8.0, 0.5, -1.0, &CODE_BASELINE_OFFSET), -8.0);
        assert_eq!(stepped(350.0, 50.0, 1.0, &MULTI_CLICK_MS), 400.0);
    }

    /// A harness on `repo` launched with `settings`, showing the large-diff
    /// revision's code rows under a collapsed header.
    fn launch(repo: &std::path::Path, settings: &str) -> crate::harness::Harness {
        let mut h =
            crate::harness::Harness::open_with_settings(Some(repo), (1280.0, 800.0), settings);
        h.wait_for_graph();
        let manifest =
            std::fs::read_to_string(repo.join(".jj/fixture-manifest")).expect("the manifest");
        let commit = manifest
            .lines()
            .find(|line| {
                line.split(' ')
                    .next()
                    .is_some_and(|names| names.split(',').any(|name| name == "large-diff"))
            })
            .and_then(|line| line.split(' ').nth(2))
            .expect("large-diff in the manifest")
            .to_owned();
        let selection = diffui_core::RevisionSelection::Commit(commit);
        h.perform(Action::SelectRevision(selection.clone()));
        h.wait_until("the large-diff diff", |ui| {
            let session = &ui.active().session;
            session.selected_revision == selection && !session.diff_in_flight()
        });
        // One width for the sidebar, which a launch sizes to its fonts'
        // minimum and a live change leaves where the user put it.
        h.act(|ui, _| {
            ui.on_sidebar_width_changed(400.0);
            ui.commit_details_collapsed = true;
        });
        h.settle();
        h.frames();
        h
    }

    /// The window once it has settled, without toasts.
    fn steady(h: &mut crate::harness::Harness) -> image::RgbaImage {
        h.act(|ui, _| ui.toasts.clear());
        h.settle();
        h.frames();
        h.screenshot()
    }

    /// A change from the dialog draws what a launch with it saved draws: no
    /// cache keeps the old fonts or row heights.
    #[test]
    fn a_live_change_draws_as_a_launch_with_it_would() {
        let repo = crate::harness::fixture_repo();
        let launched = steady(&mut launch(
            &repo,
            "theme = \"Light\"\nui_font = \"Helvetica\"\nmono_font = \"Monaco\"\n\
             code_font_size = 15\ncode_line_height = 2.1\ncode_baseline_offset = 1\n",
        ));
        let mut h = launch(&repo, "theme = \"Light\"\n");
        for change in [
            Setting::UiFont(Some("Helvetica".to_owned())),
            Setting::MonoFont(Some("Monaco".to_owned())),
            Setting::CodeFontSize(15.0),
            Setting::CodeLineHeight(2.1),
            Setting::CodeBaselineOffset(1.0),
        ] {
            h.perform(Action::ChangeSetting(change));
        }
        let live = steady(&mut h);
        assert_eq!(launched.dimensions(), live.dimensions());
        // A channel a step off is rounding where an edge lands, not a stale
        // cache, which moves whole glyphs and rows.
        let differing = launched
            .pixels()
            .zip(live.pixels())
            .filter(|(a, b)| a.0.iter().zip(b.0).any(|(a, b)| a.abs_diff(b) > 2))
            .count();
        assert_eq!(differing, 0, "pixels the live change draws differently");
    }

    #[test]
    fn a_locked_key_changes_for_the_session_without_a_write() {
        let mut ui = crate::tabs::tests::app();
        let dir = std::env::temp_dir().join(format!("diffui-settings-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ui.config_files.settings_path = Some(dir.join("settings.toml"));
        ui.config.locked = crate::config::Locked::of(&[Key::CodeFontSize]);

        ui.change_setting(Setting::CodeFontSize(16.0));
        assert_eq!(ui.config.code_type.size, 16.0);
        assert!(!dir.exists(), "nothing is written for a locked key");
        assert_eq!(ui.toasts.len(), 1);

        ui.change_setting(Setting::CodeLineHeight(2.0));
        assert_eq!(ui.config.code_type.line_height, 2.0);
        let text = std::fs::read_to_string(dir.join("settings.toml")).expect("saved");
        assert!(text.contains("code_line_height = 2.0"), "{text}");
        assert!(!text.contains("code_font_size"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

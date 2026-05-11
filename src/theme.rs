use iced::{
    Background, Border, Color, Element, Length, Shadow, Theme,
    theme,
    widget::{button, container, text},
};

use crate::diff_view::Palette;
use crate::scrollbar;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePreference {
    System,
    Dark,
    Light,
    HighContrast,
}

impl ThemePreference {
    pub const ALL: [Self; 4] = [Self::System, Self::Dark, Self::Light, Self::HighContrast];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::HighContrast => "Contrast",
        }
    }

    pub fn active(self, system_theme: theme::Mode) -> ResolvedTheme {
        match self {
            Self::System => match system_theme {
                theme::Mode::Light => ResolvedTheme::Light,
                theme::Mode::Dark | theme::Mode::None => ResolvedTheme::Dark,
            },
            Self::Dark => ResolvedTheme::Dark,
            Self::Light => ResolvedTheme::Light,
            Self::HighContrast => ResolvedTheme::HighContrast,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedTheme {
    Dark,
    Light,
    HighContrast,
}

impl ResolvedTheme {
    pub fn spec(self) -> ThemeSpec {
        match self {
            Self::Dark => ThemeSpec {
                background: Color::from_rgb(0.035, 0.040, 0.052),
                panel_background: Color::from_rgb(0.058, 0.066, 0.084),
                panel_background_elevated: Color::from_rgb(0.083, 0.094, 0.120),
                selected_file: Color::from_rgb(0.105, 0.150, 0.190),
                text: Color::from_rgb(0.925, 0.940, 0.960),
                muted_text: Color::from_rgb(0.665, 0.710, 0.760),
                subtle_text: Color::from_rgb(0.500, 0.545, 0.600),
                accent: Color::from_rgb(0.160, 0.640, 0.780),
                added_line: Color::from_rgba(0.065, 0.500, 0.260, 0.18),
                removed_line: Color::from_rgba(0.690, 0.145, 0.180, 0.19),
                added_text: Color::from_rgb(0.450, 0.890, 0.590),
                removed_text: Color::from_rgb(0.980, 0.470, 0.500),
                modified_token: Color::from_rgb(0.920, 0.690, 0.265),
                file_header: Color::from_rgb(0.070, 0.080, 0.102),
                hunk_header: Color::from_rgb(0.105, 0.132, 0.155),
                conflict_marker: Color::from_rgb(1.000, 0.310, 0.350),
                border: Color::from_rgb(0.180, 0.205, 0.245),
                note_background: Color::from_rgba(0.720, 0.490, 0.150, 0.18),
                note_text: Color::from_rgb(0.940, 0.760, 0.390),
            },
            Self::Light => ThemeSpec {
                background: Color::from_rgb(0.945, 0.946, 0.940),
                panel_background: Color::from_rgb(0.988, 0.988, 0.982),
                panel_background_elevated: Color::from_rgb(0.965, 0.966, 0.958),
                selected_file: Color::from_rgb(0.860, 0.910, 0.925),
                text: Color::from_rgb(0.120, 0.130, 0.145),
                muted_text: Color::from_rgb(0.390, 0.430, 0.470),
                subtle_text: Color::from_rgb(0.585, 0.610, 0.635),
                accent: Color::from_rgb(0.045, 0.430, 0.545),
                added_line: Color::from_rgba(0.120, 0.610, 0.330, 0.14),
                removed_line: Color::from_rgba(0.760, 0.120, 0.145, 0.14),
                added_text: Color::from_rgb(0.080, 0.430, 0.225),
                removed_text: Color::from_rgb(0.660, 0.105, 0.125),
                modified_token: Color::from_rgb(0.625, 0.410, 0.080),
                file_header: Color::from_rgb(0.930, 0.932, 0.922),
                hunk_header: Color::from_rgb(0.875, 0.905, 0.910),
                conflict_marker: Color::from_rgb(0.760, 0.080, 0.100),
                border: Color::from_rgb(0.760, 0.770, 0.780),
                note_background: Color::from_rgba(0.820, 0.560, 0.110, 0.18),
                note_text: Color::from_rgb(0.500, 0.320, 0.045),
            },
            Self::HighContrast => ThemeSpec {
                background: Color::BLACK,
                panel_background: Color::from_rgb(0.030, 0.030, 0.030),
                panel_background_elevated: Color::from_rgb(0.070, 0.070, 0.070),
                selected_file: Color::from_rgb(0.000, 0.180, 0.240),
                text: Color::WHITE,
                muted_text: Color::from_rgb(0.780, 0.820, 0.840),
                subtle_text: Color::from_rgb(0.620, 0.660, 0.690),
                accent: Color::from_rgb(0.000, 0.900, 1.000),
                added_line: Color::from_rgb(0.000, 0.235, 0.080),
                removed_line: Color::from_rgb(0.300, 0.000, 0.045),
                added_text: Color::from_rgb(0.500, 1.000, 0.600),
                removed_text: Color::from_rgb(1.000, 0.520, 0.560),
                modified_token: Color::from_rgb(1.000, 0.920, 0.000),
                file_header: Color::from_rgb(0.120, 0.120, 0.120),
                hunk_header: Color::from_rgb(0.000, 0.220, 0.310),
                conflict_marker: Color::from_rgb(1.000, 0.140, 0.140),
                border: Color::from_rgb(0.570, 0.620, 0.660),
                note_background: Color::from_rgb(0.260, 0.210, 0.000),
                note_text: Color::from_rgb(1.000, 0.940, 0.500),
            },
        }
    }

    pub fn iced_theme(self) -> Theme {
        match self {
            Self::Dark => Theme::Dark,
            Self::Light => Theme::Light,
            Self::HighContrast => Theme::Dark,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeSpec {
    pub background: Color,
    pub panel_background: Color,
    pub panel_background_elevated: Color,
    pub selected_file: Color,
    pub text: Color,
    pub muted_text: Color,
    pub subtle_text: Color,
    pub accent: Color,
    pub added_line: Color,
    pub removed_line: Color,
    pub added_text: Color,
    pub removed_text: Color,
    pub modified_token: Color,
    pub file_header: Color,
    pub hunk_header: Color,
    pub conflict_marker: Color,
    pub border: Color,
    pub note_background: Color,
    pub note_text: Color,
}

pub fn diff_palette(theme: ThemeSpec) -> Palette {
    Palette {
        text: theme.text,
        text_muted: theme.subtle_text,
        addition_text: theme.added_text,
        deletion_text: theme.removed_text,
        modified_token: theme.modified_token,
        conflict_marker: theme.conflict_marker,
        note_text: theme.note_text,
        panel: theme.panel_background_elevated,
        file_header: theme.file_header,
        hunk_header: theme.hunk_header,
        addition_background: theme.added_line,
        deletion_background: theme.removed_line,
        note_background: theme.note_background,
        gutter_background: theme.panel_background,
        border: theme.border,
        // Translucent accent so the underlying syntax-highlighted text and
        // line-change tints stay readable under the selection.
        selection: Color {
            a: 0.30,
            ..theme.accent
        },
        scrollbar: scrollbar_style(theme),
    }
}

pub fn scrollbar_style(theme: ThemeSpec) -> scrollbar::ScrollbarStyle {
    scrollbar::ScrollbarStyle {
        // Soft pill behind the thumb, lighter than the thumb so the two
        // read as distinct without looking heavy on light themes.
        track_color: Color {
            a: 0.18,
            ..theme.muted_text
        },
        thumb_color: Color {
            a: 0.55,
            ..theme.muted_text
        },
    }
}

pub fn app_shell_style(theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(theme.background)
        .color(theme.text)
}

pub fn vertical_divider<Message: 'static>(theme: ThemeSpec) -> Element<'static, Message> {
    container(text(""))
        .width(Length::Fixed(1.0))
        .height(Length::Fill)
        .style(move |_| {
            container::Style::default()
                .background(theme.border)
                .border(Border {
                    width: 0.0,
                    color: Color::TRANSPARENT,
                    radius: 0.0.into(),
                })
        })
        .into()
}

pub fn sidebar_header_style(theme: ThemeSpec) -> container::Style {
    container::Style::default().background(theme.panel_background)
}

pub fn sidebar_panel_style(theme: ThemeSpec) -> container::Style {
    container::Style::default().background(theme.panel_background)
}

pub fn diff_panel_style(theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(theme.panel_background)
        .border(Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 0.0.into(),
        })
}

pub fn theme_switcher_button_style(
    status: button::Status,
    selected: bool,
    theme: ThemeSpec,
) -> button::Style {
    let background = match (selected, status) {
        (true, _) => theme.selected_file,
        (false, button::Status::Hovered | button::Status::Pressed) => theme.selected_file,
        (false, _) => theme.panel_background_elevated,
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: theme.text,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 0.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Translucent chip background derived from the chip's text color. This way
/// the chip reads independently of whether the row is selected — its visual
/// frame comes from the tint rather than from the row's solid background.
pub fn chip_background(color: Color) -> Color {
    Color { a: 0.20, ..color }
}

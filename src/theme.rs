use iced::{
    Background, Border, Color, Element, Font, Length, Shadow, Theme, border,
    font::{Family, Weight},
    theme,
    widget::{container, scrollable, text},
};

use crate::diff_view::Palette;
use crate::scrollbar;

/// Apply a `Medium` (or other emphasized) weight to a font *only* when
/// the font is a specific named family the OS can actually look up.
///
/// On macOS, asking the platform font matcher for a generic family
/// (`Family::SansSerif`, `Family::Monospace`) at a non-default weight
/// frequently returns nothing — the matcher resolves the generic to the
/// platform UI font but doesn't enumerate weights under it, so the
/// renderer falls back to `.notdef` glyphs (the empty-box "tofu").
/// Specific named families (`Family::Name("Cascadia Code")` etc.) round-
/// trip through fontdb's family→weight index correctly, so weight
/// overrides are safe there.
///
/// Use this for any label that wants to emphasize itself with a
/// heavier weight; pass-through to the platform default when the family
/// is generic and trust the OS's UI typography defaults.
pub fn emphasis_font(base: Font, weight: Weight) -> Font {
    match base.family {
        Family::Name(_) => Font { weight, ..base },
        // Generic families: don't override weight — the platform will
        // pick a sensible default and we'd otherwise risk tofu.
        _ => base,
    }
}

/// Geometry knobs shared by every `iced::widget::scrollable` we style.
/// Mirrors the values in `crate::scrollbar` (12px outer, 2px margin → 8px
/// pill) so the iced-managed scrollbars in the palette / find overlays
/// read as the same widget as the hand-drawn ones in `RevisionList` and
/// `DiffView`.
pub const SCROLLBAR_WIDTH: f32 = 8.0;
pub const SCROLLBAR_MARGIN: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePreference {
    System,
    Dark,
    Light,
    HighContrast,
}

impl ThemePreference {
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
            // Dark-first warm ink with a coral accent. Tokens mirror the
            // Diffui v2 design system: `bg-canvas`, `bg-pane`, `bg-elev`,
            // `bg-active`, `ink-100..300`, `accent #FF7A59`. The trunk lane
            // is intentionally decoupled from the accent — the accent is
            // reserved for the working copy, selection, and focus, while
            // the graph rides on a violet base so coral doesn't fight the
            // green/red diff signals.
            Self::Dark => ThemeSpec {
                background: Color::from_rgb(0.043, 0.051, 0.071),
                panel_background: Color::from_rgb(0.067, 0.078, 0.106),
                panel_background_elevated: Color::from_rgb(0.094, 0.110, 0.145),
                selected_file: Color::from_rgb(0.153, 0.176, 0.231),
                text: Color::from_rgb(0.902, 0.910, 0.933),
                muted_text: Color::from_rgb(0.690, 0.710, 0.761),
                subtle_text: Color::from_rgb(0.486, 0.510, 0.580),
                accent: Color::from_rgb(1.000, 0.478, 0.349),
                added_line: Color::from_rgba(0.357, 0.773, 0.478, 0.07),
                removed_line: Color::from_rgba(0.929, 0.431, 0.361, 0.07),
                added_text: Color::from_rgb(0.357, 0.773, 0.478),
                removed_text: Color::from_rgb(0.929, 0.431, 0.361),
                modified_token: Color::from_rgb(0.961, 0.706, 0.345),
                info: Color::from_rgb(0.416, 0.659, 1.000),
                // Pulled darker than `panel_background` so the file
                // header strip reads as a clear divider when scrolling
                // across files (the previous value matched the panel
                // and made the header invisible against the diff body).
                file_header: Color::from_rgb(0.043, 0.051, 0.071),
                hunk_header: Color::from_rgba(0.416, 0.659, 1.000, 0.06),
                conflict_marker: Color::from_rgb(0.949, 0.361, 0.361),
                border: Color::from_rgb(0.137, 0.153, 0.196),
                note_background: Color::from_rgba(0.961, 0.706, 0.345, 0.14),
                note_text: Color::from_rgb(0.961, 0.706, 0.345),
                lane_base: Color::from_rgb(0.655, 0.545, 0.980),
            },
            // Neutral whites — panes are pure white, canvas is a faint
            // gray so the panes still read as elevated. Coral accent stays
            // for selection / working-copy signalling.
            Self::Light => ThemeSpec {
                background: Color::from_rgb(0.957, 0.957, 0.961),
                panel_background: Color::from_rgb(1.000, 1.000, 1.000),
                panel_background_elevated: Color::from_rgb(0.976, 0.976, 0.980),
                selected_file: Color::from_rgb(0.910, 0.918, 0.933),
                text: Color::from_rgb(0.106, 0.114, 0.133),
                muted_text: Color::from_rgb(0.314, 0.337, 0.380),
                subtle_text: Color::from_rgb(0.486, 0.506, 0.553),
                accent: Color::from_rgb(0.851, 0.275, 0.122),
                added_line: Color::from_rgba(0.184, 0.620, 0.365, 0.06),
                removed_line: Color::from_rgba(0.800, 0.247, 0.184, 0.05),
                added_text: Color::from_rgb(0.184, 0.620, 0.365),
                removed_text: Color::from_rgb(0.800, 0.247, 0.184),
                modified_token: Color::from_rgb(0.773, 0.518, 0.133),
                info: Color::from_rgb(0.165, 0.435, 0.859),
                // Same role as in the dark theme: visibly darker than the
                // surrounding panel so file headers stand out as the
                // scroll body slides past, but not so dark that it
                // overpowers the body content.
                file_header: Color::from_rgb(0.937, 0.941, 0.949),
                hunk_header: Color::from_rgba(0.165, 0.435, 0.859, 0.06),
                conflict_marker: Color::from_rgb(0.800, 0.247, 0.184),
                border: Color::from_rgb(0.882, 0.886, 0.898),
                note_background: Color::from_rgba(0.773, 0.518, 0.133, 0.14),
                note_text: Color::from_rgb(0.500, 0.320, 0.045),
                lane_base: Color::from_rgb(0.486, 0.357, 0.910),
            },
            Self::HighContrast => ThemeSpec {
                background: Color::BLACK,
                panel_background: Color::from_rgb(0.030, 0.030, 0.030),
                panel_background_elevated: Color::from_rgb(0.070, 0.070, 0.070),
                selected_file: Color::from_rgb(0.000, 0.180, 0.240),
                text: Color::WHITE,
                muted_text: Color::from_rgb(0.780, 0.820, 0.840),
                subtle_text: Color::from_rgb(0.620, 0.660, 0.690),
                accent: Color::from_rgb(1.000, 0.478, 0.349),
                added_line: Color::from_rgb(0.000, 0.235, 0.080),
                removed_line: Color::from_rgb(0.300, 0.000, 0.045),
                added_text: Color::from_rgb(0.500, 1.000, 0.600),
                removed_text: Color::from_rgb(1.000, 0.520, 0.560),
                modified_token: Color::from_rgb(1.000, 0.920, 0.000),
                info: Color::from_rgb(0.380, 0.770, 1.000),
                file_header: Color::from_rgb(0.120, 0.120, 0.120),
                hunk_header: Color::from_rgb(0.000, 0.220, 0.310),
                conflict_marker: Color::from_rgb(1.000, 0.140, 0.140),
                border: Color::from_rgb(0.570, 0.620, 0.660),
                note_background: Color::from_rgb(0.260, 0.210, 0.000),
                note_text: Color::from_rgb(1.000, 0.940, 0.500),
                lane_base: Color::from_rgb(0.760, 0.620, 1.000),
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
    /// Informational blue. Used for the `M` (Modified) file-status chip
    /// and the diff-view hunk header — wherever the design system uses
    /// `--info`. Separate from `accent` so the accent (coral) stays
    /// reserved for selection, working copy, and primary actions.
    pub info: Color,
    pub file_header: Color,
    pub hunk_header: Color,
    pub conflict_marker: Color,
    pub border: Color,
    pub note_background: Color,
    pub note_text: Color,
    /// Base color for lane 0 of the revision graph. Subsequent lanes
    /// derive their hue from this via `RevisionGraphStyle::lane_color`'s
    /// HSL rotation. Decoupled from `accent` so the trunk (violet in the
    /// design) doesn't fight diff add/del greens and reds, and so the
    /// coral accent stays reserved for the working copy and selection.
    pub lane_base: Color,
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
        // line-change tints stay readable under the selection. Alpha tuned
        // to match the design's `--accent-soft` token — strong enough to
        // clearly mark the selection, soft enough to keep the diff colors
        // legible behind it.
        selection: Color {
            a: 0.18,
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

/// Style closure for `iced::widget::scrollable`. Reuses the
/// `scrollbar_style` colors and a fully-rounded pill border so the iced
/// scrollbar visually matches the hand-drawn one in `RevisionList` /
/// `DiffView`. Status is ignored — the custom widgets don't react to hover
/// either, and matching that keeps the two indistinguishable side by side.
pub fn iced_scrollable_style(theme: ThemeSpec, _status: scrollable::Status) -> scrollable::Style {
    let colors = scrollbar_style(theme);
    let pill = border::rounded(SCROLLBAR_WIDTH / 2.0);
    let rail = scrollable::Rail {
        background: Some(Background::Color(colors.track_color)),
        border: pill,
        scroller: scrollable::Scroller {
            background: Background::Color(colors.thumb_color),
            border: pill,
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
            shadow: Shadow::default(),
            icon: theme.muted_text,
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

pub fn horizontal_divider<Message: 'static>(theme: ThemeSpec) -> Element<'static, Message> {
    container(text(""))
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
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

/// Translucent chip background derived from the chip's text color. This way
/// the chip reads independently of whether the row is selected — its visual
/// frame comes from the tint rather than from the row's solid background.
///
/// Alpha picked to match the design system's `--*-soft` tokens (e.g.
/// `--accent-soft: rgba(..,..,..,.14)`, `--add-soft: rgba(..,..,..,.13)`).
/// The previous 0.20 made the chip dominate the row; .14 keeps it as a
/// quiet tint that lets the colored glyph carry the signal.
pub fn chip_background(color: Color) -> Color {
    Color { a: 0.14, ..color }
}

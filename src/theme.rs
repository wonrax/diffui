use iced::{
    Background, Border, Color, Element, Font, Length, Shadow, Theme, Vector, border,
    font::{Family, Weight},
    theme,
    widget::{button, container, scrollable, text, text_input},
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
                subtle_text: Color::from_rgb(0.563, 0.584, 0.643),
                accent: Color::from_rgb(1.000, 0.478, 0.349),
                added_line: Color::from_rgba(0.357, 0.773, 0.478, 0.07),
                removed_line: Color::from_rgba(0.936, 0.487, 0.424, 0.07),
                added_text: Color::from_rgb(0.357, 0.773, 0.478),
                removed_text: Color::from_rgb(0.936, 0.487, 0.424),
                modified_token: Color::from_rgb(0.961, 0.706, 0.345),
                info: Color::from_rgb(0.416, 0.659, 1.000),
                // A step *lighter* than the elevated code surface so the
                // strip reads as a raised header rather than a hole cut
                // into the scroll body.
                file_header: Color::from_rgb(0.122, 0.141, 0.184),
                hunk_header: Color::from_rgba(0.416, 0.659, 1.000, 0.08),
                conflict_marker: Color::from_rgb(0.958, 0.469, 0.469),
                border: Color::from_rgb(0.137, 0.153, 0.196),
                note_background: Color::from_rgba(0.961, 0.706, 0.345, 0.14),
                note_text: Color::from_rgb(0.961, 0.706, 0.345),
                lane_base: Color::from_rgb(0.655, 0.545, 0.980),
                syntax_keyword: Color::from_rgb(0.729, 0.624, 0.996),
                syntax_type: Color::from_rgb(0.337, 0.788, 0.745),
                syntax_function: Color::from_rgb(0.541, 0.729, 1.000),
                syntax_literal: Color::from_rgb(0.961, 0.706, 0.345),
                syntax_property: Color::from_rgb(0.557, 0.812, 0.902),
            },
            // Neutral whites — panes are pure white, canvas is a faint
            // gray so the panes still read as elevated. Coral accent stays
            // for selection / working-copy signalling.
            //
            // Every ink here is derived for a white ground rather than
            // mirrored from the dark spec: a colour that reads well against
            // near-black is nowhere near AA against near-white, and mirroring
            // is what left the captions, the diff inks and the chips between
            // 2.6:1 and 3.9:1. Hues are the dark theme's; only lightness
            // moved, down to whatever clears 4.5:1 against the *darkest*
            // surface each ink lands on — usually the selected row, not the
            // white pane. `every_pairing_meets_wcag_aa` holds the line.
            Self::Light => ThemeSpec {
                background: Color::from_rgb(0.957, 0.957, 0.961),
                panel_background: Color::from_rgb(1.000, 1.000, 1.000),
                panel_background_elevated: Color::from_rgb(0.976, 0.976, 0.980),
                selected_file: Color::from_rgb(0.910, 0.918, 0.933),
                text: Color::from_rgb(0.106, 0.114, 0.133),
                muted_text: Color::from_rgb(0.314, 0.337, 0.380),
                subtle_text: Color::from_rgb(0.392, 0.410, 0.451),
                accent: Color::from_rgb(0.632, 0.204, 0.091),
                added_line: Color::from_rgba(0.120, 0.405, 0.238, 0.06),
                removed_line: Color::from_rgba(0.642, 0.198, 0.148, 0.05),
                added_text: Color::from_rgb(0.120, 0.405, 0.238),
                removed_text: Color::from_rgb(0.642, 0.198, 0.148),
                modified_token: Color::from_rgb(0.488, 0.327, 0.084),
                info: Color::from_rgb(0.119, 0.345, 0.702),
                // Same role as in the dark theme: distinct from the white
                // code surface so file headers stand out as the scroll
                // body slides past, but not so dark that it overpowers
                // the body content.
                file_header: Color::from_rgb(0.949, 0.953, 0.961),
                hunk_header: Color::from_rgba(0.119, 0.345, 0.702, 0.06),
                conflict_marker: Color::from_rgb(0.642, 0.198, 0.148),
                border: Color::from_rgb(0.882, 0.886, 0.898),
                note_background: Color::from_rgba(0.488, 0.327, 0.084, 0.14),
                note_text: Color::from_rgb(0.500, 0.320, 0.045),
                lane_base: Color::from_rgb(0.486, 0.357, 0.910),
                syntax_keyword: Color::from_rgb(0.399, 0.202, 0.839),
                syntax_type: Color::from_rgb(0.038, 0.402, 0.377),
                syntax_function: Color::from_rgb(0.134, 0.346, 0.673),
                syntax_literal: Color::from_rgb(0.502, 0.320, 0.057),
                syntax_property: Color::from_rgb(0.110, 0.378, 0.521),
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
                conflict_marker: Color::from_rgb(1.000, 0.445, 0.445),
                border: Color::from_rgb(0.570, 0.620, 0.660),
                note_background: Color::from_rgb(0.260, 0.210, 0.000),
                note_text: Color::from_rgb(1.000, 0.940, 0.500),
                lane_base: Color::from_rgb(0.760, 0.620, 1.000),
                syntax_keyword: Color::from_rgb(0.870, 0.740, 1.000),
                syntax_type: Color::from_rgb(0.400, 1.000, 0.920),
                syntax_function: Color::from_rgb(0.600, 0.840, 1.000),
                syntax_literal: Color::from_rgb(1.000, 0.880, 0.420),
                syntax_property: Color::from_rgb(0.720, 0.920, 1.000),
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
    /// Dedicated syntax ramp. Deliberately decoupled from the diff
    /// semantics (`added_text`, `removed_text`, `conflict_marker`) so
    /// code coloring never echoes add/del/conflict signals — a keyword
    /// must not look like a conflict just because both are red.
    pub syntax_keyword: Color,
    pub syntax_type: Color,
    pub syntax_function: Color,
    /// Strings and numbers.
    pub syntax_literal: Color,
    pub syntax_property: Color,
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
        syntax_keyword: theme.syntax_keyword,
        syntax_type: theme.syntax_type,
        syntax_function: theme.syntax_function,
        syntax_literal: theme.syntax_literal,
        syntax_property: theme.syntax_property,
        panel: theme.panel_background_elevated,
        file_header: theme.file_header,
        hunk_header: theme.hunk_header,
        addition_background: theme.added_line,
        deletion_background: theme.removed_line,
        // Word-diff token tint, composited over the line tint. A translucent
        // wash of the add/del *text* color tracks both light and dark themes
        // without growing the theme spec.
        addition_emphasis: Color {
            a: 0.28,
            ..theme.added_text
        },
        deletion_emphasis: Color {
            a: 0.28,
            ..theme.removed_text
        },
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

/// Saturated color for a file's status letter chip. Mapping follows the
/// design system: A→green (added_text), M→blue (info), D→red (removed_text),
/// R→amber (modified_token). The chip's background is derived from this
/// color via `chip_background` so the glyph and the tint share a hue.
pub fn file_status_color(status: diffui_core::DiffFileStatus, theme: ThemeSpec) -> Color {
    use diffui_core::DiffFileStatus;
    match status {
        DiffFileStatus::Added => theme.added_text,
        DiffFileStatus::Deleted => theme.removed_text,
        DiffFileStatus::Modified => theme.info,
        DiffFileStatus::Renamed => theme.modified_token,
        DiffFileStatus::Conflicted => theme.conflict_marker,
    }
}

/// App-wide type scale. Every text run in the chrome picks one of these
/// steps so sizes can't drift apart file by file. (The diff/code panes are
/// exempt — their text follows the configured code font size.)
pub mod text_size {
    /// Tiny counters and markers: tab badges, `+N` pills.
    pub const BADGE: f32 = 10.0;
    /// Secondary metadata beside a body run: shortcut hints, timestamps,
    /// result summaries.
    pub const CAPTION: f32 = 11.0;
    /// Chrome controls: buttons, tabs, menu rows, footer, dialog body.
    pub const UI: f32 = 12.0;
    /// Primary content runs: ids, chips, list rows, inputs.
    pub const BODY: f32 = 13.0;
    /// Emphasized content: revision descriptions, empty states.
    pub const BODY_LG: f32 = 14.0;
    /// Dialog and palette titles.
    pub const TITLE: f32 = 15.0;
    /// The welcome screen's app heading.
    pub const DISPLAY: f32 = 22.0;
}

/// Corner radii shared across the chrome. Chips keep their own tighter
/// rounding ([`crate::chip::RADIUS`]).
pub mod radius {
    /// Inputs, hover washes, list/menu rows.
    pub const CONTROL: f32 = 5.0;
    /// Every button-shaped control (ghost, raised, primary, dialog) and
    /// the toolbar's segmented well — one rounding for everything that
    /// reads as a button.
    pub const BUTTON: f32 = 7.0;
    /// Inset row cards (revision/file selection).
    pub const PUSH: f32 = 6.0;
    /// Floating cards: popovers, modals, and tooltips.
    pub const SURFACE: f32 = 10.0;
}

/// Ghost button: invisible at rest, translucent wash on hover/press. The
/// wash is [`chip_background`] of `muted_text` so it reads the same on
/// panel and elevated backgrounds. For icon-only and small-label controls
/// embedded in framed surfaces (tab strip, find card, popover footers) —
/// standalone actions use [`raised_button_style`] so they stay visible at
/// rest.
pub fn ghost_button_style(theme: ThemeSpec, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => {
                Some(Background::Color(chip_background(theme.muted_text)))
            }
            _ => None,
        },
        text_color: theme.text,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: radius::BUTTON.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Blend `a` toward `b` by `t` (0..=1), ignoring alpha. Used to derive the
/// raised buttons' bezel stops from surface tokens without growing the spec,
/// and the sidebar's target-mode wash.
pub(crate) fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

/// Whisper of a drop shadow shared by the raised/primary/destructive
/// buttons — just enough to lift them off the bar, nowhere near a card.
fn button_shadow() -> Shadow {
    Shadow {
        color: Color {
            a: 0.15,
            ..Color::BLACK
        },
        offset: Vector::new(0.0, 1.0),
        blur_radius: 2.0,
    }
}

/// Fill for [`raised_button_style`] at rest: a flat step from the toolbar
/// surface toward the selection tone, so the button separates from the
/// (also elevated) bar it sits on. Lighter on dark themes, darker on
/// light — both the conventional direction.
fn raised_fill(theme: ThemeSpec, t: f32) -> Color {
    mix(theme.panel_background_elevated, theme.selected_file, t)
}

/// Fill for recessed wells (the toolbar's Diff/Source switcher): the
/// window canvas pushed a step darker, so the well clearly sinks below
/// both the canvas and the elevated bar it sits on — recessed surfaces
/// deepen in both dark and light themes.
pub fn well_fill(theme: ThemeSpec) -> Color {
    mix(theme.background, Color::BLACK, 0.06)
}

/// The standard button: a flat fill one step above its surface, a crisp
/// 1px border, and a whisper of drop shadow — visible at rest without any
/// skeuomorphic shading. Hover deepens the fill; pressing deepens it
/// further and drops the shadow so the button reads as pushed in.
pub fn raised_button_style(theme: ThemeSpec, status: button::Status) -> button::Style {
    let (fill, shadow) = match status {
        button::Status::Pressed => (raised_fill(theme, 0.9), Shadow::default()),
        button::Status::Hovered => (raised_fill(theme, 0.65), button_shadow()),
        _ => (raised_fill(theme, 0.35), button_shadow()),
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: theme.text,
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: radius::BUTTON.into(),
        },
        shadow,
        snap: true,
    }
}

/// Primary call-to-action: flat accent fill with the raised buttons\'
/// whisper of shadow, inverted label. The one loudest action on a
/// surface (the welcome screen\'s "Open repository…").
pub fn primary_button_style(theme: ThemeSpec) -> button::Style {
    button::Style {
        background: Some(Background::Color(theme.accent)),
        text_color: theme.background,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: radius::BUTTON.into(),
        },
        shadow: button_shadow(),
        snap: true,
    }
}

/// Destructive call-to-action for confirm dialogs: flat red fill so the
/// dangerous choice is unmistakable next to the neutral cancel button.
pub fn destructive_button_style(theme: ThemeSpec) -> button::Style {
    button::Style {
        background: Some(Background::Color(theme.removed_text)),
        text_color: theme.background,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: radius::BUTTON.into(),
        },
        shadow: button_shadow(),
        snap: true,
    }
}

/// Push button for dialog footers — the shared raised chrome.
pub fn dialog_button_style(theme: ThemeSpec, status: button::Status) -> button::Style {
    raised_button_style(theme, status)
}

/// Floating card anchored to a trigger with nothing dimming the content
/// behind it (dropdown menus, the activity popover). The heavy drop shadow
/// is what separates it from the busy panel underneath.
pub fn popover_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme.panel_background_elevated)),
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: radius::SURFACE.into(),
        },
        shadow: floating_shadow(8.0, 16.0),
        ..container::Style::default()
    }
}

/// The drop shadow every floating surface shares — same tint and alpha
/// everywhere, geometry scaled to the surface (popovers are bigger than
/// tooltips).
pub fn floating_shadow(offset_y: f32, blur_radius: f32) -> Shadow {
    Shadow {
        color: Color {
            a: 0.05,
            ..Color::BLACK
        },
        offset: Vector::new(0.0, offset_y),
        blur_radius,
    }
}

/// Centered card over a scrim (confirm dialog, open-repo dialog, command
/// palette). The scrim already darkens what's behind the modal, so a heavy
/// shadow would just look like double dimming — soft, just enough to
/// suggest the card is lifted.
pub fn modal_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme.panel_background_elevated)),
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: radius::SURFACE.into(),
        },
        shadow: floating_shadow(4.0, 12.0),
        ..container::Style::default()
    }
}

/// Dimming layer behind modal surfaces. One alpha everywhere so stacked
/// takeovers (confirm dialog, open-repo dialog, command palette) feel
/// equally deep.
pub fn scrim_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color {
            a: 0.40,
            ..Color::BLACK
        })),
        ..container::Style::default()
    }
}

/// Text-input field: a 1px outline carrying the input identity colors
/// (placeholder / value / selection). Structural variants — the palette's
/// borderless embedded input — override background and border but keep the
/// colors, so every input hints and selects identically.
pub fn input_style(theme: ThemeSpec) -> text_input::Style {
    text_input::Style {
        background: Background::Color(theme.panel_background),
        border: Border {
            width: 1.0,
            color: theme.border,
            radius: radius::CONTROL.into(),
        },
        icon: theme.muted_text,
        placeholder: theme.subtle_text,
        value: theme.text,
        selection: Color {
            a: 0.25,
            ..theme.accent
        },
    }
}

/// Hover tooltip card. The revision list's custom-drawn tooltip overlay
/// mirrors these colors through `RevisionListStyle` and its radius through
/// [`radius::SURFACE`].
pub fn tooltip_style(theme: ThemeSpec) -> container::Style {
    container::Style {
        background: Some(Background::Color(theme.panel_background_elevated)),
        text_color: Some(theme.text),
        border: Border {
            color: theme.border,
            width: 1.0,
            radius: radius::SURFACE.into(),
        },
        ..container::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG AA for body text. Everything the chrome renders is between 10 and
    /// 22 px, so nothing here qualifies as "large text" (18 pt / 24 px) and
    /// gets the relaxed 3:1.
    const TEXT: f32 = 4.5;
    /// WCAG AA for a graphical object rather than a run of prose: the graph's
    /// lane strokes, and the single letter in a status chip — which is read off
    /// its shape and position as much as its colour, and never carries meaning
    /// the row's text doesn't repeat.
    const GLYPH: f32 = 3.0;

    /// Relative luminance, WCAG 2.1 §dfn-relative-luminance.
    fn luminance(color: Color) -> f32 {
        let channel = |c: f32| {
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }

    fn contrast(a: Color, b: Color) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// `fg` composited over the opaque `bg` — how a translucent wash (a chip
    /// tint, a diff line, the selection) actually reaches the eye.
    fn over(fg: Color, bg: Color) -> Color {
        Color {
            r: fg.r * fg.a + bg.r * (1.0 - fg.a),
            g: fg.g * fg.a + bg.g * (1.0 - fg.a),
            b: fg.b * fg.a + bg.b * (1.0 - fg.a),
            a: 1.0,
        }
    }

    /// Every foreground the theme puts on a background, with the minimum ratio
    /// it has to clear. The point of listing them out rather than checking a
    /// couple of obvious ones is that the failures were never in the obvious
    /// places: the body text was fine, the captions and the chip letters were
    /// not.
    fn pairings(spec: ThemeSpec) -> Vec<(&'static str, Color, Color, f32)> {
        let mut pairs = Vec::new();
        let surfaces = [
            ("canvas", spec.background),
            ("pane", spec.panel_background),
            ("elevated", spec.panel_background_elevated),
            ("selected row", spec.selected_file),
            ("file header", spec.file_header),
        ];
        // Chrome text sits on any of the surfaces — a sidebar row is selected
        // as often as not, and that is the darkest of them on a light theme.
        for (label, color) in [
            ("text", spec.text),
            ("muted text", spec.muted_text),
            ("subtle text", spec.subtle_text),
        ] {
            for (surface, background) in surfaces {
                pairs.push((label, color, background, TEXT));
                let _ = surface;
            }
        }

        // Code and diff inks, on the code surface and each wash laid over it.
        let code = spec.panel_background_elevated;
        let code_surfaces = [
            code,
            spec.file_header,
            over(spec.added_line, code),
            over(spec.removed_line, code),
            over(spec.hunk_header, code),
            over(
                Color {
                    a: 0.18,
                    ..spec.accent
                },
                code,
            ),
        ];
        for (label, color) in [
            ("added text", spec.added_text),
            ("removed text", spec.removed_text),
            ("modified token", spec.modified_token),
            ("info", spec.info),
            ("conflict marker", spec.conflict_marker),
            ("syntax keyword", spec.syntax_keyword),
            ("syntax type", spec.syntax_type),
            ("syntax function", spec.syntax_function),
            ("syntax literal", spec.syntax_literal),
            ("syntax property", spec.syntax_property),
        ] {
            for background in code_surfaces {
                pairs.push((label, color, background, TEXT));
            }
        }

        // A chip's letter over its own tint — the tint is derived from the
        // letter's colour, so the two move together and the pairing is easy to
        // leave unchecked.
        for (label, color) in [
            ("added chip", spec.added_text),
            ("removed chip", spec.removed_text),
            ("modified chip", spec.info),
            ("renamed chip", spec.modified_token),
            ("conflicted chip", spec.conflict_marker),
            ("accent chip", spec.accent),
        ] {
            for base in [spec.panel_background, spec.selected_file] {
                pairs.push((label, color, over(chip_background(color), base), GLYPH));
            }
        }

        pairs.push((
            "note text",
            spec.note_text,
            over(spec.note_background, spec.panel_background_elevated),
            TEXT,
        ));
        // Inverted labels on a filled button.
        pairs.push(("primary button label", spec.background, spec.accent, TEXT));
        pairs.push((
            "destructive button label",
            spec.background,
            spec.removed_text,
            TEXT,
        ));
        // The graph's lanes are strokes, not glyphs to read.
        for base in [spec.panel_background, spec.selected_file] {
            pairs.push(("lane", spec.lane_base, base, GLYPH));
        }
        pairs
    }

    /// The light palette was first written by mirroring the dark values, which
    /// is why so much of it landed between 2.6:1 and 3.9:1 on a white ground.
    /// Re-deriving it fixed that once; this keeps the next edit honest.
    #[test]
    fn every_pairing_meets_wcag_aa() {
        let mut failures = Vec::new();
        for theme in [
            ResolvedTheme::Dark,
            ResolvedTheme::Light,
            ResolvedTheme::HighContrast,
        ] {
            for (label, foreground, background, minimum) in pairings(theme.spec()) {
                let ratio = contrast(foreground, background);
                if ratio < minimum {
                    failures.push(format!("{theme:?}: {label} is {ratio:.2}, needs {minimum}"));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}

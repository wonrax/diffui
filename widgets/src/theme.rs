//! diffui's palette and shared geometry tokens.
//!
//! [`ThemeSpec`] is the source of truth for every colour on screen. kit's own
//! theme is derived from it ([`apply_to_kit`]), so kit components drawn with
//! their stock variants read as diffui's: there is one palette, not two.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, BoxShadow, Hsla, Rgba, Styled, WindowAppearance, point, px};

use crate::diff_view::Palette;
use crate::scrollbar;

/// A palette colour. Plain sRGB channels in 0..=1, so tokens can be mixed and
/// re-alpha'd with struct update (`Color { a: 0.2, ..theme.accent }`). It goes
/// straight into `.bg()`, `.text_color()` and `.border_color()`.
pub type Color = Rgba;

const fn rgb(r: f32, g: f32, b: f32) -> Color {
    Rgba { r, g, b, a: 1.0 }
}

const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Color {
    Rgba { r, g, b, a }
}

pub const BLACK: Color = rgb(0.0, 0.0, 0.0);
pub const WHITE: Color = rgb(1.0, 1.0, 1.0);
pub const TRANSPARENT: Color = rgba(0.0, 0.0, 0.0, 0.0);

/// Font weight, with GPUI's named constants (`Weight::MEDIUM`, …).
pub type Weight = gpui_kit::FontWeight;

/// A font, by value. GPUI's own `Font` holds shared strings and has to be
/// cloned at every use; this is `Copy`, so a theme or a config can hand it
/// around like a colour, and [`Font::gpui`] makes the real one where it's drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Font {
    pub family: &'static str,
    pub weight: Weight,
}

impl Font {
    /// The platform UI font.
    pub const DEFAULT: Self = Self::new(".SystemUIFont");
    /// The platform's monospaced font.
    pub const MONOSPACE: Self = Self::new("Menlo");

    pub const fn new(family: &'static str) -> Self {
        Self {
            family,
            weight: Weight::NORMAL,
        }
    }

    pub fn gpui(self) -> gpui_kit::Font {
        gpui_kit::Font {
            weight: self.weight,
            ..gpui_kit::font(self.family)
        }
    }
}

/// `base` at `weight`. Unlike iced's matcher, CoreText resolves weights of the
/// system font too, so this no longer has to pass generic families through.
pub fn emphasis_font(base: Font, weight: Weight) -> Font {
    Font { weight, ..base }
}

/// Insets of a box. Used where padding is computed rather than a fixed token
/// ([`crate::typography`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Padding {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Padding {
    pub const ZERO: Self = Self {
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
        left: 0.0,
    };

    /// Applies this padding to an element.
    pub fn apply<E: Styled>(self, element: E) -> E {
        element
            .pt(px(self.top))
            .pr(px(self.right))
            .pb(px(self.bottom))
            .pl(px(self.left))
    }
}

/// `[vertical, horizontal]`.
impl From<[f32; 2]> for Padding {
    fn from([y, x]: [f32; 2]) -> Self {
        Self {
            top: y,
            right: x,
            bottom: y,
            left: x,
        }
    }
}

/// Geometry of the scrollbar pill. Mirrors `crate::scrollbar` (a 12px
/// footprint, 2px clear of every edge → an 8px pill).
pub const SCROLLBAR_WIDTH: f32 = 8.0;
pub const SCROLLBAR_MARGIN: f32 = 2.0;
pub const COLLAPSED_PANEL_WIDTH: f32 = 36.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePreference {
    System,
    Dark,
    Light,
    HighContrast,
}

impl ThemePreference {
    pub fn active(self, system_theme: SystemAppearance) -> ResolvedTheme {
        match self {
            Self::System => match system_theme {
                SystemAppearance::Light => ResolvedTheme::Light,
                SystemAppearance::Dark => ResolvedTheme::Dark,
            },
            Self::Dark => ResolvedTheme::Dark,
            Self::Light => ResolvedTheme::Light,
            Self::HighContrast => ResolvedTheme::HighContrast,
        }
    }
}

/// What the OS is showing, which is what [`ThemePreference::System`] follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAppearance {
    Light,
    Dark,
}

impl From<WindowAppearance> for SystemAppearance {
    fn from(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
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
            // `bg-active`, `ink-100..300`, and a vivid coral accent. The trunk lane
            // is intentionally decoupled from the accent — the accent is
            // reserved for the working copy, selection, and focus, while
            // the graph rides on a violet base so coral doesn't fight the
            // green/red diff signals.
            Self::Dark => ThemeSpec {
                background: rgb(0.043, 0.051, 0.071),
                panel_background: rgb(0.067, 0.078, 0.106),
                panel_background_elevated: rgb(0.094, 0.110, 0.145),
                selected_file: rgb(0.160, 0.178, 0.255),
                text: rgb(0.902, 0.910, 0.933),
                muted_text: rgb(0.690, 0.710, 0.761),
                subtle_text: rgb(0.563, 0.584, 0.643),
                accent: rgb(1.000, 0.400, 0.245),
                added_line: rgba(0.263, 0.843, 0.463, 0.11),
                removed_line: rgba(1.000, 0.408, 0.349, 0.10),
                added_text: rgb(0.263, 0.843, 0.463),
                removed_text: rgb(1.000, 0.408, 0.349),
                modified_token: rgb(1.000, 0.706, 0.220),
                info: rgb(0.310, 0.655, 1.000),
                // A step *lighter* than the elevated code surface so the
                // strip reads as a raised header rather than a hole cut
                // into the scroll body.
                file_header: rgb(0.122, 0.141, 0.184),
                hunk_header: rgba(0.310, 0.655, 1.000, 0.12),
                conflict_marker: rgb(1.000, 0.408, 0.443),
                border: rgb(0.137, 0.153, 0.196),
                note_background: rgba(1.000, 0.706, 0.220, 0.17),
                note_text: rgb(1.000, 0.706, 0.220),
                lane_base: rgb(0.690, 0.510, 1.000),
                syntax_keyword: rgb(0.760, 0.610, 1.000),
                syntax_type: rgb(0.260, 0.840, 0.755),
                syntax_function: rgb(0.480, 0.725, 1.000),
                syntax_literal: rgb(1.000, 0.706, 0.220),
                syntax_property: rgb(0.460, 0.820, 0.930),
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
                background: rgb(0.957, 0.957, 0.961),
                panel_background: rgb(1.000, 1.000, 1.000),
                panel_background_elevated: rgb(0.976, 0.976, 0.980),
                selected_file: rgb(0.923, 0.910, 0.957),
                text: rgb(0.106, 0.114, 0.133),
                muted_text: rgb(0.314, 0.337, 0.380),
                subtle_text: rgb(0.392, 0.410, 0.451),
                accent: rgb(0.690, 0.160, 0.055),
                added_line: rgba(0.020, 0.410, 0.190, 0.10),
                removed_line: rgba(0.670, 0.130, 0.105, 0.09),
                added_text: rgb(0.020, 0.410, 0.190),
                removed_text: rgb(0.670, 0.130, 0.105),
                modified_token: rgb(0.490, 0.290, 0.000),
                info: rgb(0.050, 0.310, 0.700),
                // Same role as in the dark theme: distinct from the white
                // code surface so file headers stand out as the scroll
                // body slides past, but not so dark that it overpowers
                // the body content.
                file_header: rgb(0.949, 0.953, 0.961),
                hunk_header: rgba(0.050, 0.310, 0.700, 0.09),
                conflict_marker: rgb(0.690, 0.115, 0.150),
                border: rgb(0.882, 0.886, 0.898),
                note_background: rgba(0.490, 0.290, 0.000, 0.17),
                note_text: rgb(0.490, 0.290, 0.000),
                lane_base: rgb(0.430, 0.235, 0.890),
                syntax_keyword: rgb(0.420, 0.145, 0.850),
                syntax_type: rgb(0.000, 0.405, 0.350),
                syntax_function: rgb(0.070, 0.315, 0.690),
                syntax_literal: rgb(0.490, 0.290, 0.000),
                syntax_property: rgb(0.000, 0.365, 0.540),
            },
            Self::HighContrast => ThemeSpec {
                background: BLACK,
                panel_background: rgb(0.030, 0.030, 0.030),
                panel_background_elevated: rgb(0.070, 0.070, 0.070),
                selected_file: rgb(0.000, 0.180, 0.240),
                text: WHITE,
                muted_text: rgb(0.780, 0.820, 0.840),
                subtle_text: rgb(0.620, 0.660, 0.690),
                accent: rgb(1.000, 0.478, 0.349),
                added_line: rgb(0.000, 0.235, 0.080),
                removed_line: rgb(0.300, 0.000, 0.045),
                added_text: rgb(0.500, 1.000, 0.600),
                removed_text: rgb(1.000, 0.520, 0.560),
                modified_token: rgb(1.000, 0.920, 0.000),
                info: rgb(0.380, 0.770, 1.000),
                file_header: rgb(0.120, 0.120, 0.120),
                hunk_header: rgb(0.000, 0.220, 0.310),
                conflict_marker: rgb(1.000, 0.445, 0.445),
                border: rgb(0.570, 0.620, 0.660),
                note_background: rgb(0.260, 0.210, 0.000),
                note_text: rgb(1.000, 0.940, 0.500),
                lane_base: rgb(0.760, 0.620, 1.000),
                syntax_keyword: rgb(0.870, 0.740, 1.000),
                syntax_type: rgb(0.400, 1.000, 0.920),
                syntax_function: rgb(0.600, 0.840, 1.000),
                syntax_literal: rgb(1.000, 0.880, 0.420),
                syntax_property: rgb(0.720, 0.920, 1.000),
            },
        }
    }

    fn kit_mode(self) -> ThemeMode {
        match self {
            Self::Light => ThemeMode::Light,
            Self::Dark | Self::HighContrast => ThemeMode::Dark,
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
            a: 0.32,
            ..theme.added_text
        },
        deletion_emphasis: Color {
            a: 0.32,
            ..theme.removed_text
        },
        note_background: theme.note_background,
        gutter_background: theme.panel_background,
        border: theme.border,
        // Translucent accent so the underlying syntax-highlighted text and
        // line-change tints stay readable under the selection. Alpha tuned
        // to match the design's accent wash — strong enough to
        // clearly mark the selection, soft enough to keep the diff colors
        // legible behind it.
        selection: Color {
            a: 0.20,
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

/// Translucent chip background derived from the chip's text color. This way
/// the chip reads independently of whether the row is selected — its visual
/// frame comes from the tint rather than from the row's solid background.
///
/// The tint stays translucent enough for selected rows to read through it,
/// while carrying enough chroma to make status easy to scan.
pub fn chip_background(color: Color) -> Color {
    Color { a: 0.18, ..color }
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
    /// The welcome screen's wordmark.
    pub const DISPLAY: f32 = 40.0;
}

/// Shared spatial rhythm. Components can make small optical corrections, but
/// their structural gaps and insets should come from this scale.
pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 6.0;
    pub const MD: f32 = 8.0;
    pub const LG: f32 = 12.0;
    pub const XL: f32 = 16.0;
    pub const XXL: f32 = 24.0;
}

/// App-wide control geometry. Fixed heights keep adjacent controls aligned
/// even when their labels use different fonts.
pub mod control {
    pub const COMPACT: f32 = 28.0;
    pub const STANDARD: f32 = 32.0;
}

/// Corner radii shared across the chrome. Chips keep their own tighter
/// rounding ([`crate::chip::RADIUS`]).
pub mod radius {
    /// Inputs, hover washes, list/menu rows.
    pub const CONTROL: f32 = 7.0;
    /// Every button-shaped control (ghost, raised, primary, dialog) and
    /// the toolbar's segmented well — one rounding for everything that
    /// reads as a button.
    pub const BUTTON: f32 = 7.0;
    /// Inset row cards (revision/file selection).
    pub const PUSH: f32 = 8.0;
    /// Floating cards: popovers, modals, and tooltips.
    pub const SURFACE: f32 = 12.0;
}

/// Blend `a` toward `b` by `t` (0..=1), ignoring alpha. Used to derive the
/// raised buttons' bezel stops from surface tokens without growing the spec,
/// and the sidebar's target-mode wash.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

fn raised_fill(theme: ThemeSpec, t: f32) -> Color {
    mix(theme.panel_background_elevated, theme.selected_file, t)
}

/// Fill for recessed wells (the toolbar's Diff/Source switcher): the
/// window canvas pushed a step darker, so the well clearly sinks below
/// both the canvas and the elevated bar it sits on — recessed surfaces
/// deepen in both dark and light themes.
pub fn well_fill(theme: ThemeSpec) -> Color {
    mix(theme.background, BLACK, 0.06)
}

/// Colours of a button in each state. Geometry lives in [`crate::ui`].
#[derive(Debug, Clone, Copy)]
pub struct ButtonStyle {
    pub rest: Option<Color>,
    pub hover: Color,
    pub active: Color,
    pub label: Color,
    pub border: Option<Color>,
}

/// Ghost button: invisible at rest, translucent wash on hover and press. The
/// wash is [`chip_background`] of `muted_text`, so it reads the same on panel
/// and elevated backgrounds. For icon-only and small-label controls embedded
/// in framed surfaces (tab strip, find card, popover footers); standalone
/// actions use [`raised_button_style`] so they stay visible at rest.
pub fn ghost_button_style(theme: ThemeSpec) -> ButtonStyle {
    let wash = chip_background(theme.muted_text);
    ButtonStyle {
        rest: None,
        hover: wash,
        active: wash,
        label: theme.text,
        border: None,
    }
}

/// The standard button: a flat fill one step above its surface, a crisp 1px
/// border. Hover and press deepen the fill without moving the control or
/// adding elevation.
pub fn raised_button_style(theme: ThemeSpec) -> ButtonStyle {
    ButtonStyle {
        rest: Some(raised_fill(theme, 0.35)),
        hover: raised_fill(theme, 0.65),
        active: raised_fill(theme, 0.9),
        label: theme.text,
        border: Some(theme.border),
    }
}

/// Primary call-to-action: informational blue fill with an inverted label.
/// Keeping actions blue leaves coral for selection and working-copy state,
/// away from destructive red.
pub fn primary_button_style(theme: ThemeSpec) -> ButtonStyle {
    ButtonStyle {
        rest: Some(theme.info),
        hover: mix(theme.info, theme.text, 0.08),
        active: mix(theme.info, theme.background, 0.12),
        label: theme.background,
        border: None,
    }
}

/// The primary button while it can't be pressed.
pub fn disabled_button_style(theme: ThemeSpec) -> ButtonStyle {
    let fill = raised_fill(theme, 0.2);
    ButtonStyle {
        rest: Some(fill),
        hover: fill,
        active: fill,
        label: theme.subtle_text,
        border: Some(theme.border),
    }
}

/// Destructive call-to-action for confirm dialogs: flat red fill so the
/// dangerous choice is unmistakable next to the neutral cancel button.
pub fn destructive_button_style(theme: ThemeSpec) -> ButtonStyle {
    ButtonStyle {
        rest: Some(theme.removed_text),
        hover: theme.removed_text,
        active: theme.removed_text,
        label: theme.background,
        border: None,
    }
}

/// The drop shadow every floating surface shares: same tint and alpha
/// everywhere, geometry scaled to the surface (popovers are bigger than
/// tooltips).
pub fn floating_shadow(offset_y: f32, blur_radius: f32) -> BoxShadow {
    BoxShadow {
        color: Hsla::from(Color { a: 0.05, ..BLACK }),
        offset: point(px(0.0), px(offset_y)),
        blur_radius: px(blur_radius),
        spread_radius: px(0.0),
        inset: false,
    }
}

/// Floating card anchored to a trigger with nothing dimming the content behind
/// it (dropdown menus, the activity popover). The heavy drop shadow is what
/// separates it from the busy panel underneath.
pub fn popover<E: Styled>(element: E, theme: ThemeSpec) -> E {
    element
        .bg(theme.panel_background_elevated)
        .border_1()
        .border_color(theme.border)
        .rounded(px(radius::SURFACE))
        .shadow(vec![floating_shadow(8.0, 16.0)])
}

/// Centered card over a scrim (confirm dialog, open-repo dialog, command
/// palette). The scrim already darkens what's behind the modal, so a heavy
/// shadow would just look like double dimming: soft, just enough to suggest
/// the card is lifted.
pub fn modal_card<E: Styled>(element: E, theme: ThemeSpec) -> E {
    element
        .bg(theme.panel_background_elevated)
        .border_1()
        .border_color(theme.border)
        .rounded(px(radius::SURFACE))
        .shadow(vec![floating_shadow(4.0, 12.0)])
}

/// Dimming layer behind modal surfaces. One alpha everywhere so stacked
/// takeovers (confirm dialog, open-repo dialog, command palette) feel equally
/// deep.
pub const SCRIM: Color = Color { a: 0.40, ..BLACK };

/// Text-selection tint in an input.
pub fn input_selection(theme: ThemeSpec) -> Color {
    Color {
        a: 0.25,
        ..theme.accent
    }
}

/// Hover tooltip card.
pub fn tooltip_card<E: Styled>(element: E, theme: ThemeSpec) -> E {
    element
        .bg(theme.panel_background_elevated)
        .text_color(theme.text)
        .border_1()
        .border_color(theme.border)
        .rounded(px(radius::SURFACE))
}

/// Makes kit's theme speak diffui's palette, so a stock kit component (a
/// button variant, an input, a scrollbar, a popover) looks native here. Call
/// after any change to the resolved theme.
pub fn apply_to_kit(resolved: ResolvedTheme, cx: &mut App) {
    let spec = resolved.spec();
    Theme::change(resolved.kit_mode(), None, cx);
    let hsla = |color: Color| Hsla::from(color);
    Theme::update(cx, |theme| {
        let raised = raised_button_style(spec);
        let primary = primary_button_style(spec);
        let ghost = ghost_button_style(spec);
        let scroll = scrollbar_style(spec);
        let c = &mut theme.colors;
        c.background = hsla(spec.background).into();
        c.foreground = hsla(spec.text).into();
        c.border = hsla(spec.border).into();
        c.input = hsla(spec.border).into();
        c.ring = hsla(spec.info).into();
        c.muted = hsla(spec.panel_background_elevated).into();
        c.muted_foreground = hsla(spec.muted_text).into();
        c.popover = hsla(spec.panel_background_elevated).into();
        c.popover_foreground = hsla(spec.text).into();
        c.accent = hsla(ghost.hover).into();
        c.accent_foreground = hsla(spec.text).into();
        c.selection = hsla(input_selection(spec)).into();
        c.caret = hsla(spec.text).into();
        c.primary = hsla(primary.rest.unwrap_or(spec.info)).into();
        c.primary_hover = hsla(primary.hover).into();
        c.primary_active = hsla(primary.active).into();
        c.primary_foreground = hsla(primary.label).into();
        c.secondary = hsla(raised.rest.unwrap_or(spec.panel_background_elevated)).into();
        c.secondary_hover = hsla(raised.hover).into();
        c.secondary_active = hsla(raised.active).into();
        c.secondary_foreground = hsla(raised.label).into();
        c.danger = hsla(spec.removed_text).into();
        c.danger_hover = hsla(spec.removed_text).into();
        c.danger_active = hsla(spec.removed_text).into();
        c.danger_foreground = hsla(spec.background).into();
        c.scrollbar = hsla(TRANSPARENT).into();
        c.scrollbar_thumb = hsla(scroll.thumb_color).into();
        c.scrollbar_thumb_hover = hsla(scroll.thumb_color).into();
        c.list_hover = hsla(ghost.hover).into();
        c.list_active = hsla(spec.selected_file).into();
        c.tab_bar = hsla(spec.background).into();
    });
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
                    a: 0.20,
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
        pairs.push(("primary button label", spec.background, spec.info, TEXT));
        pairs.push((
            "destructive button label",
            spec.background,
            spec.removed_text,
            TEXT,
        ));
        // Names in dialog, toast and banner prose (`crate::prose`).
        for (label, color) in [
            ("revision id in prose", spec.accent),
            ("bookmark in prose", spec.lane_base),
        ] {
            for background in [spec.panel_background, spec.panel_background_elevated] {
                pairs.push((label, color, background, TEXT));
            }
        }
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

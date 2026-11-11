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

/// `base` at `weight`. CoreText resolves weights of the system font too.
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
            // Cool, slightly blue-biased neutrals with a cobalt accent. The
            // accent marks selection, focus, the working copy and primary
            // actions; everything else that carries colour means something
            // (add/delete/modify/rename, a graph lane). Panes and the code
            // surface sit a step above the chrome so hairlines, not fills,
            // separate them.
            Self::Dark => ThemeSpec {
                background: rgb(0.086, 0.094, 0.114),
                panel_background: rgb(0.067, 0.075, 0.090),
                panel_background_elevated: rgb(0.090, 0.102, 0.122),
                selected_file: rgb(0.137, 0.165, 0.243),
                text: rgb(0.910, 0.918, 0.933),
                muted_text: rgb(0.714, 0.737, 0.776),
                subtle_text: rgb(0.561, 0.588, 0.635),
                accent: rgb(0.525, 0.627, 0.980),
                added_line: rgba(0.365, 0.769, 0.545, 0.10),
                removed_line: rgba(0.957, 0.494, 0.514, 0.10),
                added_text: rgb(0.365, 0.769, 0.545),
                removed_text: rgb(0.957, 0.494, 0.514),
                modified_token: rgb(0.902, 0.659, 0.322),
                renamed: rgb(0.710, 0.592, 0.949),
                info: rgb(0.525, 0.627, 0.980),
                // One step above the code surface: the sticky file header
                // reads as a raised strip as the body scrolls under it.
                file_header: rgb(0.106, 0.118, 0.141),
                hunk_header: rgba(0.525, 0.627, 0.980, 0.12),
                conflict_marker: rgb(0.957, 0.494, 0.514),
                border: rgb(0.137, 0.149, 0.176),
                note_background: rgba(0.902, 0.659, 0.322, 0.13),
                note_text: rgb(0.902, 0.659, 0.322),
                change_id: rgb(0.949, 0.490, 0.733),
                lane_base: rgb(0.494, 0.608, 0.965),
                lanes: [
                    rgb(0.494, 0.608, 0.965),
                    rgb(0.310, 0.753, 0.682),
                    rgb(0.890, 0.635, 0.298),
                    rgb(0.878, 0.486, 0.725),
                ],
                syntax_keyword: rgb(0.760, 0.610, 1.000),
                syntax_type: rgb(0.260, 0.840, 0.755),
                syntax_function: rgb(0.480, 0.725, 1.000),
                syntax_literal: rgb(1.000, 0.706, 0.220),
                syntax_property: rgb(0.460, 0.820, 0.930),
            },
            // Every ink here is derived for a white ground rather than
            // mirrored from the dark spec: a colour that reads well against
            // near-black is nowhere near AA against near-white. Lightness is
            // set to clear 4.5:1 against the *darkest* surface each ink lands
            // on, usually the selected row or the accent wash, not the white
            // pane. `every_pairing_meets_wcag_aa` holds the line.
            Self::Light => ThemeSpec {
                background: rgb(0.953, 0.957, 0.965),
                panel_background: rgb(1.000, 1.000, 1.000),
                panel_background_elevated: rgb(1.000, 1.000, 1.000),
                selected_file: rgb(0.914, 0.933, 0.988),
                text: rgb(0.082, 0.090, 0.110),
                muted_text: rgb(0.267, 0.290, 0.337),
                subtle_text: rgb(0.388, 0.416, 0.467),
                accent: rgb(0.184, 0.310, 0.769),
                added_line: rgba(0.094, 0.478, 0.235, 0.08),
                removed_line: rgba(0.749, 0.161, 0.188, 0.07),
                added_text: rgb(0.075, 0.420, 0.200),
                removed_text: rgb(0.700, 0.140, 0.170),
                modified_token: rgb(0.560, 0.320, 0.000),
                renamed: rgb(0.439, 0.255, 0.761),
                info: rgb(0.184, 0.310, 0.769),
                file_header: rgb(0.969, 0.973, 0.980),
                hunk_header: rgba(0.184, 0.310, 0.769, 0.07),
                conflict_marker: rgb(0.700, 0.140, 0.170),
                border: rgb(0.894, 0.902, 0.922),
                note_background: rgba(0.560, 0.320, 0.000, 0.12),
                note_text: rgb(0.560, 0.320, 0.000),
                change_id: rgb(0.690, 0.141, 0.424),
                lane_base: rgb(0.290, 0.424, 0.831),
                lanes: [
                    rgb(0.290, 0.424, 0.831),
                    rgb(0.137, 0.537, 0.486),
                    rgb(0.737, 0.455, 0.078),
                    rgb(0.682, 0.290, 0.545),
                ],
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
                selected_file: rgb(0.050, 0.110, 0.300),
                text: WHITE,
                muted_text: rgb(0.780, 0.820, 0.840),
                subtle_text: rgb(0.620, 0.660, 0.690),
                accent: rgb(0.580, 0.700, 1.000),
                added_line: rgb(0.000, 0.235, 0.080),
                removed_line: rgb(0.300, 0.000, 0.045),
                added_text: rgb(0.500, 1.000, 0.600),
                removed_text: rgb(1.000, 0.520, 0.560),
                modified_token: rgb(1.000, 0.920, 0.000),
                renamed: rgb(0.830, 0.730, 1.000),
                info: rgb(0.580, 0.700, 1.000),
                file_header: rgb(0.120, 0.120, 0.120),
                hunk_header: rgb(0.000, 0.150, 0.330),
                conflict_marker: rgb(1.000, 0.445, 0.445),
                border: rgb(0.570, 0.620, 0.660),
                note_background: rgb(0.260, 0.210, 0.000),
                note_text: rgb(1.000, 0.940, 0.500),
                change_id: rgb(1.000, 0.560, 0.820),
                lane_base: rgb(0.580, 0.700, 1.000),
                lanes: [
                    rgb(0.580, 0.700, 1.000),
                    rgb(0.400, 0.950, 0.850),
                    rgb(1.000, 0.800, 0.400),
                    rgb(1.000, 0.600, 0.850),
                ],
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
    /// A renamed or copied file's status letter.
    pub renamed: Color,
    /// Informational ink: primary buttons, the hunk header's tint, file and
    /// command icons. The accent's hue, kept as its own token so a theme can
    /// split them.
    pub info: Color,
    pub file_header: Color,
    pub hunk_header: Color,
    pub conflict_marker: Color,
    pub border: Color,
    pub note_background: Color,
    pub note_text: Color,
    /// The highlighted shortest-unique prefix of a change id (jj prints it
    /// in magenta, so jj users already read it as "the id part").
    pub change_id: Color,
    /// Lane 0 of the revision graph, and the colour bookmark names take in
    /// prose. Always `lanes[0]`.
    pub lane_base: Color,
    /// The revision graph's lane colours, cycled by lane index. Four muted
    /// hues of like lightness: enough to tell adjacent branches apart
    /// without turning the gutter into a rainbow.
    pub lanes: [Color; 4],
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
        accent: theme.accent,
    }
}

/// The scrollbar thumb, kit's and so every bar's.
pub fn scrollbar_thumb(theme: ThemeSpec) -> Color {
    Color {
        a: 0.55,
        ..theme.muted_text
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

/// Ink for a file's status letter: A green, M amber, D red, R violet. The
/// badge's tint is derived from it via `chip_background`, so the letter and
/// its fill share a hue.
pub fn file_status_color(status: diffui_core::DiffFileStatus, theme: ThemeSpec) -> Color {
    use diffui_core::DiffFileStatus;
    match status {
        DiffFileStatus::Added => theme.added_text,
        DiffFileStatus::Deleted => theme.removed_text,
        DiffFileStatus::Modified => theme.modified_token,
        DiffFileStatus::Renamed => theme.renamed,
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
    /// Chrome controls and content runs: buttons, tabs, menu and list rows,
    /// inputs, ids, dialog body. macOS's regular control size
    /// (`NSFont.systemFontSize`).
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
    /// A row's and a button's text inset.
    pub const MD_LG: f32 = 10.0;
    pub const LG: f32 = 12.0;
    pub const XL: f32 = 16.0;
    /// A dialog's inset around its content.
    pub const XL_XXL: f32 = 20.0;
    pub const XXL: f32 = 24.0;
}

/// The 1px line that divides panes, bars and sections. Layout that sums
/// heights counts it with this.
pub const HAIRLINE: f32 = 1.0;

/// App-wide control geometry. Fixed heights keep adjacent controls aligned
/// even when their labels use different fonts.
pub mod control {
    /// Close and clear buttons, small toggles inside a row.
    pub const SMALL: f32 = 22.0;
    pub const COMPACT: f32 = 28.0;
    pub const STANDARD: f32 = 32.0;
}

/// Glyph sizes. Lucide's 24px grid carries about 2px of padding, so a glyph
/// reads a step smaller than text of the same size.
pub mod icon {
    /// Chevrons, carets and glyphs inside small controls.
    pub const SM: f32 = 12.0;
    /// A glyph leading a label or filling an icon button.
    pub const MD: f32 = 14.0;
    /// Empty states and standalone marks.
    pub const LG: f32 = 16.0;
}

/// Height of every pane header: the side panes' field rows, the History
/// lists' title rows and the commit bar over the diff. The toolbar's height
/// too, so the panes' first lines meet the chrome on one rhythm.
pub const PANE_HEADER_HEIGHT: f32 = 40.0;

/// Corner radii. One scale for the whole app: pick the step by the size of
/// the thing being rounded, so a 20px chip and a 400px card read as the same
/// family. The named roles below are the steps the chrome reaches for.
pub mod radius {
    pub const XS: f32 = 6.0;
    pub const SM: f32 = 10.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 16.0;
    pub const XL: f32 = 20.0;

    /// Chips, badges, keycaps, small icon buttons, short list rows.
    pub const CHIP: f32 = XS;
    /// Buttons, inputs, hover washes, list and menu rows, segmented wells.
    pub const CONTROL: f32 = SM;
    /// Revision and history rows, inner cards and dropdowns.
    pub const CARD: f32 = MD;
    /// Floating cards anchored to a trigger: popovers, menus.
    pub const POPOVER: f32 = LG;
    /// Modals: dialogs, the palette.
    pub const MODAL: f32 = XL;

    /// The radius of a box `inset` px inside one rounded by `outer`, so the
    /// two corners stay concentric.
    pub const fn inner(outer: f32, inset: f32) -> f32 {
        let radius = outer - inset;
        if radius > 0.0 { radius } else { 0.0 }
    }
}

/// Lists: every row shell, scroll list and bespoke list (the revision list,
/// the file trees, the diff) reads its geometry and washes from here.
pub mod list {
    use super::{Color, ThemeSpec, space};

    /// A modal list's inset from its card (the palette, the open-repo dialog,
    /// the activity popover).
    pub const INSET: f32 = space::SM;
    /// A dropdown's or a menu's list inset from its card.
    pub const INSET_TIGHT: f32 = space::XS;
    /// A row's text inset from its wash, in a modal list.
    pub const PAD_X: f32 = space::MD_LG;
    /// The same in a dropdown or a menu.
    pub const PAD_X_TIGHT: f32 = space::MD;
    /// A row's text inset from a list's right edge where the scrollbar runs
    /// over the rows: the bar's footprint.
    pub const PAD_END: f32 = crate::scrollbar::GUTTER;
    /// How far the fade under a pane's header reaches into its list.
    pub const TOP_FADE: f32 = 28.0;

    /// Row heights in use: one step per kind of row.
    pub mod height {
        /// File tree rows.
        pub const FILE: f32 = 26.0;
        /// Menu rows, dropdown completions, op log snapshot runs.
        pub const MENU: f32 = 28.0;
        /// An operation's changed commits.
        pub const CHANGE: f32 = 32.0;
        /// Palette and command line results.
        pub const RESULT: f32 = 34.0;
        /// Open-repo recents and folders.
        pub const DIALOG: f32 = 36.0;
        /// Revisions and operations: two lines.
        pub const REVISION: f32 = 46.0;
    }

    /// The corner of a row `height` tall: rounder rows for taller ones, never
    /// a capsule.
    pub const fn radius(height: f32) -> f32 {
        if height < 28.0 {
            super::radius::XS
        } else if height <= 36.0 {
            super::radius::SM
        } else {
            super::radius::MD
        }
    }

    /// A row under the cursor.
    pub fn row_hover(theme: ThemeSpec) -> Color {
        super::hover_wash(theme)
    }

    /// The selected or highlighted row.
    pub fn row_selected(theme: ThemeSpec) -> Color {
        theme.selected_file
    }
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

/// The window chrome's one fill: the tab strip, the toolbar and the status
/// bar. The window canvas a step darker, so the panes read as raised above it.
pub fn chrome_fill(theme: ThemeSpec) -> Color {
    mix(theme.background, BLACK, 0.06)
}

/// The wash a quiet control takes under the cursor, and a row's hover: a
/// faint veil of the text colour, so it reads on the chrome, the panes and
/// the cards alike.
pub fn hover_wash(theme: ThemeSpec) -> Color {
    Color {
        a: 0.06,
        ..theme.text
    }
}

/// A recessed well inside a surface (the segmented controls' track, a
/// progress track) and a neutral badge's fill: a step under
/// [`press_wash`], so a pressed control in it still reads.
pub fn well_wash(theme: ThemeSpec) -> Color {
    Color {
        a: 0.08,
        ..theme.text
    }
}

/// [`hover_wash`] while pressed, or while what it opens is open.
pub fn press_wash(theme: ThemeSpec) -> Color {
    Color {
        a: 0.10,
        ..theme.text
    }
}

/// Colours of a button in each state. Geometry lives in [`crate::ui`].
#[derive(Debug, Clone)]
pub struct ButtonStyle {
    pub rest: Option<Color>,
    pub hover: Color,
    pub active: Color,
    pub label: Color,
    /// The button's 1px edge, inside its box and following its squircle. An
    /// edge never takes room, so a button keeps its size when it changes
    /// style (a Save that turns primary once there's something to save).
    pub ring: Option<Color>,
    /// The soft drop under a button that stands off its surface.
    pub lift: Option<BoxShadow>,
}

/// Ghost button: invisible at rest, [`hover_wash`] on hover and
/// [`press_wash`] on press, its label a step quieter than body text. For
/// controls on the chrome and in bars and cards; standalone actions use
/// [`raised_button_style`] so they stay visible at rest.
pub fn ghost_button_style(theme: ThemeSpec) -> ButtonStyle {
    ButtonStyle {
        rest: None,
        hover: hover_wash(theme),
        active: press_wash(theme),
        label: theme.muted_text,
        ring: None,
        lift: None,
    }
}

/// The standard button: the panes' raised surface with a firm hairline ring
/// and a faint lift. Hover and press deepen the fill without moving it.
pub fn raised_button_style(theme: ThemeSpec) -> ButtonStyle {
    ButtonStyle {
        rest: Some(theme.panel_background_elevated),
        hover: mix(theme.panel_background_elevated, theme.text, 0.035),
        active: mix(theme.panel_background_elevated, theme.text, 0.07),
        label: theme.text,
        ring: Some(firm_ring(theme)),
        lift: Some(lift(0.05, 1.0)),
    }
}

/// A faint drop one pixel under a control.
fn lift(alpha: f32, blur: f32) -> BoxShadow {
    BoxShadow {
        color: Hsla::from(Color { a: alpha, ..BLACK }),
        offset: point(px(0.0), px(1.0)),
        blur_radius: px(blur),
        spread_radius: px(0.0),
        inset: false,
    }
}

/// The hairline of a raised control, a step firmer than the panes' borders.
pub fn firm_ring(theme: ThemeSpec) -> Color {
    mix(theme.border, theme.subtle_text, 0.25)
}

/// Primary call-to-action: the accent, with an inverted label, a darker inner
/// ring and a faint lift, well away from the destructive red.
pub fn primary_button_style(theme: ThemeSpec) -> ButtonStyle {
    ButtonStyle {
        rest: Some(theme.accent),
        hover: mix(theme.accent, theme.text, 0.10),
        active: mix(theme.accent, theme.text, 0.18),
        label: theme.background,
        ring: Some(Color { a: 0.10, ..BLACK }),
        lift: Some(lift(0.08, 1.0)),
    }
}

/// The primary button while it can't be pressed: grey instead of the accent.
/// The button fades the label itself ([`crate::ui::Button::disabled`]).
pub fn disabled_button_style(theme: ThemeSpec) -> ButtonStyle {
    let fill = raised_fill(theme, 0.2);
    ButtonStyle {
        rest: Some(fill),
        hover: fill,
        active: fill,
        label: theme.text,
        ring: Some(theme.border),
        lift: None,
    }
}

/// A 1px edge drawn inside a box. Unlike a border it takes no room, so a
/// control that gains or loses its edge with its state keeps its size.
pub fn inset_ring(color: Color) -> BoxShadow {
    BoxShadow {
        color: Hsla::from(color),
        offset: point(px(0.0), px(0.0)),
        blur_radius: px(0.0),
        spread_radius: px(1.0),
        inset: true,
    }
}

/// Destructive call-to-action for confirm dialogs: flat red fill so the
/// dangerous choice is unmistakable next to the neutral cancel button.
pub fn destructive_button_style(theme: ThemeSpec) -> ButtonStyle {
    ButtonStyle {
        rest: Some(theme.removed_text),
        hover: mix(theme.removed_text, theme.text, 0.10),
        active: mix(theme.removed_text, theme.text, 0.18),
        label: theme.background,
        ring: Some(Color { a: 0.10, ..BLACK }),
        lift: Some(lift(0.08, 1.0)),
    }
}

/// The soft drop under a control raised out of its track (the active tab, the
/// active segment). Its edge is the squircle's ring, in [`firm_ring`].
pub fn raised_lift() -> BoxShadow {
    lift(0.10, 2.0)
}

/// Gives `element` a squircle of `radius`: filled with `fill`, and edged in
/// `border` 1px inside its box where a border would be (the element keeps a
/// transparent border for its layout). Call it before adding children: the
/// squircle is the first.
pub fn squircle<E: Styled + gpui_kit::ParentElement>(
    element: E,
    radius: f32,
    fill: Option<Color>,
    border: Option<Color>,
) -> E {
    use crate::squircle::{Corners, Fill, background};
    let mut shape = Fill::new(Corners::all(radius));
    if let Some(fill) = fill {
        shape = shape.background(fill);
    }
    let element = element.rounded(px(radius));
    match border {
        Some(border) => element
            .border_1()
            .border_color(TRANSPARENT)
            .child(background(shape.border(1.0, border), 1.0)),
        None => element.child(background(shape, 0.0)),
    }
}

/// Raises `element` out of its track (the active tab, the active segment): the
/// panes' surface in a squircle of `radius` with a [`firm_ring`] edge, and a
/// soft drop. Call it before adding children: the squircle is the first.
pub fn raised<E: Styled + gpui_kit::ParentElement>(element: E, radius: f32, theme: ThemeSpec) -> E {
    use crate::squircle::{Corners, Fill, background};
    element
        .rounded(px(radius))
        .shadow(vec![raised_lift()])
        .child(background(
            Fill::new(Corners::all(radius))
                .background(theme.panel_background_elevated)
                .border(1.0, firm_ring(theme)),
            0.0,
        ))
}

/// How far a floating surface stands off what's under it. Each step owns its
/// corner, edge and drop shadow, so every surface at a step reads the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Hover labels.
    Tooltip,
    /// Cards anchored to a trigger with nothing dimming what's behind them:
    /// menus, popovers, the find bar, toasts.
    Popover,
    /// Cards over the scrim: dialogs, the palette.
    Modal,
}

impl Elevation {
    pub fn radius(self) -> f32 {
        match self {
            Elevation::Tooltip => radius::XS,
            Elevation::Popover => radius::POPOVER,
            Elevation::Modal => radius::MODAL,
        }
    }

    /// The drop under a surface at this step. Black on a dark canvas needs
    /// far more alpha than on a light one to show at all.
    pub fn shadow(self, theme: ThemeSpec) -> BoxShadow {
        let dark = is_dark(theme);
        let (offset_y, blur, light_alpha, dark_alpha) = match self {
            Elevation::Tooltip => (2.0, 8.0, 0.14, 0.40),
            Elevation::Popover => (8.0, 24.0, 0.20, 0.55),
            Elevation::Modal => (12.0, 32.0, 0.28, 0.70),
        };
        BoxShadow {
            color: Hsla::from(Color {
                a: if dark { dark_alpha } else { light_alpha },
                ..BLACK
            }),
            offset: point(px(0.0), px(offset_y)),
            blur_radius: px(blur),
            spread_radius: px(0.0),
            inset: false,
        }
    }
}

/// Whether `theme` draws light ink on a dark canvas.
fn is_dark(theme: ThemeSpec) -> bool {
    let canvas = theme.background;
    canvas.r + canvas.g + canvas.b < 1.5
}

/// A floating surface at `elevation`: the raised fill and the border's edge in
/// a squircle, and the step's drop shadow. Add its content after this: its
/// squircle is the card's first child.
pub fn surface<E: Styled + gpui_kit::ParentElement>(
    element: E,
    elevation: Elevation,
    theme: ThemeSpec,
) -> E {
    surface_with(element, elevation, elevation.radius(), theme.border, theme)
}

/// [`surface`] with its own corner and edge: the palette rings the column that
/// has the keyboard, the revset dropdown is a compact card with a firmer edge.
pub fn surface_with<E: Styled + gpui_kit::ParentElement>(
    element: E,
    elevation: Elevation,
    radius: f32,
    edge: Color,
    theme: ThemeSpec,
) -> E {
    use crate::squircle::{Corners, Fill, background};
    // The transparent border keeps the card's layout and lets an
    // overflow-hidden card clip to its whole box, edge included. The shadow
    // keeps the circular corner GPUI shadows have, which the blur hides.
    element
        .border_1()
        .border_color(TRANSPARENT)
        .rounded(px(radius))
        .shadow(vec![elevation.shadow(theme)])
        .child(background(
            Fill::new(Corners::all(radius))
                .background(theme.panel_background_elevated)
                .border(1.0, edge),
            1.0,
        ))
}

/// Dimming layer behind modal surfaces. One alpha everywhere so stacked
/// takeovers (confirm dialog, open-repo dialog, command palette) feel equally
/// deep.
pub const SCRIM: Color = rgba(0.0, 0.0, 0.0, 0.40);

/// Text-selection tint in an input.
pub fn input_selection(theme: ThemeSpec) -> Color {
    Color {
        a: 0.25,
        ..theme.accent
    }
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
        let thumb = scrollbar_thumb(spec);
        let c = &mut theme.colors;
        c.background = hsla(spec.background);
        c.foreground = hsla(spec.text);
        c.border = hsla(spec.border);
        c.input = hsla(spec.border);
        c.ring = hsla(spec.info);
        c.muted = hsla(spec.panel_background_elevated);
        c.muted_foreground = hsla(spec.muted_text);
        c.popover = hsla(spec.panel_background_elevated);
        c.popover_foreground = hsla(spec.text);
        c.accent = hsla(ghost.hover);
        c.accent_foreground = hsla(spec.text);
        c.selection = hsla(input_selection(spec));
        c.caret = hsla(spec.text);
        c.primary = hsla(primary.rest.unwrap_or(spec.info));
        c.primary_hover = hsla(primary.hover);
        c.primary_active = hsla(primary.active);
        c.primary_foreground = hsla(primary.label);
        c.secondary = hsla(raised.rest.unwrap_or(spec.panel_background_elevated));
        c.secondary_hover = hsla(raised.hover);
        c.secondary_active = hsla(raised.active);
        c.secondary_foreground = hsla(raised.label);
        let destructive = destructive_button_style(spec);
        c.danger = hsla(spec.removed_text);
        c.danger_hover = hsla(destructive.hover);
        c.danger_active = hsla(destructive.active);
        c.danger_foreground = hsla(spec.background);
        c.scrollbar = hsla(TRANSPARENT);
        c.scrollbar_thumb = hsla(thumb);
        c.scrollbar_thumb_hover = hsla(thumb);
        c.list_hover = hsla(ghost.hover);
        c.list_active = hsla(spec.selected_file);
        c.tab_bar = hsla(spec.background);
    });
    scrollbar::place_kit_bars(cx);
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
            ("modified chip", spec.modified_token),
            ("renamed chip", spec.renamed),
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
        for lane in spec.lanes {
            for base in [spec.panel_background, spec.selected_file] {
                pairs.push(("lane", lane, base, GLYPH));
            }
        }
        for background in [spec.panel_background, spec.selected_file] {
            pairs.push(("change id prefix", spec.change_id, background, TEXT));
        }
        // The revision rows' neutral ref chips, their `+N`, and the working
        // copy's label: pane-coloured text on the accent.
        for base in [spec.panel_background, spec.selected_file] {
            let fill = over(chip_background(spec.muted_text), base);
            pairs.push(("ref chip", spec.muted_text, fill, TEXT));
            pairs.push(("overflow chip", spec.subtle_text, fill, GLYPH));
        }
        pairs.push((
            "working copy label",
            spec.panel_background,
            spec.accent,
            TEXT,
        ));
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

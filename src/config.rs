//! User configuration loaded from `$XDG_CONFIG_HOME/diffui/config.toml`
//! (or `$HOME/.config/diffui/config.toml`).
//!
//! Configurable fields:
//!   ui_font              — family used for chrome, sidebar, file names, etc.
//!                          Falls back to the system default when unset.
//!   mono_font            — family used for code/IDs.
//!                          Falls back to "Menlo" on macOS and "Cascadia Code"
//!                          elsewhere.
//!   multi_click_ms       — max ms between clicks to count as a multi-click in
//!                          the diff view. Defaults to 350.
//!   theme                — color theme: System | Light | Dark | Contrast.
//!                          `System` follows the OS appearance. Defaults to
//!                          System.
//!   code_font_size       — px size of the code surfaces (diff view, source
//!                          browser, description editor). Defaults to 12.
//!   code_line_height     — row height of the diff/source grid as a multiple
//!                          of `code_font_size`. Defaults to 1.85.
//!   code_baseline_offset — px nudge of code text within its row; positive
//!                          moves it down. Defaults to 0.
//!
//! Kerning and OpenType feature toggles (ligatures, stylistic sets) aren't
//! configurable: the text stack (iced/cosmic-text) exposes no per-run feature
//! or letter-spacing control today.

use std::{env, path::PathBuf};

use iced::Font;
use serde::Deserialize;

use crate::theme::ThemePreference;

/// Default row-height factor of the code grid — 1.85 × the font size gives
/// the historical 22px rows at the default 12px code font.
pub const DEFAULT_CODE_LINE_HEIGHT: f32 = 1.85;

/// Typography of the code surfaces, resolved and clamped at load. One value
/// object so the diff view's metrics take the whole set at once.
#[derive(Debug, Clone, Copy)]
pub struct CodeTypography {
    /// Font size in px (`code_font_size`).
    pub size: f32,
    /// Row height as a multiple of `size` (`code_line_height`).
    pub line_height: f32,
    /// Vertical nudge of text within its row, px, positive = down
    /// (`code_baseline_offset`).
    pub baseline_offset: f32,
}

impl Default for CodeTypography {
    fn default() -> Self {
        Self {
            size: 12.0,
            line_height: DEFAULT_CODE_LINE_HEIGHT,
            baseline_offset: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AppConfig {
    pub ui_font: Font,
    pub mono_font: Font,
    pub multi_click_ms: u64,
    pub theme: ThemePreference,
    pub code_type: CodeTypography,
}

impl AppConfig {
    pub fn load() -> Self {
        let raw = read_config_file().unwrap_or_default();
        let defaults = CodeTypography::default();
        Self {
            ui_font: raw
                .ui_font
                .as_deref()
                .map(|name| resolve_font(name, Font::DEFAULT))
                .unwrap_or(Font::DEFAULT),
            mono_font: raw
                .mono_font
                .as_deref()
                .map(|name| resolve_font(name, default_mono_font()))
                .unwrap_or_else(default_mono_font),
            multi_click_ms: raw.multi_click_ms.unwrap_or(350),
            theme: raw
                .theme
                .as_deref()
                .and_then(theme_from_name)
                .unwrap_or(ThemePreference::System),
            // Clamped to keep a fat-fingered config renderable rather than
            // degenerate (zero-height rows, text nudged out of its row).
            code_type: CodeTypography {
                size: raw
                    .code_font_size
                    .map(|size| size.clamp(6.0, 32.0))
                    .unwrap_or(defaults.size),
                line_height: raw
                    .code_line_height
                    .map(|factor| factor.clamp(1.0, 3.0))
                    .unwrap_or(defaults.line_height),
                baseline_offset: raw
                    .code_baseline_offset
                    .map(|px| px.clamp(-8.0, 8.0))
                    .unwrap_or(defaults.baseline_offset),
            },
        }
    }
}

#[derive(Default, Debug, Deserialize)]
struct RawConfig {
    ui_font: Option<String>,
    mono_font: Option<String>,
    multi_click_ms: Option<u64>,
    theme: Option<String>,
    code_font_size: Option<f32>,
    code_line_height: Option<f32>,
    code_baseline_offset: Option<f32>,
}

/// Parse a config `theme` value. Case-insensitive; `Contrast` maps to the
/// high-contrast theme. Unknown values fall back to `System` (via the
/// `None` the caller unwraps).
fn theme_from_name(name: &str) -> Option<ThemePreference> {
    match name.trim().to_ascii_lowercase().as_str() {
        "system" => Some(ThemePreference::System),
        "light" => Some(ThemePreference::Light),
        "dark" => Some(ThemePreference::Dark),
        "contrast" | "highcontrast" | "high-contrast" => Some(ThemePreference::HighContrast),
        _ => None,
    }
}

fn read_config_file() -> Option<RawConfig> {
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&text).ok()
}

fn config_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(env::var("HOME").ok()?).join(".config")
    };
    Some(base.join("diffui").join("config.toml"))
}

fn font_from_name(name: &str) -> Font {
    // `Font::new` takes a `&'static str`; we have a runtime string, so
    // intentionally leak the family name. The config is loaded once at
    // startup, so the resulting heap usage is bounded.
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    Font::new(leaked)
}

/// Resolve a configured family name against the installed fonts, falling
/// back to `fallback` when it isn't there. A named family that the font
/// database can't resolve doesn't merely degrade: requesting a non-default
/// weight (the tab bar's `Medium`, via `theme::emphasis_font`) on a missing
/// family makes cosmic-text's fallback come up empty and the text renders
/// as invisible `.notdef` glyphs. Validating up front means a config shared
/// across machines just falls back to the platform default where the font
/// isn't installed.
fn resolve_font(name: &str, fallback: Font) -> Font {
    let name = name.trim();
    match installed_family(name) {
        Some(canonical) => font_from_name(&canonical),
        None => {
            eprintln!(
                "diffui: configured font family {name:?} is not installed; \
                 falling back to the system default"
            );
            fallback
        }
    }
}

/// Look `name` up in the same font database the renderer will use, returning
/// the canonical family string on a hit. Exact (case-insensitive) matches
/// win; failing that, a whitespace-insensitive match catches naming variance
/// like `InterVariable` vs `Inter Variable`. `None` means no installed face
/// carries the family.
fn installed_family(name: &str) -> Option<String> {
    let Ok(mut system) = iced::advanced::graphics::text::font_system().write() else {
        // Can't inspect the database — trust the config rather than override.
        return Some(name.to_owned());
    };
    let normalize = |s: &str| {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let target = normalize(name);
    let mut loose_hit = None;
    for face in system.raw().db().faces() {
        for (family, _) in &face.families {
            if family.eq_ignore_ascii_case(name) {
                return Some(family.clone());
            }
            if loose_hit.is_none() && normalize(family) == target {
                loose_hit = Some(family.clone());
            }
        }
    }
    loose_hit
}

fn default_mono_font() -> Font {
    // `Font::MONOSPACE` is the generic monospace family — cosmic-text routes
    // it through the platform's font matcher (fontconfig on Linux,
    // DirectWrite on Windows, CoreText on macOS), so users get whatever
    // their OS considers the default fixed-width font without us guessing
    // bundled names. Hardcoding "Menlo" / "Cascadia Code" was fragile:
    // Cascadia Code only ships on Windows 11+ and isn't on Linux at all.
    Font::MONOSPACE
}

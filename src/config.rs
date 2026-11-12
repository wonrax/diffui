//! User configuration, from two files in `$XDG_CONFIG_HOME/diffui/` (or
//! `$HOME/.config/diffui/`):
//!
//! - `config.toml` is the user's. The app never writes it, so it can be a
//!   read-only file a declarative setup (nix, home-manager) links in.
//! - `settings.toml` is the settings dialog's, written whole by the app.
//!
//! Both hold the settings below; `config.toml` wins per key, and a key it
//! sets is locked in the dialog. `[keys]` and `[[revset_presets]]` are read
//! from `config.toml` only.
//!
//! Configurable fields:
//!   ui_font              — family used for chrome, sidebar, file names, etc.
//!                          Falls back to the system default when unset.
//!   mono_font            — family used for code/IDs.
//!                          Falls back to the platform's default monospace
//!                          family.
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
//!   refresh              — Auto | Manual. Auto folds edits into `@` as they
//!                          land and updates a stale checkout on its own;
//!                          Manual leaves both to the Refresh button, which
//!                          shows when either is due. Defaults to Auto.
//!
//! Keyboard bindings live in a `[keys]` section; see [`crate::keymap`] for the
//! chord syntax and the per-context tables. Example:
//!
//! ```toml
//! [keys]
//! # applies in every context the command is valid in
//! "palette.toggle" = ["cmd+p", "f1"]
//!
//! [keys.base]
//! "file.next" = "n"          # replaces the default j / ↓
//! "selection.clear" = "none" # unbind
//! ```
//!
//! A file that fails to parse is ignored whole, leaving the other one in
//! force, and the app says where the error is (see [`AppConfig::load`]).
//!
//! The revset filter's preset menu (jj repositories) can be replaced with
//! `[[revset_presets]]` entries, listed in order under the repo's own
//! "Default" (its `revsets.log`). Without any, the built-in set is used:
//! Everything `all()`, Mine `mine()`, Current line `ancestors(@)`,
//! Conflicts `conflicts()`. Git repositories keep their own built-ins.
//!
//! ```toml
//! [[revset_presets]]
//! name = "My stack"
//! revset = "trunk()..@"
//!
//! [[revset_presets]]
//! name = "Recent"
//! revset = "ancestors(@, 30)"
//! ```
//!
//! Kerning and OpenType feature toggles (ligatures, stylistic sets) aren't
//! configurable: the text stack exposes no per-run feature or letter-spacing
//! control today.

use crate::app_files::{AppDir, app_file, write_atomic};
use crate::theme::Font;
use diffui_core::RefreshMode;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::env;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::keymap::RawKeys;
use crate::theme::ThemePreference;
use crate::typography::CodeTypography;

#[derive(Debug, Clone, Copy)]
pub struct AppConfig {
    pub ui_font: Font,
    pub mono_font: Font,
    pub multi_click_ms: u64,
    pub theme: ThemePreference,
    pub code_type: CodeTypography,
    pub refresh: RefreshMode,
    /// The settings `config.toml` sets, which `settings.toml` can't change.
    pub locked: Locked,
}

/// `code_font_size`'s range, px. Loading clamps to these ranges to keep a
/// fat-fingered config renderable rather than degenerate (zero-height rows,
/// text nudged out of its row); the settings dialog steps within them.
pub const CODE_FONT_SIZE: RangeInclusive<f32> = 6.0..=32.0;
/// `code_line_height`'s range, a multiple of the font size.
pub const CODE_LINE_HEIGHT: RangeInclusive<f32> = 1.0..=3.0;
/// `code_baseline_offset`'s range, px.
pub const CODE_BASELINE_OFFSET: RangeInclusive<f32> = -8.0..=8.0;

impl AppConfig {
    /// Read both files, returning the resolved settings, the raw `[keys]`
    /// section for [`crate::keymap::Keymap::build`] to layer over the
    /// registry's defaults, and the files as read (with why either was
    /// ignored, if it was).
    pub fn load() -> (Self, RawKeys, ConfigFiles) {
        Self::load_from(config_path(), settings_path())
    }

    fn load_from(
        config_path: Option<PathBuf>,
        settings_path: Option<PathBuf>,
    ) -> (Self, RawKeys, ConfigFiles) {
        // Each file stands alone: one that doesn't parse leaves every setting
        // it holds on what the other file or the default says, since a
        // partial read would apply half a file that was never meant to be
        // used that way.
        let (config, file_only, config_error) = match read_config_file(config_path.as_deref()) {
            Ok(read) => {
                let (config, file_only) = read.unwrap_or_default();
                (config, file_only, None)
            }
            Err(error) => {
                tracing::warn!("ignoring config file: {error}");
                (Scalars::default(), FileOnly::default(), Some(error))
            }
        };
        let (stored, settings_error) = match read_settings_file(settings_path.as_deref()) {
            Ok(read) => (read.unwrap_or_default(), None),
            Err(error) => {
                tracing::warn!("ignoring settings file: {error}");
                (Scalars::default(), Some(error))
            }
        };
        let resolved = Self::resolve(&config, &stored);
        if let Some(presets) = file_only.revset_presets {
            let _ = REVSET_PRESETS.set(presets);
        }
        let files = ConfigFiles {
            config_path,
            settings_path,
            stored,
            config_error,
            settings_error,
        };
        (resolved, file_only.keys, files)
    }

    /// The settings `config` and `settings` add up to: `config.toml` wins
    /// per key, and every key it sets is locked.
    fn resolve(config: &Scalars, settings: &Scalars) -> Self {
        let raw = config.over(settings);
        let defaults = CodeTypography::default();
        Self {
            ui_font: ui_font(raw.ui_font.as_deref()),
            mono_font: mono_font(raw.mono_font.as_deref()),
            multi_click_ms: raw.multi_click_ms.unwrap_or(350),
            theme: raw
                .theme
                .as_deref()
                .and_then(theme_from_name)
                .unwrap_or(ThemePreference::System),
            refresh: raw
                .refresh
                .as_deref()
                .and_then(refresh_from_name)
                .unwrap_or_default(),
            code_type: CodeTypography {
                size: raw
                    .code_font_size
                    .map(|size| clamp(size, CODE_FONT_SIZE))
                    .unwrap_or(defaults.size),
                line_height: raw
                    .code_line_height
                    .map(|factor| clamp(factor, CODE_LINE_HEIGHT))
                    .unwrap_or(defaults.line_height),
                baseline_offset: raw
                    .code_baseline_offset
                    .map(|px| clamp(px, CODE_BASELINE_OFFSET))
                    .unwrap_or(defaults.baseline_offset),
            },
            locked: config.set_keys(),
        }
    }
}

fn clamp(value: f64, range: RangeInclusive<f32>) -> f32 {
    (value as f32).clamp(*range.start(), *range.end())
}

/// A setting both files can hold, by the key it goes by in them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Theme,
    Refresh,
    UiFont,
    MonoFont,
    CodeFontSize,
    CodeLineHeight,
    CodeBaselineOffset,
    MultiClickMs,
}

impl Key {
    /// What the settings dialog and its toasts call it.
    pub fn label(self) -> &'static str {
        match self {
            Key::Theme => "Theme",
            Key::Refresh => "Refresh",
            Key::UiFont => "UI font",
            Key::MonoFont => "Code font",
            Key::CodeFontSize => "Code font size",
            Key::CodeLineHeight => "Line height",
            Key::CodeBaselineOffset => "Baseline offset",
            Key::MultiClickMs => "Double-click speed",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// A set of [`Key`]s: the ones `config.toml` sets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Locked(u8);

impl Locked {
    pub fn contains(self, key: Key) -> bool {
        self.0 & key.bit() != 0
    }

    fn insert(&mut self, key: Key) {
        self.0 |= key.bit();
    }

    #[cfg(test)]
    pub fn of(keys: &[Key]) -> Self {
        let mut set = Self::default();
        for key in keys {
            set.insert(*key);
        }
        set
    }
}

/// A new value for one setting, as the settings dialog and the palette ask
/// for it.
#[derive(Debug, Clone, PartialEq)]
pub enum Setting {
    Theme(ThemePreference),
    Refresh(RefreshMode),
    /// A family name; `None` is the platform default.
    UiFont(Option<String>),
    MonoFont(Option<String>),
    CodeFontSize(f32),
    CodeLineHeight(f32),
    CodeBaselineOffset(f32),
    MultiClickMs(u64),
}

impl Setting {
    pub fn key(&self) -> Key {
        match self {
            Setting::Theme(_) => Key::Theme,
            Setting::Refresh(_) => Key::Refresh,
            Setting::UiFont(_) => Key::UiFont,
            Setting::MonoFont(_) => Key::MonoFont,
            Setting::CodeFontSize(_) => Key::CodeFontSize,
            Setting::CodeLineHeight(_) => Key::CodeLineHeight,
            Setting::CodeBaselineOffset(_) => Key::CodeBaselineOffset,
            Setting::MultiClickMs(_) => Key::MultiClickMs,
        }
    }

    /// Put the value in `config`, resolved and clamped as loading would.
    /// Theme and refresh live elsewhere at runtime and are left alone.
    pub fn apply(&self, config: &mut AppConfig) {
        match self {
            Setting::Theme(_) | Setting::Refresh(_) => {}
            Setting::UiFont(name) => config.ui_font = ui_font(name.as_deref()),
            Setting::MonoFont(name) => config.mono_font = mono_font(name.as_deref()),
            Setting::CodeFontSize(size) => {
                config.code_type.size = clamp(f64::from(*size), CODE_FONT_SIZE);
            }
            Setting::CodeLineHeight(factor) => {
                config.code_type.line_height = clamp(f64::from(*factor), CODE_LINE_HEIGHT);
            }
            Setting::CodeBaselineOffset(px) => {
                config.code_type.baseline_offset = clamp(f64::from(*px), CODE_BASELINE_OFFSET);
            }
            Setting::MultiClickMs(ms) => config.multi_click_ms = *ms,
        }
    }

    fn store(&self, into: &mut Scalars) {
        // Two decimals: an `f32` widened as it is would be written out as
        // `1.850000023841858`.
        let decimal = |value: f32| (f64::from(value) * 100.0).round() / 100.0;
        match self {
            Setting::Theme(theme) => into.theme = Some(theme_name(*theme).to_owned()),
            Setting::Refresh(mode) => into.refresh = Some(refresh_name(*mode).to_owned()),
            Setting::UiFont(name) => into.ui_font = name.clone(),
            Setting::MonoFont(name) => into.mono_font = name.clone(),
            Setting::CodeFontSize(size) => into.code_font_size = Some(decimal(*size)),
            Setting::CodeLineHeight(factor) => into.code_line_height = Some(decimal(*factor)),
            Setting::CodeBaselineOffset(px) => into.code_baseline_offset = Some(decimal(*px)),
            Setting::MultiClickMs(ms) => into.multi_click_ms = Some(*ms),
        }
    }
}

/// One of the two files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFile {
    Config,
    Settings,
}

impl ConfigFile {
    pub fn name(self) -> &'static str {
        match self {
            ConfigFile::Config => "config.toml",
            ConfigFile::Settings => "settings.toml",
        }
    }
}

/// The two files, and what became of reading them.
#[derive(Debug, Default)]
pub struct ConfigFiles {
    /// The user's file. Never written: it may be a read-only store path.
    pub config_path: Option<PathBuf>,
    /// The settings dialog's file. `None` writes nowhere.
    pub settings_path: Option<PathBuf>,
    /// `settings.toml` as last read or written: what the next write keeps.
    stored: Scalars,
    /// Why `config.toml` was ignored, as `path:line:col: message`.
    pub config_error: Option<String>,
    /// Why `settings.toml` was ignored.
    pub settings_error: Option<String>,
}

impl ConfigFiles {
    pub fn path(&self, file: ConfigFile) -> Option<&Path> {
        match file {
            ConfigFile::Config => self.config_path.as_deref(),
            ConfigFile::Settings => self.settings_path.as_deref(),
        }
    }

    pub fn exists(&self, file: ConfigFile) -> bool {
        self.path(file).is_some_and(Path::exists)
    }

    /// Why `file` was ignored, if it was.
    pub fn error(&self, file: ConfigFile) -> Option<&str> {
        match file {
            ConfigFile::Config => self.config_error.as_deref(),
            ConfigFile::Settings => self.settings_error.as_deref(),
        }
    }

    /// Save `setting` to `settings.toml`, keeping what else it holds. The one
    /// way anything is written to either file. A file that didn't parse is
    /// replaced: the app owns it, and nothing in it was being used.
    pub fn write(&mut self, setting: &Setting) -> Result<(), String> {
        let Some(path) = &self.settings_path else {
            return Ok(());
        };
        let mut next = self.stored.clone();
        setting.store(&mut next);
        let body = toml::to_string(&next).map_err(|error| error.to_string())?;
        let text = format!("{SETTINGS_HEADER}{body}");
        write_atomic(path, text.as_bytes())
            .map_err(|error| format!("{}: {error}", display_path(path)))?;
        self.stored = next;
        self.settings_error = None;
        Ok(())
    }
}

const SETTINGS_HEADER: &str = "\
# Written by diffui's settings. A key set in config.toml wins over this file.
";

/// One `[[revset_presets]]` entry: a label for the menu and the revset it
/// applies.
#[derive(Debug, Clone, Deserialize)]
pub struct RevsetPreset {
    pub name: String,
    pub revset: String,
}

/// The configured presets, set once by [`AppConfig::load`]. Kept beside
/// `AppConfig` rather than in it so that type stays `Copy`.
static REVSET_PRESETS: OnceLock<Vec<RevsetPreset>> = OnceLock::new();

/// The `[[revset_presets]]` from `config.toml`, or `None` when the file has
/// none (use the built-ins). An empty list is a choice: no presets.
pub fn revset_presets() -> Option<&'static [RevsetPreset]> {
    REVSET_PRESETS.get().map(Vec::as_slice)
}

/// The keys both files can hold, each `None` when the file leaves it out.
#[derive(Default, Debug, Clone, PartialEq, Deserialize, Serialize)]
struct Scalars {
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ui_font: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mono_font: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_font_size: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_line_height: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_baseline_offset: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    multi_click_ms: Option<u64>,
}

impl Scalars {
    /// Each key from `self`, else from `under`.
    fn over(&self, under: &Scalars) -> Scalars {
        Scalars {
            theme: self.theme.clone().or_else(|| under.theme.clone()),
            refresh: self.refresh.clone().or_else(|| under.refresh.clone()),
            ui_font: self.ui_font.clone().or_else(|| under.ui_font.clone()),
            mono_font: self.mono_font.clone().or_else(|| under.mono_font.clone()),
            code_font_size: self.code_font_size.or(under.code_font_size),
            code_line_height: self.code_line_height.or(under.code_line_height),
            code_baseline_offset: self.code_baseline_offset.or(under.code_baseline_offset),
            multi_click_ms: self.multi_click_ms.or(under.multi_click_ms),
        }
    }

    /// The keys this file sets.
    fn set_keys(&self) -> Locked {
        let mut set = Locked::default();
        for (key, present) in [
            (Key::Theme, self.theme.is_some()),
            (Key::Refresh, self.refresh.is_some()),
            (Key::UiFont, self.ui_font.is_some()),
            (Key::MonoFont, self.mono_font.is_some()),
            (Key::CodeFontSize, self.code_font_size.is_some()),
            (Key::CodeLineHeight, self.code_line_height.is_some()),
            (Key::CodeBaselineOffset, self.code_baseline_offset.is_some()),
            (Key::MultiClickMs, self.multi_click_ms.is_some()),
        ] {
            if present {
                set.insert(key);
            }
        }
        set
    }
}

/// What only `config.toml` holds.
#[derive(Default, Debug, Deserialize)]
struct FileOnly {
    #[serde(default)]
    keys: RawKeys,
    revset_presets: Option<Vec<RevsetPreset>>,
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

/// Parse a config `refresh` value, case-insensitively. Unknown values fall
/// back to `Auto` (via the `None` the caller unwraps).
fn refresh_from_name(name: &str) -> Option<RefreshMode> {
    match name.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(RefreshMode::Auto),
        "manual" => Some(RefreshMode::Manual),
        _ => None,
    }
}

/// How `settings.toml` spells `theme`, the way the docs above do.
fn theme_name(theme: ThemePreference) -> &'static str {
    match theme {
        ThemePreference::System => "System",
        ThemePreference::Light => "Light",
        ThemePreference::Dark => "Dark",
        ThemePreference::HighContrast => "Contrast",
    }
}

fn refresh_name(mode: RefreshMode) -> &'static str {
    match mode {
        RefreshMode::Auto => "Auto",
        RefreshMode::Manual => "Manual",
    }
}

/// `config.toml`'s settings and what only it holds. `Ok(None)` when there is
/// no file; `Err` with a `path:line:col: message` description when there is
/// one that can't be read or parsed.
fn read_config_file(path: Option<&Path>) -> Result<Option<(Scalars, FileOnly)>, String> {
    let Some((path, text)) = read_file(path)? else {
        return Ok(None);
    };
    Ok(Some((parse(path, &text)?, parse(path, &text)?)))
}

/// `settings.toml`'s settings, as [`read_config_file`] reads its file.
fn read_settings_file(path: Option<&Path>) -> Result<Option<Scalars>, String> {
    let Some((path, text)) = read_file(path)? else {
        return Ok(None);
    };
    parse(path, &text).map(Some)
}

fn read_file(path: Option<&Path>) -> Result<Option<(&Path, String)>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some((path, text))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", display_path(path))),
    }
}

fn parse<T: DeserializeOwned>(path: &Path, text: &str) -> Result<T, String> {
    toml::from_str(text).map_err(|error| describe_parse_error(path, text, &error))
}

/// `~/.config/diffui/config.toml:3:9: invalid type: …` — the location an
/// editor can jump to, then the first line of toml's message (the rest is a
/// source excerpt that doesn't fit a toast).
fn describe_parse_error(path: &std::path::Path, text: &str, error: &toml::de::Error) -> String {
    let message = error.message().lines().next().unwrap_or("").trim();
    match error.span() {
        Some(span) => {
            let before = &text[..span.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            let column = before
                .rsplit('\n')
                .next()
                .map_or(0, |line| line.chars().count())
                + 1;
            format!("{}:{line}:{column}: {message}", display_path(path))
        }
        None => format!("{}: {message}", display_path(path)),
    }
}

/// `path` with the home directory folded to `~`.
pub(crate) fn display_path(path: &Path) -> String {
    if let Some(home) = env::var_os("HOME").filter(|home| !home.is_empty())
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

pub(crate) fn config_path() -> Option<PathBuf> {
    app_file(AppDir::Config, "config.toml")
}

fn settings_path() -> Option<PathBuf> {
    app_file(AppDir::Config, "settings.toml")
}

fn ui_font(name: Option<&str>) -> Font {
    name.map(|name| resolve_font(name, Font::DEFAULT))
        .unwrap_or(Font::DEFAULT)
}

fn mono_font(name: Option<&str>) -> Font {
    name.map(|name| resolve_font(name, default_mono_font()))
        .unwrap_or_else(default_mono_font)
}

fn font_from_name(name: &str) -> Font {
    // `Font::new` takes a `&'static str`; we have a runtime string, so the
    // family name is leaked, once per family: the settings dialog can pick
    // the same one any number of times.
    static FAMILIES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut families = FAMILIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let family = match families.iter().find(|family| **family == name) {
        Some(family) => *family,
        None => {
            let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
            families.push(leaked);
            leaked
        }
    };
    Font::new(family)
}

/// The installed families a font setting can name, sorted, without the
/// platform's hidden ones (`.SF NS`, …).
pub(crate) fn font_families() -> Vec<String> {
    let mut families: Vec<String> = crate::measure::text_system()
        .all_font_names()
        .into_iter()
        .filter(|family| !family.starts_with('.'))
        .collect();
    families.sort_by_key(|family| family.to_lowercase());
    families.dedup();
    families
}

/// Resolve a configured family name against the installed fonts, falling
/// back to `fallback` when it isn't there. A named family that the font
/// database can't resolve doesn't merely degrade: requesting a non-default
/// weight (the tab bar's `Medium`, via `theme::emphasis_font`) on a missing
/// family can leave text rendering as invisible `.notdef` glyphs. Validating up front means a config shared
/// across machines just falls back to the platform default where the font
/// isn't installed.
fn resolve_font(name: &str, fallback: Font) -> Font {
    let name = name.trim();
    match installed_family(name) {
        Some(canonical) => font_from_name(&canonical),
        None => {
            tracing::warn!(
                "configured font family {name:?} is not installed; \
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
    let families = crate::measure::text_system().all_font_names();
    let normalize = |s: &str| {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let target = normalize(name);
    let mut loose_hit = None;
    for family in families {
        if family.eq_ignore_ascii_case(name) {
            return Some(family);
        }
        if loose_hit.is_none() && normalize(&family) == target {
            loose_hit = Some(family);
        }
    }
    loose_hit
}

fn default_mono_font() -> Font {
    Font::MONOSPACE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parse_error_names_the_line_and_column() {
        let text = "theme = \"dark\"\ncode_font_size = \"big\"\n";
        let error = toml::from_str::<Scalars>(text).expect_err("a string is not a size");
        let described =
            describe_parse_error(std::path::Path::new("/etc/diffui.toml"), text, &error);
        assert!(
            described.starts_with("/etc/diffui.toml:2:18: "),
            "{described}"
        );
        assert!(!described.contains('\n'), "{described}");
    }

    /// A scratch config directory holding `config` and `settings` (when
    /// given) as its two files.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str, config: Option<&str>, settings: Option<&str>) -> Self {
            let dir = env::temp_dir().join(format!("diffui-config-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("make the directory");
            let dir = Dir(dir);
            if let Some(config) = config {
                std::fs::write(dir.config(), config).expect("write config.toml");
            }
            if let Some(settings) = settings {
                std::fs::write(dir.settings(), settings).expect("write settings.toml");
            }
            dir
        }

        fn config(&self) -> PathBuf {
            self.0.join("config.toml")
        }

        fn settings(&self) -> PathBuf {
            self.0.join("settings.toml")
        }

        fn load(&self) -> (AppConfig, ConfigFiles) {
            let (config, _, files) =
                AppConfig::load_from(Some(self.config()), Some(self.settings()));
            (config, files)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn config_toml_wins_per_key_and_locks_what_it_sets() {
        let dir = Dir::new(
            "precedence",
            Some("theme = \"Dark\"\ncode_font_size = 14\n"),
            Some("theme = \"Light\"\nrefresh = \"Manual\"\ncode_font_size = 20\n"),
        );
        let (config, files) = dir.load();
        assert_eq!(config.theme, ThemePreference::Dark);
        assert_eq!(config.code_type.size, 14.0);
        // A key only settings.toml sets comes from there.
        assert_eq!(config.refresh, RefreshMode::Manual);
        assert!(config.locked.contains(Key::Theme));
        assert!(config.locked.contains(Key::CodeFontSize));
        assert!(!config.locked.contains(Key::Refresh));
        assert!(!config.locked.contains(Key::MultiClickMs));
        assert_eq!((files.config_error, files.settings_error), (None, None));
    }

    #[test]
    fn a_broken_settings_toml_leaves_config_toml_alone() {
        let dir = Dir::new(
            "broken-settings",
            Some("theme = \"Dark\"\n"),
            Some("refresh = = \"Manual\"\n"),
        );
        let (config, files) = dir.load();
        assert_eq!(config.theme, ThemePreference::Dark);
        assert_eq!(config.refresh, RefreshMode::Auto);
        assert!(config.locked.contains(Key::Theme));
        assert_eq!(files.config_error, None);
        let error = files.settings_error.expect("the broken file is reported");
        assert!(error.contains("settings.toml:1:"), "{error}");
    }

    #[test]
    fn a_broken_config_toml_leaves_settings_toml_alone() {
        let dir = Dir::new(
            "broken-config",
            Some("theme = \"Dark\"\ncode_font_size = \"big\"\n"),
            Some("theme = \"Light\"\nrefresh = \"Manual\"\n"),
        );
        let (config, files) = dir.load();
        // None of the broken file applies, not even its well-formed keys, and
        // none of it locks anything.
        assert_eq!(config.theme, ThemePreference::Light);
        assert_eq!(config.refresh, RefreshMode::Manual);
        assert_eq!(config.locked, Locked::default());
        assert_eq!(files.settings_error, None);
        let error = files.config_error.expect("the broken file is reported");
        assert!(error.contains("config.toml:2:18:"), "{error}");
    }

    #[test]
    fn a_write_round_trips_and_never_creates_config_toml() {
        let dir = Dir::new("round-trip", None, None);
        let (_, mut files) = dir.load();
        files
            .write(&Setting::Theme(ThemePreference::HighContrast))
            .expect("the first write");
        files
            .write(&Setting::CodeLineHeight(1.85))
            .expect("the second write");
        files
            .write(&Setting::Refresh(RefreshMode::Manual))
            .expect("the third write");
        assert!(!dir.config().exists(), "config.toml is never created");
        let text = std::fs::read_to_string(dir.settings()).expect("settings.toml");
        // Written as the docs spell it, and as typed rather than as an f32
        // widened to f64 would print.
        assert!(text.contains("theme = \"Contrast\""), "{text}");
        assert!(text.contains("code_line_height = 1.85\n"), "{text}");
        let (config, files) = dir.load();
        assert_eq!(config.theme, ThemePreference::HighContrast);
        assert_eq!(config.refresh, RefreshMode::Manual);
        assert_eq!(config.code_type.line_height, 1.85);
        assert_eq!(config.locked, Locked::default());
        assert_eq!(files.settings_error, None);
    }

    #[test]
    fn a_write_replaces_a_broken_settings_toml() {
        let dir = Dir::new("replace-broken", None, Some("not = = toml\n"));
        let (_, mut files) = dir.load();
        assert!(files.settings_error.is_some());
        files.write(&Setting::MultiClickMs(500)).expect("the write");
        assert_eq!(files.settings_error, None);
        let (config, files) = dir.load();
        assert_eq!(config.multi_click_ms, 500);
        assert_eq!(files.settings_error, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_config_toml_loads_and_is_left_untouched() {
        use std::os::unix::fs::PermissionsExt as _;
        let text = "theme = \"Dark\"\nmulti_click_ms = 400\n";
        let dir = Dir::new("read-only", Some(text), None);
        std::fs::set_permissions(dir.config(), std::fs::Permissions::from_mode(0o444))
            .expect("make config.toml read-only");
        let (config, mut files) = dir.load();
        assert_eq!(config.theme, ThemePreference::Dark);
        assert_eq!(config.multi_click_ms, 400);
        assert_eq!(files.config_error, None);
        files
            .write(&Setting::CodeFontSize(13.0))
            .expect("settings.toml is writable beside it");
        assert_eq!(std::fs::read_to_string(dir.config()).unwrap(), text);
        let (config, _) = dir.load();
        assert_eq!(config.code_type.size, 13.0);
    }

    #[test]
    fn revset_presets_parse_as_an_array_of_tables() {
        let raw: FileOnly = toml::from_str(
            r#"
            [[revset_presets]]
            name = "My stack"
            revset = "trunk()..@"

            [[revset_presets]]
            name = "Recent"
            revset = "ancestors(@, 30)"
            "#,
        )
        .expect("parses");
        let presets = raw.revset_presets.expect("present");
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].name, "My stack");
        assert_eq!(presets[1].revset, "ancestors(@, 30)");
        // Absent means "use the built-ins", not "none".
        let raw: FileOnly = toml::from_str("theme = \"dark\"").expect("parses");
        assert!(raw.revset_presets.is_none());
    }
}

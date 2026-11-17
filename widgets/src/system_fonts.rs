//! The platform's monospace font and the size code is set at, by each
//! platform's convention:
//!
//! - macOS: SF Mono where it's installed as a font, else Menlo, at 12px.
//! - Windows: Cascadia Mono (Windows Terminal's and Visual Studio's), else
//!   Consolas, at 10pt (13px).
//! - Linux and the BSDs: the desktop's monospace setting (GNOME's
//!   `monospace-font-name`, KDE's `fixed`), else fontconfig's `monospace`, at
//!   the desktop's size, else 10pt.
//!
//! Every candidate is checked against the installed fonts: GPUI can't lay out
//! text in a family it can't find.

use std::sync::OnceLock;

/// Where nothing on the list is installed. GPUI resolves it on every platform.
const LAST_RESORT: &str = ".SystemUIFont";

/// Families tried after the desktop's own choice, in order.
const CANDIDATES: &[&str] = if cfg!(target_os = "macos") {
    &["SF Mono", "Menlo", "Monaco", "Courier New"]
} else if cfg!(windows) {
    &["Cascadia Mono", "Consolas", "Courier New"]
} else {
    &[
        "DejaVu Sans Mono",
        "Noto Sans Mono",
        "Liberation Mono",
        "Ubuntu Mono",
    ]
};

/// Code size in px when the desktop doesn't say.
const DEFAULT_SIZE: f32 = if cfg!(target_os = "macos") {
    12.0
} else {
    13.0
};

/// The platform's monospace font.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Monospace {
    /// An installed family.
    pub family: &'static str,
    /// Code size in px.
    pub size: f32,
}

/// The platform's monospace font, resolved on first use.
pub fn monospace() -> Monospace {
    static RESOLVED: OnceLock<Monospace> = OnceLock::new();
    *RESOLVED.get_or_init(resolve)
}

fn resolve() -> Monospace {
    let desktop = desktop::monospace();
    let installed = crate::measure::text_system().all_font_names();
    let family = desktop
        .as_ref()
        .and_then(|setting| setting.family.as_deref())
        .into_iter()
        .chain(CANDIDATES.iter().copied())
        .find_map(|wanted| {
            installed
                .iter()
                .find(|name| name.eq_ignore_ascii_case(wanted))
        })
        .map(|name| &*Box::leak(name.clone().into_boxed_str()))
        .unwrap_or_else(|| {
            tracing::warn!("no monospace font installed; using the system UI font");
            LAST_RESORT
        });
    let size = desktop
        .and_then(|setting| setting.points)
        .map_or(DEFAULT_SIZE, points_to_px);
    Monospace { family, size }
}

/// GPUI's pixels are 1/96 inch on Windows and Linux, where font settings are
/// in points (1/72 inch).
fn points_to_px(points: f32) -> f32 {
    (points * 96.0 / 72.0).round()
}

/// What the desktop names as its monospace font.
#[derive(Debug, Default, PartialEq)]
struct Setting {
    family: Option<String>,
    points: Option<f32>,
}

#[cfg(any(target_os = "macos", windows))]
mod desktop {
    /// macOS and Windows have no user setting for it.
    pub(super) fn monospace() -> Option<super::Setting> {
        None
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod desktop {
    use super::Setting;
    use std::process::Command;

    pub(super) fn monospace() -> Option<Setting> {
        let kde = std::env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|desktop| desktop.split(':').any(|name| name == "KDE"));
        let setting = if kde { kde_fixed() } else { gnome() };
        let setting = setting.unwrap_or_default();
        let family = setting.family.or_else(fontconfig);
        (family.is_some() || setting.points.is_some()).then_some(Setting {
            family,
            points: setting.points,
        })
    }

    fn run(program: &str, args: &[&str]) -> Option<String> {
        let output = Command::new(program).args(args).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|text| !text.is_empty())
    }

    /// GNOME's `monospace-font-name`, a Pango description such as
    /// `'Adwaita Mono 11'`.
    fn gnome() -> Option<Setting> {
        let value = run(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "monospace-font-name"],
        )?;
        Some(parse_pango(value.trim_matches('\'')))
    }

    pub(super) fn parse_pango(description: &str) -> Setting {
        match description.rsplit_once(' ') {
            Some((family, size)) if size.parse::<f32>().is_ok() => Setting {
                family: Some(family.trim().to_owned()).filter(|family| !family.is_empty()),
                points: size.parse().ok(),
            },
            _ => Setting {
                family: Some(description.trim().to_owned()).filter(|family| !family.is_empty()),
                points: None,
            },
        }
    }

    /// KDE's `fixed` in `kdeglobals`, a Qt font string such as
    /// `Hack,10,-1,5,50,0,0,0,0,0`.
    fn kde_fixed() -> Option<Setting> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".config"))
            })?;
        let text = std::fs::read_to_string(config.join("kdeglobals")).ok()?;
        parse_kdeglobals(&text)
    }

    pub(super) fn parse_kdeglobals(text: &str) -> Option<Setting> {
        let mut general = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                general = line == "[General]";
            } else if general && let Some(value) = line.strip_prefix("fixed=") {
                let mut fields = value.split(',');
                return Some(Setting {
                    family: fields
                        .next()
                        .map(str::trim)
                        .filter(|family| !family.is_empty())
                        .map(str::to_owned),
                    points: fields.next().and_then(|size| size.trim().parse().ok()),
                });
            }
        }
        None
    }

    /// The family fontconfig's `monospace` alias resolves to.
    fn fontconfig() -> Option<String> {
        run("fc-match", &["-f", "%{family[0]}", "monospace"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_are_converted_at_96_dpi() {
        assert_eq!(points_to_px(10.0), 13.0);
        assert_eq!(points_to_px(11.0), 15.0);
        assert_eq!(points_to_px(12.0), 16.0);
    }

    #[test]
    fn the_resolved_family_is_installed() {
        let resolved = monospace();
        let installed = crate::measure::text_system().all_font_names();
        assert!(
            resolved.family == LAST_RESORT || installed.iter().any(|name| name == resolved.family),
            "{resolved:?}"
        );
        assert!(resolved.size > 0.0);
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn desktop_settings_parse() {
        use super::desktop::{parse_kdeglobals, parse_pango};
        assert_eq!(
            parse_pango("Adwaita Mono 11"),
            Setting {
                family: Some("Adwaita Mono".into()),
                points: Some(11.0)
            }
        );
        assert_eq!(
            parse_pango("Monospace"),
            Setting {
                family: Some("Monospace".into()),
                points: None
            }
        );
        assert_eq!(
            parse_kdeglobals("[KDE]\nfixed=Nope,9\n[General]\nfixed=Hack,10,-1,5,50,0,0,0,0,0\n"),
            Some(Setting {
                family: Some("Hack".into()),
                points: Some(10.0)
            })
        );
        assert_eq!(parse_kdeglobals("[General]\nfont=Noto Sans,10\n"), None);
    }
}

//! Whether a plugin may run: what the user answered when asked, and which
//! permissions that answer was about.
//!
//! A plugin runs only after the user allowed it, and an answer covers the
//! permissions the manifest asked for then. A manifest that later asks for
//! more is asked about again; one that asks for the same or less runs on the
//! old answer. Nothing here ever allows a plugin by itself.

use diffui_plugins::{JjAccess, Manifest, Permissions};
use serde::{Deserialize, Serialize};

/// What `settings.toml` keeps about one plugin, under `[plugins.<id>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The toggle in Settings. Off: the plugin isn't loaded.
    #[serde(default = "enabled", skip_serializing_if = "is_true")]
    pub(crate) enabled: bool,
    /// The answer to the permission dialog. Absent: never asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) allowed: Option<bool>,
    /// The permissions that answer was about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permissions: Option<Permissions>,
}

fn enabled() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl Default for Record {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed: None,
            permissions: None,
        }
    }
}

/// Where a plugin stands against its record.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verdict {
    /// The user allowed these permissions, or more.
    Allowed,
    /// The user refused exactly these permissions.
    Refused,
    /// Nobody answered for these permissions yet. `before` is what was
    /// allowed last time, when the manifest now asks for more.
    Ask { before: Option<Permissions> },
}

pub(crate) fn verdict(record: Option<&Record>, asked: &Permissions) -> Verdict {
    let answered = record.and_then(|record| Some((record.allowed?, record.permissions.as_ref()?)));
    match answered {
        Some((true, granted)) if covers(granted, asked) => Verdict::Allowed,
        Some((true, granted)) => Verdict::Ask {
            before: Some(granted.clone()),
        },
        Some((false, refused)) if refused == asked => Verdict::Refused,
        _ => Verdict::Ask { before: None },
    }
}

/// Whether everything `asked` names was already in `granted`.
fn covers(granted: &Permissions, asked: &Permissions) -> bool {
    asked.jj <= granted.jj
        && asked.http.iter().all(|host| granted.http.contains(host))
        && asked.exec.iter().all(|argv| exec_covered(granted, argv))
}

fn exec_covered(granted: &Permissions, argv: &[String]) -> bool {
    granted
        .exec
        .iter()
        .any(|prefix| argv.starts_with(prefix.as_slice()))
}

/// One thing a plugin may do, in plain words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) glyph: &'static str,
    pub(crate) text: String,
    /// The host or command line the words are about, shown as code after
    /// them.
    pub(crate) code: Option<String>,
    /// Words after the code.
    pub(crate) tail: &'static str,
    /// It wasn't in what the user allowed before.
    pub(crate) new: bool,
}

/// What `manifest` lets the plugin do, a line each. `before` is what the
/// user allowed last time, so what has been added can be marked.
pub(crate) fn lines(manifest: &Manifest, before: Option<&Permissions>) -> Vec<Line> {
    use crate::icons;
    let asked = &manifest.permissions;
    let line = |glyph: &'static str, text: &str, code: Option<String>, new: bool| Line {
        glyph,
        text: text.to_owned(),
        code,
        tail: "",
        new,
    };
    let mut lines = Vec::new();
    for host in &asked.http {
        lines.push(Line {
            tail: ", using your git credentials for it",
            ..line(
                icons::EXTERNAL_LINK,
                "Send requests to",
                Some(host.to_string()),
                before.is_some_and(|before| !before.http.contains(host)),
            )
        });
    }
    for argv in &asked.exec {
        lines.push(line(
            icons::TERMINAL,
            "Run programs that start with",
            Some(argv.join(" ")),
            before.is_some_and(|before| !exec_covered(before, argv)),
        ));
    }
    let more_jj = before.is_some_and(|before| asked.jj > before.jj);
    match asked.jj {
        JjAccess::None => {}
        JjAccess::Read => lines.push(line(
            icons::SEARCH,
            "Read the open repository: its revisions, diffs and files",
            None,
            more_jj,
        )),
        JjAccess::Write => lines.push(line(
            icons::PENCIL,
            "Read the open repository and change it with jj commands",
            None,
            more_jj,
        )),
    }
    if let Some(forge) = &manifest.forge {
        let hosts: Vec<String> = forge.hosts.iter().map(ToString::to_string).collect();
        let text = match forge.term {
            diffui_plugins::ForgeTerm::PullRequest => "Show pull requests for remotes on",
            diffui_plugins::ForgeTerm::MergeRequest => "Show merge requests for remotes on",
        };
        lines.push(line(
            icons::PULL_REQUEST,
            text,
            Some(hosts.join(", ")),
            false,
        ));
    }
    lines.push(line(
        icons::SCROLL_TEXT,
        "Show messages and prompts, open links and copy to the clipboard",
        None,
        false,
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use diffui_plugins::HostPattern;

    fn permissions(http: &[&str], exec: &[&[&str]], jj: JjAccess) -> Permissions {
        Permissions {
            http: http
                .iter()
                .map(|host| HostPattern::parse(host).expect("a host"))
                .collect(),
            exec: exec
                .iter()
                .map(|argv| argv.iter().map(|word| (*word).to_owned()).collect())
                .collect(),
            jj,
        }
    }

    fn record(allowed: bool, permissions: &Permissions) -> Record {
        Record {
            enabled: true,
            allowed: Some(allowed),
            permissions: Some(permissions.clone()),
        }
    }

    #[test]
    fn nothing_runs_until_somebody_answers() {
        let asked = Permissions::default();
        assert_eq!(verdict(None, &asked), Verdict::Ask { before: None });
        assert_eq!(
            verdict(Some(&Record::default()), &asked),
            Verdict::Ask { before: None }
        );
    }

    #[test]
    fn an_answer_covers_the_same_permissions_or_fewer() {
        let granted = permissions(
            &["codeberg.org", "example.com"],
            &[&["git", "credential"]],
            JjAccess::Write,
        );
        let allowed = record(true, &granted);
        assert_eq!(verdict(Some(&allowed), &granted), Verdict::Allowed);
        let fewer = permissions(
            &["codeberg.org"],
            &[&["git", "credential", "fill"]],
            JjAccess::Read,
        );
        assert_eq!(verdict(Some(&allowed), &fewer), Verdict::Allowed);
    }

    #[test]
    fn asking_for_more_asks_again() {
        let granted = permissions(&["codeberg.org"], &[&["git", "credential"]], JjAccess::Read);
        let allowed = record(true, &granted);
        for more in [
            permissions(
                &["codeberg.org", "evil.example"],
                &[&["git", "credential"]],
                JjAccess::Read,
            ),
            permissions(&["codeberg.org"], &[&["git"]], JjAccess::Read),
            permissions(
                &["codeberg.org"],
                &[&["git", "credential"]],
                JjAccess::Write,
            ),
        ] {
            assert_eq!(
                verdict(Some(&allowed), &more),
                Verdict::Ask {
                    before: Some(granted.clone())
                },
                "{more:?}"
            );
        }
    }

    #[test]
    fn a_refusal_holds_for_what_was_refused_only() {
        let refused = permissions(&["codeberg.org"], &[], JjAccess::Write);
        let record = record(false, &refused);
        assert_eq!(verdict(Some(&record), &refused), Verdict::Refused);
        let other = permissions(&["codeberg.org"], &[], JjAccess::Read);
        assert_eq!(
            verdict(Some(&record), &other),
            Verdict::Ask { before: None }
        );
    }

    #[test]
    fn a_record_round_trips_through_settings_toml() {
        let granted = permissions(
            &["codeberg.org", "*.example.com", "localhost:3000"],
            &[&["git", "credential"]],
            JjAccess::Read,
        );
        let text = toml::to_string(&record(true, &granted)).expect("serialises");
        assert!(!text.contains("enabled"), "{text}");
        let read: Record = toml::from_str(&text).expect("parses");
        assert_eq!(read, record(true, &granted));

        let off = Record {
            enabled: false,
            ..Record::default()
        };
        let text = toml::to_string(&off).expect("serialises");
        assert_eq!(text.trim(), "enabled = false");
        assert_eq!(toml::from_str::<Record>(&text).expect("parses"), off);
    }
}

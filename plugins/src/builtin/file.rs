//! `files.read`: read a file `[permissions] read` names, such as a CLI's own
//! config (`gh`'s `hosts.yml`).
//!
//! An entry is a path written the way a shell would spell it: `~/` for the
//! home directory, `$NAME` / `${NAME}` for a variable and `${NAME:-word}` for
//! a variable or, when it is unset or empty, `word` (itself expanded). The
//! script names a file by its entry as written, or by the path it expands
//! to, so it never needs the environment to find it.

use std::io::Read as _;

use crate::host::HostError;

/// The path `entry` expands to with `var` looking up variables and `home`
/// standing for `~`. `Err` says what is wrong with how it is written; a
/// variable with no default that `var` doesn't have is `Ok(None)`.
pub(crate) fn expand(
    entry: &str,
    var: &dyn Fn(&str) -> Option<String>,
    home: &str,
) -> Result<Option<String>, String> {
    let mut chars = entry.chars().peekable();
    let path = word(&mut chars, var, home, false)?;
    if chars.next().is_some() {
        return Err("a \"}\" closes nothing".to_owned());
    }
    Ok(path)
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Expand up to the end, or up to a `}` when `nested` (left unread).
fn word(
    chars: &mut Chars<'_>,
    var: &dyn Fn(&str) -> Option<String>,
    home: &str,
    nested: bool,
) -> Result<Option<String>, String> {
    let mut out = String::new();
    let mut missing = false;
    if chars.peek() == Some(&'~') {
        chars.next();
        if !matches!(chars.peek(), None | Some('/') | Some('}')) {
            return Err("\"~\" is only the home directory: \"~/…\"".to_owned());
        }
        out.push_str(home);
    }
    while let Some(&c) = chars.peek() {
        match c {
            '}' if nested => break,
            '}' => return Err("a \"}\" closes nothing".to_owned()),
            '$' => {
                chars.next();
                let braced = chars.peek() == Some(&'{');
                if braced {
                    chars.next();
                }
                let mut name = String::new();
                while let Some(&c) = chars.peek()
                    && (c.is_ascii_alphanumeric() || c == '_')
                {
                    name.push(c);
                    chars.next();
                }
                if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
                    return Err("\"$\" must be followed by a variable's name".to_owned());
                }
                let value = var(&name).filter(|value| !value.is_empty());
                if !braced {
                    match value {
                        Some(value) => out.push_str(&value),
                        None => missing = true,
                    }
                    continue;
                }
                let fallback = if chars.peek() == Some(&':') {
                    chars.next();
                    if chars.next() != Some('-') {
                        return Err(format!("only \"${{{name}:-default}}\" is understood"));
                    }
                    Some(word(chars, var, home, true)?)
                } else {
                    None
                };
                if chars.next() != Some('}') {
                    return Err(format!("\"${{{name}\" needs its \"}}\""));
                }
                match (value, fallback) {
                    (Some(value), _) => out.push_str(&value),
                    (None, Some(Some(fallback))) => out.push_str(&fallback),
                    (None, _) => missing = true,
                }
            }
            c => {
                out.push(c);
                chars.next();
            }
        }
    }
    Ok((!missing).then_some(out))
}

/// Whether `entry` is written right, for the manifest: it expands, and to an
/// absolute path without `..` (checked with every variable unset, so with
/// the defaults it gives).
pub(crate) fn check_entry(entry: &str) -> Result<(), String> {
    let expanded = expand(entry, &|_| None, "/home")?;
    let Some(path) = expanded else {
        // A variable without a default: what it holds is checked when read.
        return if entry.starts_with('$') {
            Ok(())
        } else {
            Err("must start with \"/\", \"~/\" or a variable".to_owned())
        };
    };
    if !path.starts_with('/') {
        return Err("must be an absolute path, or start with \"~/\" or a variable".to_owned());
    }
    if path.split('/').any(|segment| segment == "..") {
        return Err("must not go up with \"..\"".to_owned());
    }
    Ok(())
}

/// The file `asked` names, when one of `allowed` (the manifest's entries)
/// names it: by the entry as written or by the path it expands to here.
pub(crate) fn resolve(asked: &str, allowed: &[String]) -> Result<String, HostError> {
    let var = |name: &str| std::env::var(name).ok();
    let home = std::env::var("HOME").unwrap_or_default();
    for entry in allowed {
        let path = expand(entry, &var, &home).ok().flatten();
        if entry == asked {
            return path.filter(|path| path.starts_with('/')).ok_or_else(|| {
                HostError::new(
                    "NotFoundError",
                    format!("{entry} names no file here: a variable in it is not set"),
                )
            });
        }
        if path.as_deref() == Some(asked) {
            return Ok(asked.to_owned());
        }
    }
    Err(HostError::denied(format!(
        "files.read {asked:?} is not allowed: plugin.toml's [permissions] read must list it"
    )))
}

/// The text of the file at `path`, at most `max` bytes of it.
pub(crate) fn read(path: &str, max: u64) -> Result<String, HostError> {
    let file = std::fs::File::open(path).map_err(|error| {
        let name = if error.kind() == std::io::ErrorKind::NotFound {
            "NotFoundError"
        } else {
            "Error"
        };
        HostError::new(name, format!("can't read {path}: {error}"))
    })?;
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| HostError::new("Error", format!("can't read {path}: {error}")))?;
    if bytes.len() as u64 > max {
        return Err(HostError::new(
            "Error",
            format!("{path} is bigger than {} MB", max / (1024 * 1024)),
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(entry: &str, vars: &[(&str, &str)]) -> Result<Option<String>, String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        expand(
            entry,
            &|name| {
                vars.iter()
                    .find(|(var, _)| var == name)
                    .map(|(_, value)| value.clone())
            },
            "/Users/me",
        )
    }

    #[test]
    fn an_entry_expands_like_a_shell_word() {
        let gh = "${GH_CONFIG_DIR:-${XDG_CONFIG_HOME:-~/.config}/gh}/hosts.yml";
        assert_eq!(
            with(gh, &[]).unwrap().as_deref(),
            Some("/Users/me/.config/gh/hosts.yml")
        );
        assert_eq!(
            with(gh, &[("XDG_CONFIG_HOME", "/xdg")]).unwrap().as_deref(),
            Some("/xdg/gh/hosts.yml")
        );
        assert_eq!(
            with(gh, &[("GH_CONFIG_DIR", "/gh"), ("XDG_CONFIG_HOME", "/xdg")])
                .unwrap()
                .as_deref(),
            Some("/gh/hosts.yml")
        );
        // Empty is unset, as with ":-".
        assert_eq!(
            with(gh, &[("GH_CONFIG_DIR", "")]).unwrap().as_deref(),
            Some("/Users/me/.config/gh/hosts.yml")
        );
        assert_eq!(
            with("$HOME/a/$X.yml", &[("HOME", "/h"), ("X", "x")])
                .unwrap()
                .as_deref(),
            Some("/h/a/x.yml")
        );
        assert_eq!(with("$NOPE/a", &[]), Ok(None));
        assert_eq!(with("${NOPE}/a", &[]), Ok(None));
    }

    #[test]
    fn a_badly_written_entry_says_why() {
        for (entry, said) in [
            ("${A", "needs its"),
            ("${A:=b}", "only \"${A:-default}\""),
            ("a}", "closes nothing"),
            ("$/x", "variable's name"),
            ("~me/x", "only the home directory"),
        ] {
            let error = with(entry, &[]).unwrap_err();
            assert!(error.contains(said), "{entry}: {error}");
        }
        assert!(check_entry("~/.config/gh/hosts.yml").is_ok());
        assert!(check_entry("$GH_CONFIG_DIR/hosts.yml").is_ok());
        assert!(check_entry("/etc/hosts").is_ok());
        for (entry, said) in [
            ("hosts.yml", "absolute path"),
            ("a/$X", "must start with"),
            ("${X:-relative}/hosts.yml", "absolute path"),
            ("~/../etc/passwd", "\"..\""),
        ] {
            let error = check_entry(entry).unwrap_err();
            assert!(error.contains(said), "{entry}: {error}");
        }
    }

    #[test]
    fn a_file_is_read_by_its_entry_or_its_path_only() {
        let dir = std::env::temp_dir().join(format!("diffui-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hosts.yml");
        std::fs::write(&file, "github.com:\n").unwrap();
        let path = file.display().to_string();
        let allowed = vec![path.clone(), "$DIFFUI_NO_SUCH_VAR/x".to_owned()];
        assert_eq!(
            resolve(&path, &allowed).map_err(|error| error.name),
            Ok(path.clone())
        );
        assert_eq!(read(&path, 1024).unwrap(), "github.com:\n");
        assert_eq!(read(&path, 4).unwrap_err().name, "Error");
        let refused = resolve("/etc/passwd", &allowed).unwrap_err();
        assert_eq!(refused.name, "PermissionError");
        let unset = resolve("$DIFFUI_NO_SUCH_VAR/x", &allowed).unwrap_err();
        assert_eq!(unset.name, "NotFoundError");
        let gone = dir.join("gone.yml").display().to_string();
        assert_eq!(read(&gone, 1024).unwrap_err().name, "NotFoundError");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

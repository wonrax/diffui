//! Conflict resolution: what the frontend shows about a conflicted file, how
//! it asks for a resolution, and the external merge tool round trip.
//!
//! Mirrors `jj resolve`: a conflict can be resolved by picking one of its
//! terms (`--tool :ours` / `:theirs`, generalised to every side and base of an
//! n-way conflict) or by running the user's `ui.merge-editor` on a 2-sided
//! file conflict and writing its output back. The tool runs in the frontend,
//! outside the repository actor, so a merge session that stays open for
//! minutes never blocks graph loads, snapshots or other mutations: the actor
//! only prepares the input files ([`crate::Command::PrepareMergeTool`]) and
//! later applies the result as an ordinary mutation
//! ([`crate::MutationOp::ResolveConflict`]).

use std::collections::HashMap;
use std::path::PathBuf;

use jj_lib::config::ConfigGetResultExt as _;
use jj_lib::settings::UserSettings;

use crate::model::RevisionSelection;

/// How to resolve one conflicted path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Take side `n` (0-based) of the conflict as the file's content —
    /// `jj resolve --tool :ours` is `Side(0)`, `:theirs` is `Side(1)`.
    Side(usize),
    /// Take base `n` (0-based): drop every side's change.
    Base(usize),
    /// Write this content. With `marker_len`, the content is parsed for jj
    /// conflict markers of that length first, so a partially resolved file
    /// stays a (smaller) conflict, like a merge tool configured with
    /// `merge-tool-edits-conflict-markers`.
    Content {
        content: Vec<u8>,
        marker_len: Option<usize>,
    },
}

/// One term of a conflict, labeled the way jj labels it in conflict markers
/// (`nlqwxzwn 7dd24e73 "description"`), or `side #n` / `base #n` when the
/// conflict carries no labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictTerm {
    pub label: String,
    /// The path doesn't exist on this term (one side deleted the file).
    pub absent: bool,
}

/// A conflicted path in a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictFile {
    pub path: String,
    /// The conflict's sides (jj's "adds"), in marker order: side 1 is what
    /// `:ours` picks, side 2 `:theirs`.
    pub sides: Vec<ConflictTerm>,
    /// The bases (jj's "removes"); one fewer than the sides.
    pub bases: Vec<ConflictTerm>,
    /// Whether a side or base can be taken as is — false when a term is a
    /// symlink, directory or submodule rather than a file.
    pub pickable: bool,
    /// Why the merge tool can't take this conflict (more than two sides, a
    /// non-file term, an executable-bit conflict), or `None` when it can.
    pub merge_tool_blocker: Option<String>,
}

/// A prepared merge-tool run: the input files are written, the arguments
/// interpolated. [`run`](Self::run) blocks until the tool exits, so call it
/// off the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeToolInvocation {
    /// The configured name (`vscode`, `meld`…), for messages.
    pub tool: String,
    pub program: String,
    pub args: Vec<String>,
    /// Scratch directory holding base/left/right/output; removed by
    /// [`cleanup`](Self::cleanup).
    pub dir: PathBuf,
    pub output: PathBuf,
    /// What `output` held before the tool ran. An unchanged (or empty)
    /// output means the user didn't resolve anything.
    pub initial_output: Vec<u8>,
    /// Exit codes that mean "exited with conflicts left" rather than failure.
    pub conflict_exit_codes: Vec<i32>,
    /// Conflict-marker length when the output may still contain markers to
    /// parse back into a conflict.
    pub marker_len: usize,
    pub edits_markers: bool,
    /// What to resolve when the tool is done.
    pub revision: RevisionSelection,
    pub path: String,
}

impl MergeToolInvocation {
    /// Run the tool and read back its output as a resolution. Blocking.
    pub fn run(&self) -> Result<ConflictResolution, String> {
        let status = std::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(std::process::Stdio::null())
            .status()
            .map_err(|error| format!("failed to run {} ({}): {error}", self.tool, self.program))?;
        let implies_conflict = status
            .code()
            .is_some_and(|code| self.conflict_exit_codes.contains(&code));
        if !status.success() && !implies_conflict {
            return Err(format!("{} exited with {status}", self.tool));
        }
        let content = std::fs::read(&self.output)
            .map_err(|error| format!("failed to read the merge tool's output: {error}"))?;
        if content.is_empty() || content == self.initial_output {
            return Err(format!(
                "{} left the output file empty or unchanged — nothing was resolved",
                self.tool
            ));
        }
        Ok(ConflictResolution::Content {
            content,
            marker_len: (self.edits_markers || implies_conflict).then_some(self.marker_len),
        })
    }

    /// Remove the scratch directory.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A merge tool as configured: `[merge-tools.<name>]` merged over the
/// definitions jj-cli ships.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeToolSpec {
    pub name: String,
    pub program: String,
    pub merge_args: Vec<String>,
    pub edits_markers: bool,
    /// `conflict-marker-style` override (`diff`, `snapshot`, `git`…).
    pub marker_style: Option<String>,
    pub conflict_exit_codes: Vec<i32>,
}

impl MergeToolSpec {
    /// Whether the arguments ask for `$marker_length`.
    pub fn uses_marker_length(&self) -> bool {
        self.merge_args
            .iter()
            .any(|arg| arg.contains("$marker_length"))
    }
}

/// The user-configurable keys of a `[merge-tools.<name>]` table that matter
/// for resolving.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct ToolConfig {
    program: Option<String>,
    merge_args: Option<Vec<String>>,
    merge_tool_edits_conflict_markers: Option<bool>,
    conflict_marker_style: Option<String>,
    merge_conflict_exit_codes: Option<Vec<i32>>,
}

/// `ui.merge-editor`: a tool name / command line, or an argv array.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum EditorSetting {
    Name(String),
    Argv(Vec<String>),
}

/// The merge-tool definitions jj-cli 0.45.1 ships in its default config
/// (`cli/src/config/merge_tools.toml`), limited to tools with `merge-args`.
/// diffui embeds jj-lib, not jj-cli, so these defaults would otherwise be
/// missing; a user's own `[merge-tools.<name>]` keys override them key by key,
/// as jj's layered config does.
fn builtin_tool(name: &str) -> Option<ToolConfig> {
    let args = |args: &[&str]| Some(args.iter().map(|arg| (*arg).to_owned()).collect());
    let tool = match name {
        "kdiff3" => ToolConfig {
            merge_args: args(&["$base", "$left", "$right", "-o", "$output", "--auto"]),
            ..ToolConfig::default()
        },
        "meld" => ToolConfig {
            merge_args: args(&["$left", "$base", "$right", "-o", "$output", "--auto-merge"]),
            ..ToolConfig::default()
        },
        "mergiraf" => ToolConfig {
            merge_args: args(&[
                "merge",
                "$base",
                "$left",
                "$right",
                "-o",
                "$output",
                "-l",
                "$marker_length",
                "-p",
                "$path",
                "--fast",
            ]),
            merge_conflict_exit_codes: Some(vec![1]),
            ..ToolConfig::default()
        },
        "smerge" => ToolConfig {
            merge_args: args(&["mergetool", "$base", "$left", "$right", "-o", "$output"]),
            conflict_marker_style: Some("git".to_owned()),
            ..ToolConfig::default()
        },
        "vscode" | "vscodium" => ToolConfig {
            program: Some(if name == "vscode" { "code" } else { "codium" }.to_owned()),
            merge_args: args(&["--wait", "--merge", "$left", "$right", "$base", "$output"]),
            merge_tool_edits_conflict_markers: Some(true),
            conflict_marker_style: Some("git".to_owned()),
            ..ToolConfig::default()
        },
        _ => return None,
    };
    Some(tool)
}

/// Terminal-only tools: a GUI app has no terminal to run them in.
fn terminal_tool(name: &str, program: &str) -> bool {
    matches!(name, "vimdiff" | ":builtin")
        || matches!(
            std::path::Path::new(program)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("vim" | "nvim" | "vimdiff" | "nvimdiff" | "emacs" | "nano")
        )
}

/// Resolve `ui.merge-editor` (and the `[merge-tools]` table it names) into a
/// runnable tool, with jj's rules: a bare name looks up `merge-tools.<name>`
/// (falling back to jj's built-in definitions), anything else is a command
/// line whose arguments must include the file variables.
pub fn merge_tool_from_settings(settings: &UserSettings) -> Result<MergeToolSpec, String> {
    let setting: Option<EditorSetting> = settings
        .get("ui.merge-editor")
        .optional()
        .map_err(|error| format!("invalid ui.merge-editor: {error}"))?;
    let Some(setting) = setting else {
        return Err(
            "No merge tool is configured. Set ui.merge-editor in your jj config \
                    (for example \"vscode\", \"meld\", \"kdiff3\" or \"smerge\")."
                .to_owned(),
        );
    };
    let spec = match setting {
        EditorSetting::Name(name) => match name.as_str() {
            ":builtin" => {
                return Err("ui.merge-editor is jj's built-in editor, which runs in a \
                            terminal. Set it to a GUI tool (vscode, meld, kdiff3, smerge) \
                            to resolve from diffui, or run `jj resolve` in a terminal."
                    .to_owned());
            }
            ":ours" | ":theirs" => {
                return Err(format!(
                    "ui.merge-editor is {name}; use \"Take side\" on the file instead"
                ));
            }
            _ if terminal_tool(&name, &name) => {
                return Err(terminal_tool_error(&name));
            }
            _ => match named_tool(settings, &name)? {
                Some(spec) => spec,
                None => {
                    let argv = split_command_line(&name);
                    spec_from_argv(&name, argv)?
                }
            },
        },
        EditorSetting::Argv(argv) => {
            let name = argv.first().cloned().unwrap_or_default();
            spec_from_argv(&name, argv)?
        }
    };
    if terminal_tool(&spec.name, &spec.program) {
        return Err(terminal_tool_error(&spec.name));
    }
    Ok(spec)
}

fn terminal_tool_error(name: &str) -> String {
    format!(
        "{name} runs in a terminal, which diffui doesn't have. Run `jj resolve` in a \
         terminal, or set ui.merge-editor to a GUI tool (vscode, meld, kdiff3, smerge)."
    )
}

fn named_tool(settings: &UserSettings, name: &str) -> Result<Option<MergeToolSpec>, String> {
    let user: Option<ToolConfig> = settings
        .get(["merge-tools", name])
        .optional()
        .map_err(|error| format!("invalid merge-tools.{name}: {error}"))?;
    let builtin = builtin_tool(name);
    if user.is_none() && builtin.is_none() {
        return Ok(None);
    }
    let user = user.unwrap_or_default();
    let builtin = builtin.unwrap_or_default();
    let merge_args = user.merge_args.or(builtin.merge_args).unwrap_or_default();
    if merge_args.is_empty() {
        return Err(format!(
            "merge tool \"{name}\" has no merge-args (set merge-tools.{name}.merge-args)"
        ));
    }
    Ok(Some(MergeToolSpec {
        name: name.to_owned(),
        program: user
            .program
            .or(builtin.program)
            .unwrap_or_else(|| name.to_owned()),
        merge_args,
        edits_markers: user
            .merge_tool_edits_conflict_markers
            .or(builtin.merge_tool_edits_conflict_markers)
            .unwrap_or(false),
        marker_style: user.conflict_marker_style.or(builtin.conflict_marker_style),
        conflict_exit_codes: user
            .merge_conflict_exit_codes
            .or(builtin.merge_conflict_exit_codes)
            .unwrap_or_default(),
    }))
}

fn spec_from_argv(name: &str, argv: Vec<String>) -> Result<MergeToolSpec, String> {
    let mut argv = argv.into_iter();
    let Some(program) = argv.next().filter(|program| !program.is_empty()) else {
        return Err("ui.merge-editor is empty".to_owned());
    };
    let merge_args: Vec<String> = argv.collect();
    if merge_args.is_empty() {
        return Err(format!(
            "merge tool \"{name}\" has no merge-args (set merge-tools.{name}.merge-args)"
        ));
    }
    Ok(MergeToolSpec {
        name: name.to_owned(),
        program,
        merge_args,
        edits_markers: false,
        marker_style: None,
        conflict_exit_codes: Vec::new(),
    })
}

/// Split a command line on whitespace, honoring single and double quotes and
/// backslash escapes — enough of POSIX shell word splitting for an editor
/// setting.
pub fn split_command_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') | (None, '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
                in_word = true;
            }
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(ch);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Replace `$name` variables in `args` with their values, leaving unknown
/// ones as written (jj's `interpolate_variables`).
pub fn interpolate(args: &[String], variables: &HashMap<&str, String>) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let mut out = String::with_capacity(arg.len());
            let mut rest = arg.as_str();
            while let Some(start) = rest.find('$') {
                out.push_str(&rest[..start]);
                let after = &rest[start + 1..];
                let len = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                let name = &after[..len];
                match variables.get(name) {
                    Some(value) if len > 0 => out.push_str(value),
                    _ => {
                        out.push('$');
                        out.push_str(name);
                    }
                }
                rest = &after[len..];
            }
            out.push_str(rest);
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(toml: &str) -> UserSettings {
        let mut config = jj_lib::config::StackedConfig::with_defaults();
        config.add_layer(
            jj_lib::config::ConfigLayer::parse(
                jj_lib::config::ConfigSource::User,
                &format!("user.name = \"t\"\nuser.email = \"t@e\"\n{toml}"),
            )
            .expect("parse"),
        );
        UserSettings::from_config(config).expect("settings")
    }

    #[test]
    fn builtin_tools_resolve_by_name() {
        let spec = merge_tool_from_settings(&settings("ui.merge-editor = \"vscode\""))
            .expect("vscode resolves");
        assert_eq!(spec.program, "code");
        assert!(spec.edits_markers);
        assert_eq!(spec.marker_style.as_deref(), Some("git"));
        assert_eq!(spec.merge_args[0], "--wait");

        let meld = merge_tool_from_settings(&settings("ui.merge-editor = \"meld\"")).expect("meld");
        assert_eq!(meld.program, "meld");
        assert!(meld.merge_args.contains(&"$output".to_owned()));
    }

    #[test]
    fn user_keys_override_builtin_definitions() {
        let spec = merge_tool_from_settings(&settings(
            "ui.merge-editor = \"meld\"\n[merge-tools.meld]\nprogram = \"/opt/meld\"\n",
        ))
        .expect("meld");
        assert_eq!(spec.program, "/opt/meld");
        // merge-args still come from the built-in definition.
        assert!(spec.merge_args.contains(&"--auto-merge".to_owned()));
    }

    #[test]
    fn custom_tools_and_command_lines() {
        let spec = merge_tool_from_settings(&settings(
            "ui.merge-editor = \"mytool\"\n[merge-tools.mytool]\nmerge-args = [\"$left\", \"$right\", \"$base\", \"$output\"]\n",
        ))
        .expect("custom tool");
        assert_eq!(spec.program, "mytool");
        assert_eq!(spec.merge_args.len(), 4);

        let argv = merge_tool_from_settings(&settings(
            "ui.merge-editor = [\"tool\", \"$base\", \"$output\"]",
        ))
        .expect("argv");
        assert_eq!(argv.program, "tool");
        assert_eq!(argv.merge_args, vec!["$base", "$output"]);

        let line = merge_tool_from_settings(&settings(
            "ui.merge-editor = \"'my tool' -o $output $left\"",
        ))
        .expect("command line");
        assert_eq!(line.program, "my tool");
        assert_eq!(line.merge_args, vec!["-o", "$output", "$left"]);
    }

    #[test]
    fn unusable_settings_explain_themselves() {
        assert!(
            merge_tool_from_settings(&settings(""))
                .unwrap_err()
                .contains("ui.merge-editor")
        );
        assert!(
            merge_tool_from_settings(&settings("ui.merge-editor = \":builtin\""))
                .unwrap_err()
                .contains("terminal")
        );
        assert!(
            merge_tool_from_settings(&settings("ui.merge-editor = \"vimdiff\""))
                .unwrap_err()
                .contains("terminal")
        );
        assert!(
            merge_tool_from_settings(&settings("ui.merge-editor = \"nothing-known\""))
                .unwrap_err()
                .contains("merge-args")
        );
    }

    #[test]
    fn interpolation_replaces_known_variables_only() {
        let vars: HashMap<&str, String> = [
            ("left", "/t/left".to_owned()),
            ("output", "/t/out".to_owned()),
        ]
        .into_iter()
        .collect();
        let args = vec![
            "$left".to_owned(),
            "--out=$output".to_owned(),
            "$unknown".to_owned(),
            "$".to_owned(),
        ];
        assert_eq!(
            interpolate(&args, &vars),
            vec!["/t/left", "--out=/t/out", "$unknown", "$"]
        );
    }

    #[test]
    fn command_lines_split_like_a_shell() {
        assert_eq!(
            split_command_line(r#"a "b c" 'd e' f\ g"#),
            vec!["a", "b c", "d e", "f g"]
        );
        assert!(split_command_line("   ").is_empty());
    }
}

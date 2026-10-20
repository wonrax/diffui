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
//!
//! The tool's definition is jj-cli's (`get_external_tool_config` over jj-cli's
//! shipped `[merge-tools]`). Ported, because jj-cli runs the tool itself with
//! a `Ui`, inside the command (check against jj-cli on a bump):
//! - `merge_tool_from_settings` ports `MergeEditor::from_settings`
//!   (`cli/src/merge_tools/mod.rs`), minus `:builtin`, which needs a terminal.
//! - `jj::resolve::prepare_merge_tool` and [`MergeToolInvocation::run`] port
//!   `run_mergetool_external_single_file` (`cli/src/merge_tools/external.rs`).

use std::path::PathBuf;

use jj_cli::config::{CommandNameAndArgs, find_all_variables};
use jj_cli::merge_tools::{ExternalMergeTool, get_external_tool_config};
use jj_lib::config::ConfigGetResultExt as _;
use jj_lib::conflicts::ConflictMarkerStyle;
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
                "{} left the output file empty or unchanged, so nothing was resolved",
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

/// A merge tool as configured: `[merge-tools.<name>]` over the definitions
/// jj-cli ships.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeToolSpec {
    pub name: String,
    pub program: String,
    pub merge_args: Vec<String>,
    pub edits_markers: bool,
    /// `conflict-marker-style` override.
    pub marker_style: Option<ConflictMarkerStyle>,
    pub conflict_exit_codes: Vec<i32>,
}

impl MergeToolSpec {
    /// Whether the arguments ask for `$marker_length`.
    pub fn uses_marker_length(&self) -> bool {
        find_all_variables(&self.merge_args).any(|name| name == "marker_length")
    }
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
/// runnable tool, with jj's rules: a bare name looks up `merge-tools.<name>`,
/// anything else is a command line whose arguments must include the file
/// variables.
pub fn merge_tool_from_settings(settings: &UserSettings) -> Result<MergeToolSpec, String> {
    let setting: Option<CommandNameAndArgs> = settings
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
    let (name, tool) = match setting.as_str() {
        Some(":builtin") => {
            return Err("ui.merge-editor is jj's built-in editor, which runs in a \
                        terminal. Set it to a GUI tool (vscode, meld, kdiff3, smerge) \
                        to resolve from diffui, or run `jj resolve` in a terminal."
                .to_owned());
        }
        Some(name @ (":ours" | ":theirs")) => {
            return Err(format!(
                "ui.merge-editor is {name}; use \"Take side\" on the file instead"
            ));
        }
        Some(name) if terminal_tool(name, name) => return Err(terminal_tool_error(name)),
        Some(name) => {
            let tool = get_external_tool_config(settings, name)
                .map_err(|error| format!("invalid merge-tools.{name}: {error}"))?;
            (
                name.to_owned(),
                tool.unwrap_or_else(|| ExternalMergeTool::with_merge_args(&setting)),
            )
        }
        None => (
            setting.split_name().into_owned(),
            ExternalMergeTool::with_merge_args(&setting),
        ),
    };
    if tool.program.is_empty() {
        return Err("ui.merge-editor is empty".to_owned());
    }
    if tool.merge_args.is_empty() {
        return Err(format!(
            "merge tool \"{name}\" has no merge-args (set merge-tools.{name}.merge-args)"
        ));
    }
    if terminal_tool(&name, &tool.program) {
        return Err(terminal_tool_error(&name));
    }
    Ok(MergeToolSpec {
        name,
        program: tool.program,
        merge_args: tool.merge_args,
        edits_markers: tool.merge_tool_edits_conflict_markers,
        marker_style: tool.conflict_marker_style,
        conflict_exit_codes: tool.merge_conflict_exit_codes,
    })
}

fn terminal_tool_error(name: &str) -> String {
    format!(
        "{name} runs in a terminal, which diffui doesn't have. Run `jj resolve` in a \
         terminal, or set ui.merge-editor to a GUI tool (vscode, meld, kdiff3, smerge)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// jj-cli's shipped config with `toml` as the user's on top.
    fn settings(toml: &str) -> UserSettings {
        let mut config = jj_lib::config::StackedConfig::with_defaults();
        config.extend_layers(jj_cli::config::default_config_layers());
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
        assert_eq!(spec.marker_style, Some(ConflictMarkerStyle::Git));
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

    /// Every tool jj-cli ships merge arguments for resolves to exactly jj-cli's
    /// definition, with nothing of diffui's own layered in.
    #[test]
    fn named_tools_are_jj_clis_definitions() {
        let shipped = settings("");
        for name in jj_cli::merge_tools::configured_merge_tools(&shipped) {
            let Some(tool) = get_external_tool_config(&shipped, name).expect("tool config") else {
                continue;
            };
            if tool.merge_args.is_empty() || terminal_tool(name, &tool.program) {
                continue;
            }
            let spec =
                merge_tool_from_settings(&settings(&format!("ui.merge-editor = \"{name}\"")))
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(spec.program, tool.program, "{name}");
            assert_eq!(spec.merge_args, tool.merge_args, "{name}");
            assert_eq!(spec.conflict_exit_codes, tool.merge_conflict_exit_codes);
            assert_eq!(spec.edits_markers, tool.merge_tool_edits_conflict_markers);
            assert_eq!(spec.marker_style, tool.conflict_marker_style);
        }
        let mergiraf =
            merge_tool_from_settings(&settings("ui.merge-editor = \"mergiraf\"")).unwrap();
        assert_eq!(mergiraf.conflict_exit_codes, vec![1]);
        assert!(mergiraf.uses_marker_length());
    }
}

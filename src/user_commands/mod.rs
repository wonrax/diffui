//! Declared commands: the `[[commands]]` rows of `config.toml`, and the ones
//! diffui ships the same way (`assets/commands.toml`). A row names what to
//! run (a program, a jj line, the user's agent CLI or their editor), where it
//! applies (a when-clause) and where it's offered (the palette, the revision,
//! bookmark and file menus, a key). The registry lists each beside the
//! built-ins; running one gathers its prompts, asks its confirm, fills its
//! variables in and runs it on the tab it was started from.
//!
//! The schema, the variables and the when-clause keys are documented for
//! users (and agents) in `assets/agent/SKILL.md`; keep the two in step.

mod run;
mod spec;
mod template;

use std::borrow::Cow;
use std::time::Duration;

#[cfg(test)]
pub(crate) use run::jj_line;
pub(crate) use run::{Generation, RunId, Runs};
pub(crate) use spec::{SHIPPED, parse_commands};
pub(crate) use template::Var;

use crate::Diffui;
use crate::commands::{CommandArg, Disabled, reason};
use crate::when::{AppKeys, When, context};
use template::Template;

/// One declared command.
#[derive(Debug)]
pub(crate) struct UserCommand {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) hint: String,
    /// The palette heading it lists under.
    pub(crate) group: &'static str,
    /// The clause as written, and parsed.
    pub(crate) when: Option<(String, When)>,
    pub(crate) places: Places,
    /// Chords, bound in the base context.
    pub(crate) keys: Vec<String>,
    pub(crate) step: Step,
    pub(crate) stdin: Option<Template>,
    pub(crate) output: Output,
    pub(crate) confirm: Option<Template>,
    pub(crate) prompts: Vec<Prompt>,
    /// `override = true`: it may take a built-in's id.
    pub(crate) replaces: bool,
    pub(crate) timeout: Duration,
    /// Where it was declared, `path:line:col`.
    pub(crate) source: String,
}

/// What running a declared command does.
#[derive(Debug)]
pub(crate) enum Step {
    /// A program and its arguments, one template per argument. No shell.
    Exec(Vec<Template>),
    /// A jj command line, split into words before substitution, run the way
    /// the palette runs it.
    Run(Vec<Template>),
    /// The user's agent CLI (`[ai] command`, else `claude -p`, else
    /// `codex exec`) with this prompt as its last argument.
    Agent(Template),
    /// The user's editor (`[editor] command`, else `$VISUAL` / `$EDITOR`
    /// when it's a GUI editor, else the system's opener) on this path, at
    /// `{line}`.
    Edit(Template),
}

/// Where a run's standard output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// The activity row and its output panel.
    Activity,
    /// A toast, once it exits.
    Toast,
    /// The clipboard, once it exits.
    Clipboard,
    /// The description editor, as it arrives.
    Description,
    /// The browser, its first line once it exits.
    OpenUrl,
    None,
}

impl Output {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "activity" => Output::Activity,
            "toast" => Output::Toast,
            "clipboard" => Output::Clipboard,
            "description" => Output::Description,
            "open-url" => Output::OpenUrl,
            "none" => Output::None,
            _ => return None,
        })
    }
}

/// A question a run asks before it starts.
#[derive(Debug, Clone)]
pub(crate) struct Prompt {
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) kind: PromptKind,
}

#[derive(Debug, Clone)]
pub(crate) enum PromptKind {
    /// A line of text, in the name prompt.
    Input,
    /// Yes or no, in the confirm dialog; no stops the run.
    Confirm,
    /// One of these, in a palette column.
    Choose(Vec<String>),
}

/// Where a command is offered, besides its keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Places {
    /// The palette's top level.
    pub(crate) palette: bool,
    /// The revision context menu, and a revision's actions in the palette.
    pub(crate) revision: bool,
    /// A bookmark's menu, and a bookmark's actions in the palette.
    pub(crate) bookmark: bool,
    /// The file context menu, and a file's actions in the palette.
    pub(crate) file: bool,
}

impl UserCommand {
    /// Every template it runs with.
    fn templates(&self) -> impl Iterator<Item = &Template> {
        let step: Vec<&Template> = match &self.step {
            Step::Exec(words) | Step::Run(words) => words.iter().collect(),
            Step::Agent(word) | Step::Edit(word) => vec![word],
        };
        step.into_iter()
            .chain(self.stdin.as_ref())
            .chain(self.confirm.as_ref())
    }

    /// Every variable it needs, each once.
    pub(crate) fn vars(&self) -> Vec<&Var> {
        let mut vars: Vec<&Var> = Vec::new();
        let edit_line = matches!(self.step, Step::Edit(_)).then_some(&Var::Line);
        for var in self.templates().flat_map(Template::vars).chain(edit_line) {
            if !vars.contains(&var) {
                vars.push(var);
            }
        }
        vars
    }

    /// `Err` with why it can't run on `arg` now: its when-clause doesn't hold,
    /// or something it names isn't there.
    pub(crate) fn availability(&self, ui: &Diffui, arg: &CommandArg) -> Result<(), Disabled> {
        if let Some((text, when)) = &self.when
            && !when.eval(&AppKeys { ui, arg })
        {
            return Err(Cow::Owned(format!("Only when {text}")));
        }
        for var in self.vars() {
            value(ui, arg, var).map_err(Cow::Borrowed)?;
        }
        match &self.step {
            Step::Run(_) => crate::commands::require(
                ui.active()
                    .repository
                    .as_ref()
                    .is_some_and(|repository| repository.vcs == crate::Vcs::Jj),
                reason::NOT_JJ,
            )
            .map_err(Cow::Borrowed),
            // Only the working copy is on disk.
            Step::Edit(path) => {
                let values = context_values(ui, arg, &self.vars());
                let on_disk = path
                    .render(&values)
                    .ok()
                    .zip(values.get(&Var::Root))
                    .is_some_and(|(path, root)| std::path::Path::new(root).join(path).exists());
                crate::commands::require(on_disk, "Not in the working copy").map_err(Cow::Borrowed)
            }
            Step::Exec(_) | Step::Agent(_) => Ok(()),
        }
    }
}

/// `var`'s value on `arg` now: `Ok(None)` for one that's read or asked for
/// when the command runs, `Err` with why it has none here.
pub(crate) fn value(
    ui: &Diffui,
    arg: &CommandArg,
    var: &Var,
) -> Result<Option<String>, &'static str> {
    const NO_REVISION: &str = "No revision here";
    let tab = ui.active();
    let root = || {
        tab.repository
            .as_ref()
            .map(|repository| repository.root.display().to_string())
            .ok_or(reason::NO_REPOSITORY)
    };
    Ok(Some(match var {
        Var::Root => root()?,
        Var::ChangeId => context::revision_row(ui, arg)
            .ok_or(NO_REVISION)?
            .change_id()
            .to_owned(),
        Var::CommitId => context::revision_row(ui, arg)
            .ok_or(NO_REVISION)?
            .commit_id()
            .to_owned(),
        Var::Description => context::revision_row(ui, arg)
            .ok_or(NO_REVISION)?
            .description()
            .to_owned(),
        Var::Bookmark => context::bookmark(ui, arg).ok_or("No bookmark here")?,
        Var::Path => context::file(ui, arg).ok_or(reason::NO_FILE)?,
        Var::Line => {
            let path = context::file(ui, arg).ok_or(reason::NO_FILE)?;
            line_in(ui, &path).to_string()
        }
        Var::Diff => {
            root()?;
            context::revision_row(ui, arg).ok_or(NO_REVISION)?;
            return Ok(None);
        }
        Var::RecentDescriptions => {
            crate::commands::require(tab.session.capabilities.mutate, reason::NOT_JJ)?;
            return Ok(None);
        }
        Var::Prompt(_) => return Ok(None),
    }))
}

/// The values of `vars` that can be read off the app now.
pub(crate) fn context_values(ui: &Diffui, arg: &CommandArg, vars: &[&Var]) -> template::Values {
    vars.iter()
        .filter_map(|var| Some(((*var).clone(), value(ui, arg, var).ok()??)))
        .collect()
}

/// The line `{line}` names in `path`: line-select mode's cursor when it's in
/// that file, else the file's first changed line, else 1.
fn line_in(ui: &Diffui, path: &str) -> usize {
    if let Some((cursor_path, line)) = context::cursor_line(ui)
        && cursor_path == path
    {
        return line;
    }
    ui.active()
        .session
        .document
        .files
        .iter()
        .find(|file| file.path == path)
        .and_then(|file| {
            file.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .find(|line| {
                    matches!(
                        line.kind,
                        diffui_core::DiffLineKind::Addition | diffui_core::DiffLineKind::Deletion
                    )
                })
                .and_then(|line| line.new_line.or(line.old_line))
        })
        .unwrap_or(1)
}

//! Typed jj commands: what the command line in the palette speaks.
//!
//! A line such as `jj rebase -s xyz -o 'trunk()'` goes through
//! [`JjCli::parse`]:
//!
//! 1. shell-style tokenising ([`tokenize`]), keeping byte spans;
//! 2. alias and default-command expansion, as jj-cli 0.45.1 does it, from
//!    the user's `[aliases]` and `ui.default-command` ([`CliConfig`]);
//! 3. validation against jj-cli's own clap tree
//!    (`jj_cli::commands::default_app()`), so flags, defaults and errors are
//!    jj's;
//! 4. classification into a [`CliCommand`]: a view diffui shows, a native
//!    mutation (the same `MutationOp` the GUI builds; lower it with
//!    [`lower`] once its revsets are resolved), something that opens
//!    diffui's own editor, or a passthrough run of the real CLI
//!    ([`passthrough`]).
//!
//! The other direction, op → command, is
//! [`MutationOp::cli_equivalent`](crate::MutationOp::cli_equivalent), and
//! [`OpDraft::cli_prefix`](crate::OpDraft::cli_prefix) for a target-mode
//! draft still picking its destination.
//!
//! [`JjCli::complete`] completes subcommands and flags and hands value
//! positions to a [`ValueCompleter`](crate::completion::ValueCompleter).

mod command;
mod complete;
mod grammar;
mod lower;
mod parse;
pub mod passthrough;
mod render;
pub mod tokenize;

use std::path::Path;

pub use command::{
    CliCommand, CommandKind, MutationCommand, RevsetArg, RevsetDestination, RevsetSlot, Spanned,
    UiCommand, ViewCommand,
};
pub use lower::{LowerContext, LowerError, NativeAction, lower};
pub use parse::{CliConfig, ParseError, ParsedCommand};
pub use passthrough::{CliInvocation, CliLine, CliStream};
pub use render::{CliEquivalent, CommitIds, RevisionNames, jj_args_tag};

use crate::completion::{Completion, ValueCompleter};

/// The command line for one repository: jj's grammar plus that
/// repository's aliases. Cheap to clone; the grammar is shared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JjCli {
    config: CliConfig,
}

impl JjCli {
    pub fn new(config: CliConfig) -> Self {
        Self { config }
    }

    /// With the user's jj config for the workspace at `repo_root`, falling
    /// back to jj's defaults if it doesn't load.
    pub fn for_repo(repo_root: &Path) -> Self {
        Self::new(CliConfig::load(repo_root).unwrap_or_default())
    }

    pub fn config(&self) -> &CliConfig {
        &self.config
    }

    /// Parse a typed line; the leading `jj` is optional.
    pub fn parse(&self, line: &str) -> Result<ParsedCommand, ParseError> {
        parse::parse(&self.config, line)
    }

    /// The revset arguments of `line` as far as jj's grammar places them,
    /// also while it doesn't parse yet (see [`parse::revset_slots`]).
    pub fn revset_slots(&self, line: &str) -> Vec<RevsetSlot> {
        parse::revset_slots(&self.config, line)
    }

    /// Candidates for the word at `cursor` (a byte offset into `line`).
    pub fn complete(
        &self,
        line: &str,
        cursor: usize,
        values: &dyn ValueCompleter,
    ) -> Vec<Completion> {
        complete::complete(&self.config, line, cursor, values)
    }
}

/// [`JjCli::complete`] with jj's default aliases only.
pub fn complete_command(line: &str, cursor: usize, values: &dyn ValueCompleter) -> Vec<Completion> {
    complete::complete(&CliConfig::builtin(), line, cursor, values)
}

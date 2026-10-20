//! The typed result of a parsed `jj …` line: which of the four kinds it is
//! and, for the kinds diffui runs itself, its arguments. Revset arguments
//! stay revset strings here; [`super::lower`] turns them into revisions once
//! the caller has resolved them.

use std::ops::Range;

use crate::completion::ValueKind;
use crate::mutations::RebaseSourceMode;
use crate::push::PushAllow;

/// A value and the bytes of the typed line it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    pub value: T,
    pub span: Range<usize>,
}

/// One revset argument. `span` is `None` for jj's implicit default (the `@`
/// of a bare `jj new`), which has no text to underline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevsetArg {
    pub revset: String,
    pub span: Option<Range<usize>>,
}

impl RevsetArg {
    pub(crate) fn implicit(revset: &str) -> Self {
        Self {
            revset: revset.to_owned(),
            span: None,
        }
    }

    /// Whether this is the default `@` rather than something typed.
    pub fn is_implicit(&self) -> bool {
        self.span.is_none()
    }
}

/// Every revset argument of a command, with the flag it was given to — for
/// live resolution and graph tinting, whatever the command's kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevsetSlot {
    /// The flag as jj spells it (`--onto`), or the positional's placeholder
    /// (`REVSETS`).
    pub flag: String,
    pub revset: String,
    pub span: Range<usize>,
}

/// One argument value of a line and the kind jj's grammar gives it, for
/// drawing a command by what its words name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueSlot {
    pub kind: ValueKind,
    pub value: String,
    pub span: Range<usize>,
}

/// The badge a command line gets: what Enter will do, in one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    /// Drives a view diffui already has (diff pane, graph revset, history).
    View,
    /// Becomes the same mutation (or target-mode draft) the GUI builds, so it
    /// can be previewed before it runs.
    Mutation,
    /// Opens diffui's own editor or flow, where jj would open `$EDITOR`.
    OpensUi,
    /// Runs the jj CLI in a subprocess and shows its output.
    Passthrough,
}

impl CommandKind {
    /// The badge text the design uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::View => "View",
            Self::Mutation => "Preview",
            Self::OpensUi => "Opens editor",
            Self::Passthrough => "CLI",
        }
    }
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    View(ViewCommand),
    Mutation(MutationCommand),
    OpensUi(UiCommand),
    /// Anything diffui doesn't run itself, including the native commands
    /// with a flag diffui's own op can't express. Run
    /// [`ParsedCommand::passthrough_args`] through the jj CLI.
    Passthrough,
}

impl CliCommand {
    pub fn kind(&self) -> CommandKind {
        match self {
            Self::View(_) => CommandKind::View,
            Self::Mutation(_) => CommandKind::Mutation,
            Self::OpensUi(_) => CommandKind::OpensUi,
            Self::Passthrough => CommandKind::Passthrough,
        }
    }
}

/// A command that shows something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewCommand {
    /// `jj diff -r REVSETS`: the combined diff of the revisions (default
    /// `@`). Several `-r` are unioned, like jj does.
    Diff { revisions: Vec<RevsetArg> },
    /// `jj diff --from A --to B` (either defaults to `@`).
    DiffRange { from: RevsetArg, to: RevsetArg },
    /// `jj interdiff --from A --to B` (either defaults to `@`).
    Interdiff { from: RevsetArg, to: RevsetArg },
    /// `jj show REVSET` (default `@`).
    Show { revision: RevsetArg },
    /// `jj log -r REVSETS`; `None` for the configured default log revset.
    /// Several `-r` are unioned.
    Log { revisions: Option<Vec<RevsetArg>> },
    /// `jj evolog -r REVSET` (default `@`).
    Evolog { revision: RevsetArg },
    /// `jj op log`, optionally `-n LIMIT`.
    OpLog { limit: Option<usize> },
    /// `jj file annotate PATH -r REVSET` (default `@`).
    FileAnnotate {
        path: Spanned<String>,
        revision: RevsetArg,
    },
}

/// Where `-o` / `-A` / `-B` put the result, at revset level. Mirrors
/// [`crate::mutations::Destination`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevsetDestination {
    Onto(RevsetArg),
    After(RevsetArg),
    Before(RevsetArg),
    /// `-A after -B before`.
    Between {
        after: RevsetArg,
        before: RevsetArg,
    },
}

impl RevsetDestination {
    /// The revision target mode should focus: what the destination is
    /// anchored on.
    pub fn anchor(&self) -> &RevsetArg {
        match self {
            Self::Onto(target) | Self::After(target) | Self::Before(target) => target,
            Self::Between { after, .. } => after,
        }
    }
}

/// A native mutation, at revset level. Lower it with
/// [`super::lower::lower`] once the revsets are resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationCommand {
    /// `jj new [REVSETS]` (default `@`). One revision is a plain new, several
    /// a merge.
    New {
        parents: Vec<RevsetArg>,
    },
    /// `jj new -A/-B`.
    NewAt {
        destination: RevsetDestination,
    },
    Edit {
        target: RevsetArg,
    },
    /// `jj abandon [REVSETS]` (default `@`).
    Abandon {
        targets: Vec<RevsetArg>,
    },
    /// `jj describe REVSET -m MESSAGE`. Several `-m` are joined as
    /// paragraphs, as jj does.
    Describe {
        target: RevsetArg,
        message: String,
    },
    /// `jj commit -m MESSAGE`.
    Commit {
        message: String,
    },
    /// `jj rebase -r/-s/-b SOURCES -o/-A/-B DESTINATION`; with no source
    /// flag jj rebases `-b @`.
    Rebase {
        mode: RebaseSourceMode,
        sources: Vec<RevsetArg>,
        destination: RevsetDestination,
    },
    /// `jj squash`, `jj squash -r X` (`into` is `None`: X's parent), or
    /// `jj squash --from A --into B` (each defaults to `@`).
    Squash {
        from: Vec<RevsetArg>,
        into: Option<RevsetArg>,
    },
    /// `jj duplicate [REVSETS] [-o/-A/-B]`.
    Duplicate {
        targets: Vec<RevsetArg>,
        destination: Option<RevsetDestination>,
    },
    Parallelize {
        targets: Vec<RevsetArg>,
    },
    /// `jj absorb [--from REVSET]`.
    Absorb {
        from: RevsetArg,
    },
    /// `jj metaedit --update-author/--update-author-timestamp [REVSETS]`.
    Metaedit {
        targets: Vec<RevsetArg>,
        update_author: bool,
        update_author_timestamp: bool,
    },
    BookmarkCreate {
        name: Spanned<String>,
        to: RevsetArg,
    },
    /// `jj bookmark set NAME -r REVSET` and `jj bookmark move NAME --to
    /// REVSET`. Without `allow_backwards` jj refuses a backwards or sideways
    /// move; run [`crate::Command::BookmarkCheck`] first to honour that.
    BookmarkSet {
        name: Spanned<String>,
        to: RevsetArg,
        allow_backwards: bool,
    },
    BookmarkDelete {
        name: Spanned<String>,
    },
    BookmarkForget {
        name: Spanned<String>,
    },
    BookmarkRename {
        old: Spanned<String>,
        new: Spanned<String>,
    },
    BookmarkTrack {
        name: String,
        remote: String,
    },
    BookmarkUntrack {
        name: String,
        remote: String,
    },
    /// `jj git push`. `remote: None` is jj's default (`git.push`, else
    /// `origin`). No `bookmarks` and no `changes` pushes the tracked
    /// bookmarks that moved.
    Push {
        remote: Option<String>,
        bookmarks: Vec<Spanned<String>>,
        changes: Vec<RevsetArg>,
        allow: PushAllow,
    },
    /// `jj git fetch`.
    Fetch,
    /// `jj git fetch --all-remotes`.
    FetchAll,
    /// `jj git fetch --remote R --branch B`.
    FetchBranch {
        remote: String,
        branch: String,
    },
    Undo,
    Redo,
    /// `jj op restore OPERATION`.
    OpRestore {
        operation: Spanned<String>,
    },
    /// `jj op revert [OPERATION]` (default `@`, the latest operation).
    OpRevert {
        operation: Option<Spanned<String>>,
    },
}

/// A command jj would answer by opening an editor; diffui opens its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    /// `jj describe [REVSET]` without `-m`.
    Describe { target: RevsetArg },
    /// `jj commit` without `-m`.
    Commit,
    /// `jj split [-r REVSET]` without paths or `-m`.
    Split { target: RevsetArg },
    /// `jj resolve [-r REVSET] [PATHS]`.
    Resolve {
        target: RevsetArg,
        paths: Vec<Spanned<String>>,
    },
}

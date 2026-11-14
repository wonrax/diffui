//! The messages a plugin and its host exchange, as plain serde data.
//!
//! Nothing here borrows from the app or from jj-lib: every type is owned and
//! serialises to the JSON the script sees (camelCase fields), so the same
//! protocol can later run over a pipe to an out-of-process plugin.
//! `assets/diffui.d.ts` describes the script's side of each type.

use serde::{Deserialize, Serialize};

/// The repository a call is about, as the host named it.
///
/// `id` is whatever the host minted for the tab's repository; `op` is the jj
/// operation that was on screen when the handle was made. The host rejects a
/// call whose `op` is no longer current for `id` (a stale handle), so a late
/// reply can't act on another tab or on a graph the user no longer sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub id: String,
    pub op: String,
}

/// What the host hands a plugin with every call: the reference plus the
/// fields a script can read without asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoHandle {
    pub id: String,
    pub op: String,
    /// The workspace root, absolute.
    pub root: String,
    /// The remote a forge plugin was matched on, parsed by the host. `None`
    /// when no remote belongs to a forge this plugin provides.
    #[serde(default)]
    pub forge_remote: Option<ForgeRemote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgeRemote {
    /// Host as in the remote's URL, with its port when it has one.
    pub host: String,
    /// Owner or group path (`team`, `group/subgroup`).
    pub owner: String,
    pub name: String,
    /// The project's page, scheme included (`https://codeberg.org/team/app`).
    pub web_url: String,
}

/// A call from the plugin that only the app can answer. Everything else a
/// script can do (`http`, `exec`, `auth`, `kv`, `console`, `config`) is
/// handled inside this crate and never reaches [`crate::Host`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "camelCase")]
pub enum HostCall {
    Repo { repo: RepoRef, call: RepoCall },
    Ui(UiCall),
}

/// Reads and jj commands on one repository. The reply each expects is named
/// on the variant; send it with [`crate::Reply::ok`].
///
/// The manifest's `jj` permission is enforced before the host sees the call:
/// every variant needs `read`, [`RepoCall::JjRun`] needs `write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum RepoCall {
    /// Reply: `Vec<`[`Bookmark`]`>`.
    Bookmarks,
    /// Reply: `Vec<`[`Remote`]`>`.
    Remotes,
    /// Reply: [`Selection`].
    Selection,
    /// Reply: `Vec<`[`Row`]`>`, newest first, at most `limit` (100 unless
    /// the script asked otherwise).
    Rows { revset: String, limit: u32 },
    /// Reply: [`Diff`], cut at `max_bytes` (1 MB unless the script asked
    /// otherwise).
    #[serde(rename_all = "camelCase")]
    Diff {
        revset: String,
        format: DiffFormat,
        max_bytes: u64,
    },
    /// Reply: the file's text, or `null` when `rev` has no such file.
    ReadFile { rev: String, path: String },
    /// A read-only jj command line (`["log", "-r", "@"]`), run at the
    /// handle's operation without snapshotting. Reply: its stdout as a string;
    /// a failed command is an error carrying stderr.
    JjQuery { args: Vec<String> },
    /// A jj command line that changes the repository, run the way the
    /// command palette runs one. Reply: [`JjRun`].
    JjRun { args: Vec<String> },
}

impl RepoCall {
    /// The script-side name, for messages (`repo.jj.run`).
    pub(crate) fn name(&self) -> &'static str {
        match self {
            RepoCall::Bookmarks => "repo.bookmarks",
            RepoCall::Remotes => "repo.remotes",
            RepoCall::Selection => "repo.selection",
            RepoCall::Rows { .. } => "repo.rows",
            RepoCall::Diff { .. } => "repo.diff",
            RepoCall::ReadFile { .. } => "repo.readFile",
            RepoCall::JjQuery { .. } => "repo.jj.query",
            RepoCall::JjRun { .. } => "repo.jj.run",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffFormat {
    #[default]
    Git,
    Stat,
    Summary,
    NameOnly,
}

/// The UI slots a plugin can fill. Plugins never draw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum UiCall {
    /// Reply: `null`.
    Toast { message: String, tone: Tone },
    /// Start a row in the activity log. Reply: an activity id (a number) for
    /// the two calls below.
    ActivityStart { label: String },
    /// Reply: `null`.
    ActivityAppend { activity: u64, text: String },
    /// Reply: `null`. `error` marks the row failed and is shown with it.
    ActivityFinish {
        activity: u64,
        error: Option<String>,
    },
    /// Reply: the text, or `null` when dismissed.
    PromptInput {
        label: String,
        value: Option<String>,
        placeholder: Option<String>,
    },
    /// Reply: `true` or `false`.
    #[serde(rename_all = "camelCase")]
    PromptConfirm {
        title: String,
        body: Option<String>,
        confirm_label: Option<String>,
        #[serde(default)]
        destructive: bool,
    },
    /// Reply: the picked choice's `value`, or `null` when dismissed.
    PromptChoose { label: String, choices: Vec<Choice> },
    /// Reply: `null`. The host opens `http` and `https` links only.
    OpenUrl { url: String },
    /// Reply: `null`.
    ClipboardWrite { text: String },
    /// Set or clear (`None`) this plugin's status-bar item. Reply: `null`.
    StatusSet { text: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tone {
    #[default]
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    #[serde(default)]
    pub hint: Option<String>,
    /// What `choose` resolves to. Defaults to the label.
    pub value: String,
}

/// A call from the host into the plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PluginCall {
    /// Run a command the manifest declares. Reply: `null` once the handler's
    /// promise settles.
    Command { id: String, context: CommandContext },
    /// Ask the registered forge provider. The reply is named on [`ForgeCall`].
    Forge { repo: RepoHandle, call: ForgeCall },
    /// Deliver an event to its listeners. Fire and forget: the reply is
    /// `null` as soon as the listeners were called, and what they throw goes
    /// to the plugin's log.
    Event { event: Event },
}

/// Where a command was invoked: what the template variables of a command
/// declared in config.toml hold (`{change_id}`, `{bookmark}`, `{path}`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandContext {
    pub repo: RepoHandle,
    #[serde(default)]
    pub change_id: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
    #[serde(default)]
    pub bookmark: Option<String>,
    /// Repo-relative path of the file in context.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum ForgeCall {
    /// Reply: `Vec<`[`PullRequest`]`>` whose head is one of `branches`.
    PullRequests { branches: Vec<String> },
    /// Reply: [`Checks`] for the commit.
    Checks { sha: String },
    /// Reply: the new [`PullRequest`].
    CreatePullRequest { request: CreatePullRequest },
    /// Reply: the updated [`PullRequest`].
    UpdatePullRequest {
        number: u64,
        patch: UpdatePullRequest,
    },
}

impl ForgeCall {
    /// The provider method this calls, as the script names it.
    pub fn method(&self) -> &'static str {
        match self {
            ForgeCall::PullRequests { .. } => "pullRequests",
            ForgeCall::Checks { .. } => "checks",
            ForgeCall::CreatePullRequest { .. } => "createPullRequest",
            ForgeCall::UpdatePullRequest { .. } => "updatePullRequest",
        }
    }
}

/// The provider methods a script can register, in the order the host lists
/// them.
pub const FORGE_METHODS: [&str; 4] = [
    "pullRequests",
    "checks",
    "createPullRequest",
    "updatePullRequest",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePullRequest {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub draft: bool,
}

/// Only the fields present change.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpdatePullRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// What the app tells plugins about. Each carries the repository it happened
/// in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "camelCase")]
pub enum Event {
    RepoOpened {
        repo: RepoHandle,
    },
    RefsChanged {
        repo: RepoHandle,
    },
    /// The repository is at another operation than the one the plugin last
    /// saw it at.
    OperationChanged {
        repo: RepoHandle,
    },
    SelectionChanged {
        repo: RepoHandle,
    },
    Fetched {
        repo: RepoHandle,
    },
    Pushed {
        repo: RepoHandle,
        bookmarks: Vec<String>,
    },
    /// A command ran, built-in or not. `id` is its registry id.
    CommandInvoked {
        repo: RepoHandle,
        id: String,
    },
}

impl Event {
    /// The name a script subscribes with.
    pub fn name(&self) -> &'static str {
        match self {
            Event::RepoOpened { .. } => "repoOpened",
            Event::RefsChanged { .. } => "refsChanged",
            Event::OperationChanged { .. } => "operationChanged",
            Event::SelectionChanged { .. } => "selectionChanged",
            Event::Fetched { .. } => "fetched",
            Event::Pushed { .. } => "pushed",
            Event::CommandInvoked { .. } => "commandInvoked",
        }
    }
}

/// Every event name, for `events.on`'s check and the type definitions.
pub const EVENT_NAMES: [&str; 7] = [
    "repoOpened",
    "refsChanged",
    "operationChanged",
    "selectionChanged",
    "fetched",
    "pushed",
    "commandInvoked",
];

// Replies.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub name: String,
    /// Commit ids the local bookmark points at: one normally, several when
    /// conflicted, none when it only exists on a remote.
    pub targets: Vec<String>,
    pub remotes: Vec<RemoteBookmark>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBookmark {
    pub remote: String,
    pub target: String,
    pub tracked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Selection {
    /// The selected revisions, the cursor's first.
    pub revisions: Vec<RevisionRef>,
    /// The file open in the diff, repo-relative.
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionRef {
    pub change_id: String,
    pub commit_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub change_id: String,
    pub commit_id: String,
    /// The first line of the description.
    pub description: String,
    pub author: String,
    /// Author time in epoch milliseconds.
    pub timestamp: Option<i64>,
    pub bookmarks: Vec<String>,
    pub working_copy: bool,
    pub immutable: bool,
    pub empty: bool,
    pub conflict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub text: String,
    /// The diff was longer than `maxBytes` and was cut.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JjRun {
    /// What the command did, in the words the command palette would use.
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub title: String,
    /// Head branch name.
    pub head: String,
    /// Base branch name.
    pub base: String,
    pub head_sha: String,
    pub state: PrState,
    #[serde(default)]
    pub review: Option<Review>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PrState {
    Open,
    Draft,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Review {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checks {
    pub state: CheckState,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckState {
    Pending,
    Success,
    Failure,
    None,
}

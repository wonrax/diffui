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
    /// Ask the registered AI provider. The reply is named on [`AiCall`].
    Ai { call: AiCall },
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
    /// Hosts the forge claims beyond its manifest's, asked once after it
    /// loads. Reply: `Vec<String>`, each a host as `[provides] forge.hosts`
    /// writes one.
    Hosts,
    /// The pull-request tab's header. Reply: [`PrInfo`].
    View { pr: PrRef },
    /// The pull request's commits, oldest first. Reply: `Vec<`[`PrCommit`]`>`.
    Commits { pr: PrRef },
    /// One commit's diff. Streams [`DiffPart`]s through the call's parts,
    /// then replies [`CommitDiff`].
    CommitDiff { pr: PrRef, oid: String },
    /// The whole pull request's diff. Streams [`DiffPart`]s through the
    /// call's parts, then replies `null`.
    Diff { pr: PrRef },
    /// The pull request's review threads, read-only. Reply:
    /// `Vec<`[`ReviewThread`]`>`.
    ReviewComments { pr: PrRef },
    /// Who the user is on `host` and how the forge reaches it, or, with
    /// `token`, who that token belongs to. Reply: [`Account`].
    Account {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<Secret>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum AiCall {
    /// Write a reply to `request`. Streams the text through the call's
    /// parts, each a string, then replies `null`.
    Complete { request: AiRequest },
    /// The models the provider offers. Reply: `Vec<`[`AiModel`]`>`.
    Models,
    /// What the provider runs as. Reply: [`AiAccount`].
    Account,
}

impl AiCall {
    /// The provider method this calls, as the script names it.
    pub fn method(&self) -> &'static str {
        match self {
            AiCall::Complete { .. } => "complete",
            AiCall::Models => "models",
            AiCall::Account => "account",
        }
    }
}

/// The provider methods a script can register: `complete` is required.
pub const AI_METHODS: [&str; 3] = ["complete", "models", "account"];

/// What an AI provider is asked to answer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiRequest {
    /// The instructions.
    pub prompt: String,
    /// What they are about (a diff): a command-line provider's stdin.
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub system: Option<String>,
    /// A model other than the one the provider's settings name.
    #[serde(default)]
    pub model: Option<String>,
    /// The repository's root, where a command-line provider runs.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiModel {
    /// What the provider's `model` setting takes.
    pub id: String,
    /// A name to show beside it.
    #[serde(default)]
    pub name: Option<String>,
}

/// What a provider runs as: a program's version, whose key it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiAccount {
    /// One line for Settings: "claude 2.1.4", "A key for Acme".
    pub label: String,
}

/// A token on its way to the script: never printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// How a forge reaches a host, and as whom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// `cli`, `token`, or `None` when nothing signs in to the host.
    pub method: Option<AccountMethod>,
    pub login: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountMethod {
    Cli,
    Token,
}

impl ForgeCall {
    /// The provider method this calls, as the script names it.
    pub fn method(&self) -> &'static str {
        match self {
            ForgeCall::PullRequests { .. } => "pullRequests",
            ForgeCall::Checks { .. } => "checks",
            ForgeCall::CreatePullRequest { .. } => "createPullRequest",
            ForgeCall::UpdatePullRequest { .. } => "updatePullRequest",
            ForgeCall::Hosts => "hosts",
            ForgeCall::View { .. } => "view",
            ForgeCall::Commits { .. } => "commits",
            ForgeCall::CommitDiff { .. } => "commitDiff",
            ForgeCall::Diff { .. } => "diff",
            ForgeCall::ReviewComments { .. } => "reviewComments",
            ForgeCall::Account { .. } => "account",
        }
    }

    /// Whether the call streams parts before it settles. Its time limit is
    /// then the longest wait between parts, not the whole call.
    pub fn streams(&self) -> bool {
        matches!(self, ForgeCall::CommitDiff { .. } | ForgeCall::Diff { .. })
    }
}

/// The provider methods a script can register, in the order the host lists
/// them.
pub const FORGE_METHODS: [&str; 11] = [
    "pullRequests",
    "checks",
    "createPullRequest",
    "updatePullRequest",
    "hosts",
    "view",
    "commits",
    "commitDiff",
    "diff",
    "reviewComments",
    "account",
];

/// The methods a pull request's tab needs, all of them.
pub const TAB_METHODS: [&str; 4] = ["view", "commits", "commitDiff", "diff"];

/// A pull request, as a tab names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrRef {
    /// As in the project's URL, with the port when it has one.
    pub host: String,
    pub owner: String,
    pub name: String,
    pub number: u64,
    /// The project's page (`https://github.com/team/app`).
    pub web_url: String,
}

/// A pull request's header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub author: String,
    pub state: PrState,
    pub base: String,
    pub head: String,
    /// Lines added and removed in the whole pull request, when the forge
    /// says. Without them the tab adds up the files.
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    #[serde(default)]
    pub changed_files: Option<u64>,
    pub url: String,
    /// The description, in Markdown.
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrCommit {
    /// The full commit id.
    pub oid: String,
    #[serde(default)]
    pub author: String,
    /// The first line of the message.
    pub headline: String,
    /// The whole message, headline included.
    #[serde(default)]
    pub message: Option<String>,
    /// When it was authored, as RFC 3339 (`2026-11-14T09:30:00Z`).
    #[serde(default)]
    pub authored_at: Option<String>,
    /// Lines the commit adds and removes, when the forge counts them.
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
}

/// A pull request's review thread: comments on lines (or a file) of its
/// diff, the first one and the replies under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    pub path: String,
    /// The side of the diff its lines are on.
    pub side: ReviewSide,
    /// The last line it is on, 1-based on `side` of the pull request's
    /// diff now; `None` for a comment on the whole file, or one whose lines
    /// the pull request no longer has.
    #[serde(default)]
    pub line: Option<u64>,
    /// The first line of a comment on several.
    #[serde(default)]
    pub start_line: Option<u64>,
    /// The last line when it was written, for an outdated thread.
    #[serde(default)]
    pub original_line: Option<u64>,
    /// The text of the lines it is on, as they were when it was written
    /// (from the comment's diff hunk), without the `+`/`-` mark.
    #[serde(default)]
    pub text: Vec<String>,
    /// The forge says its lines changed since.
    #[serde(default)]
    pub outdated: bool,
    #[serde(default)]
    pub resolved: bool,
    /// The thread's page on the forge.
    pub url: String,
    /// Oldest first.
    pub comments: Vec<ReviewComment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewSide {
    /// The base: lines the pull request removes.
    Old,
    /// The head.
    New,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewComment {
    #[serde(default)]
    pub author: String,
    /// Markdown.
    pub body: String,
    /// RFC 3339.
    #[serde(default)]
    pub created_at: Option<String>,
}

/// What `commitDiff` settles with, once its parts are all sent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitDiff {
    #[serde(default)]
    pub details: Option<CommitDetails>,
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CommitDetails {
    /// The whole message.
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub email: String,
    /// RFC 3339.
    #[serde(default)]
    pub date: Option<String>,
}

/// One piece of a streamed diff: unified `git diff` text, cut anywhere
/// (the host joins lines across pieces), or one file as a forge's API
/// lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DiffPart {
    Text(String),
    File(FilePatch),
}

/// One file of a diff: its hunks without a `diff --git` header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePatch {
    pub path: String,
    #[serde(default)]
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// `@@` hunks; absent for a binary or oversized file.
    #[serde(default)]
    pub patch: Option<String>,
    /// The forge's own counts, which win over counting the patch (an API
    /// that leaves a big file's patch out still counts its lines).
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
}

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

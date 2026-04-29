use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
};

mod diff_view;

use anyhow::{Context, Result, bail};
use arborium::{
    GrammarStore,
    advanced::{CompiledGrammar, ParseContext},
};
use bstr::BStr;
use clap::Parser;
use diff_view::{
    DiffFileView, DiffHunkView, DiffLine, DiffLineKind, DiffView, Palette, SyntaxKind, SyntaxSpan,
};
use futures::StreamExt;
use iced::{
    Background, Border, Color, Element, Font, Length, Shadow, Subscription, Task, Theme, alignment,
    keyboard, system, theme,
    widget::{
        button, column, container, row, scrollable, text,
        text::{Ellipsis, Wrapping},
        tooltip,
    },
};
use jj_lib::{
    backend::CommitId,
    config::StackedConfig,
    conflicts::{ConflictMarkerStyle, ConflictMaterializeOptions, materialized_diff_stream},
    copies::CopyRecords,
    diff_presentation::{
        LineCompareMode,
        unified::{DiffLineType, git_diff_part, unified_diff_hunks},
    },
    files::FileMergeHunkLevel,
    matchers::{EverythingMatcher, Matcher, PrefixMatcher},
    merge::{Diff, SameChange},
    object_id::ObjectId,
    repo::{Repo, StoreFactories},
    repo_path::{RepoPath, RepoPathBuf},
    settings::UserSettings,
    tree_merge::MergeOptions,
    workspace::{Workspace, default_working_copy_factories},
};
use tokio::process::Command;

#[cfg(target_os = "macos")]
const CODE_FONT: Font = Font::new("Menlo");
#[cfg(not(target_os = "macos"))]
const CODE_FONT: Font = Font::new("Cascadia Code");
const CODE_TEXT_SIZE: f32 = 13.0;
const CAPTION_TEXT_SIZE: f32 = 13.0;
const SMALL_TEXT_SIZE: f32 = 14.0;
const TITLE_TEXT_SIZE: f32 = 18.0;
const SIDEBAR_REVISION_ID_CHARS: usize = 12;
const SIDEBAR_COMMIT_ID_CHARS: usize = 12;
const PANEL_RADIUS: f32 = 3.0;
const CONTROL_RADIUS: f32 = 5.0;
const SIDEBAR_WIDTH: f32 = 360.0;
const SIDEBAR_FILE_BADGE_WIDTH: f32 = 24.0;
const SIDEBAR_FILE_STAT_MIN_WIDTH: f32 = 24.0;
const SIDEBAR_FILE_STAT_CHAR_WIDTH: f32 = 7.0;
const SIDEBAR_FILE_STAT_PADDING: f32 = 8.0;
const REVISION_CHIP_HEIGHT: f32 = 20.0;
const SIDEBAR_SCROLLBAR_WIDTH: f32 = 10.0;
const SIDEBAR_SCROLLBAR_SCROLLER_WIDTH: f32 = 7.0;
const SIDEBAR_SCROLLBAR_SPACING: f32 = 0.0;
const SIDEBAR_FILE_TEXT_CHAR_WIDTH: f32 = 7.0;
const SIDEBAR_FILE_TEXT_RESERVED_WIDTH: f32 =
    SIDEBAR_FILE_BADGE_WIDTH + SIDEBAR_FILE_STAT_MIN_WIDTH * 2.0 + 6.0 * 4.0 + 20.0;

fn main() -> iced::Result {
    let cli = Cli::parse();

    iced::application(
        move || Diffui::new(cli.clone()),
        Diffui::update,
        Diffui::view,
    )
    .title("diffui")
    .subscription(Diffui::subscription)
    .theme(Diffui::theme)
    .run()
}

#[derive(Debug, Clone, Parser)]
#[command(version, about = "Native GUI diff viewer for jj and git")]
struct Cli {
    #[arg(default_value = ".")]
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct Diffui {
    repository: Option<Repository>,
    status: LoadStatus,
    document: DiffDocument,
    commits: Vec<CommitSummary>,
    selected_revision: RevisionSelection,
    expanded_revision: RevisionSelection,
    pending_revision: Option<RevisionSelection>,
    selected_theme: ThemePreference,
    system_theme: theme::Mode,
    selected_file: usize,
}

#[derive(Debug, Clone)]
struct Repository {
    root: PathBuf,
    vcs: Vcs,
    scope: PathBuf,
}

#[derive(Debug, Clone, Copy)]
enum Vcs {
    Jj,
    Git,
}

#[derive(Debug, Clone)]
enum LoadStatus {
    Loading,
    Loaded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RevisionSelection {
    WorkingCopy,
    Commit(String),
}

impl RevisionSelection {
    fn view_key(&self) -> String {
        match self {
            Self::WorkingCopy => "working-copy".to_owned(),
            Self::Commit(change_id) => format!("commit:{change_id}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct DiffDocument {
    files: Vec<DiffFile>,
    total_additions: usize,
    total_deletions: usize,
}

impl DiffDocument {
    fn has_changes(&self) -> bool {
        self.total_additions > 0
            || self.total_deletions > 0
            || self.files.iter().any(|file| !file.hunks.is_empty())
    }
}

#[derive(Debug, Clone)]
struct DiffFile {
    path: String,
    old_path: Option<String>,
    status: DiffFileStatus,
    hunks: Vec<DiffHunkView>,
    additions: usize,
    deletions: usize,
}

#[derive(Debug, Clone)]
struct CommitSummary {
    change_id: String,
    commit_id: String,
    revision_id: String,
    shortest_change_id_len: Option<usize>,
    description: String,
    author: String,
    has_description: bool,
    is_empty: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffFileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone)]
enum Message {
    BackendLoaded(RevisionSelection, Result<BackendOutput, String>),
    SelectFile(usize),
    SelectRevision(RevisionSelection),
    SelectTheme(ThemePreference),
    SystemThemeChanged(theme::Mode),
    SelectNextFile,
    SelectPreviousFile,
}

#[derive(Debug, Clone)]
struct BackendOutput {
    document: DiffDocument,
    commits: Vec<CommitSummary>,
}

#[derive(Debug, Clone)]
struct PendingHunk {
    header: String,
    rows: Vec<DiffLine>,
    next_old_line: usize,
    next_new_line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemePreference {
    System,
    Dark,
    Light,
    HighContrast,
}

impl ThemePreference {
    const ALL: [Self; 4] = [Self::System, Self::Dark, Self::Light, Self::HighContrast];

    fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::HighContrast => "Contrast",
        }
    }

    fn active(self, system_theme: theme::Mode) -> ResolvedTheme {
        match self {
            Self::System => match system_theme {
                theme::Mode::Light => ResolvedTheme::Light,
                theme::Mode::Dark | theme::Mode::None => ResolvedTheme::Dark,
            },
            Self::Dark => ResolvedTheme::Dark,
            Self::Light => ResolvedTheme::Light,
            Self::HighContrast => ResolvedTheme::HighContrast,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedTheme {
    Dark,
    Light,
    HighContrast,
}

impl ResolvedTheme {
    fn spec(self) -> ThemeSpec {
        match self {
            Self::Dark => ThemeSpec {
                background: Color::from_rgb(0.035, 0.040, 0.052),
                panel_background: Color::from_rgb(0.058, 0.066, 0.084),
                panel_background_elevated: Color::from_rgb(0.083, 0.094, 0.120),
                selected_file: Color::from_rgb(0.105, 0.150, 0.190),
                text: Color::from_rgb(0.925, 0.940, 0.960),
                muted_text: Color::from_rgb(0.665, 0.710, 0.760),
                subtle_text: Color::from_rgb(0.500, 0.545, 0.600),
                accent: Color::from_rgb(0.160, 0.640, 0.780),
                added_line: Color::from_rgba(0.065, 0.500, 0.260, 0.18),
                removed_line: Color::from_rgba(0.690, 0.145, 0.180, 0.19),
                added_text: Color::from_rgb(0.450, 0.890, 0.590),
                removed_text: Color::from_rgb(0.980, 0.470, 0.500),
                modified_token: Color::from_rgb(0.920, 0.690, 0.265),
                file_header: Color::from_rgb(0.070, 0.080, 0.102),
                hunk_header: Color::from_rgb(0.105, 0.132, 0.155),
                conflict_marker: Color::from_rgb(1.000, 0.310, 0.350),
                border: Color::from_rgb(0.180, 0.205, 0.245),
                note_background: Color::from_rgba(0.720, 0.490, 0.150, 0.18),
                note_text: Color::from_rgb(0.940, 0.760, 0.390),
            },
            Self::Light => ThemeSpec {
                background: Color::from_rgb(0.945, 0.946, 0.940),
                panel_background: Color::from_rgb(0.988, 0.988, 0.982),
                panel_background_elevated: Color::from_rgb(0.965, 0.966, 0.958),
                selected_file: Color::from_rgb(0.860, 0.910, 0.925),
                text: Color::from_rgb(0.120, 0.130, 0.145),
                muted_text: Color::from_rgb(0.390, 0.430, 0.470),
                subtle_text: Color::from_rgb(0.585, 0.610, 0.635),
                accent: Color::from_rgb(0.045, 0.430, 0.545),
                added_line: Color::from_rgba(0.120, 0.610, 0.330, 0.14),
                removed_line: Color::from_rgba(0.760, 0.120, 0.145, 0.14),
                added_text: Color::from_rgb(0.080, 0.430, 0.225),
                removed_text: Color::from_rgb(0.660, 0.105, 0.125),
                modified_token: Color::from_rgb(0.625, 0.410, 0.080),
                file_header: Color::from_rgb(0.930, 0.932, 0.922),
                hunk_header: Color::from_rgb(0.875, 0.905, 0.910),
                conflict_marker: Color::from_rgb(0.760, 0.080, 0.100),
                border: Color::from_rgb(0.760, 0.770, 0.780),
                note_background: Color::from_rgba(0.820, 0.560, 0.110, 0.18),
                note_text: Color::from_rgb(0.500, 0.320, 0.045),
            },
            Self::HighContrast => ThemeSpec {
                background: Color::BLACK,
                panel_background: Color::from_rgb(0.030, 0.030, 0.030),
                panel_background_elevated: Color::from_rgb(0.070, 0.070, 0.070),
                selected_file: Color::from_rgb(0.000, 0.180, 0.240),
                text: Color::WHITE,
                muted_text: Color::from_rgb(0.780, 0.820, 0.840),
                subtle_text: Color::from_rgb(0.620, 0.660, 0.690),
                accent: Color::from_rgb(0.000, 0.900, 1.000),
                added_line: Color::from_rgb(0.000, 0.235, 0.080),
                removed_line: Color::from_rgb(0.300, 0.000, 0.045),
                added_text: Color::from_rgb(0.500, 1.000, 0.600),
                removed_text: Color::from_rgb(1.000, 0.520, 0.560),
                modified_token: Color::from_rgb(1.000, 0.920, 0.000),
                file_header: Color::from_rgb(0.120, 0.120, 0.120),
                hunk_header: Color::from_rgb(0.000, 0.220, 0.310),
                conflict_marker: Color::from_rgb(1.000, 0.140, 0.140),
                border: Color::from_rgb(0.570, 0.620, 0.660),
                note_background: Color::from_rgb(0.260, 0.210, 0.000),
                note_text: Color::from_rgb(1.000, 0.940, 0.500),
            },
        }
    }

    fn iced_theme(self) -> Theme {
        match self {
            Self::Dark => Theme::Dark,
            Self::Light => Theme::Light,
            Self::HighContrast => Theme::Dark,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ThemeSpec {
    background: Color,
    panel_background: Color,
    panel_background_elevated: Color,
    selected_file: Color,
    text: Color,
    muted_text: Color,
    subtle_text: Color,
    accent: Color,
    added_line: Color,
    removed_line: Color,
    added_text: Color,
    removed_text: Color,
    modified_token: Color,
    file_header: Color,
    hunk_header: Color,
    conflict_marker: Color,
    border: Color,
    note_background: Color,
    note_text: Color,
}

impl Diffui {
    fn new(cli: Cli) -> (Self, Task<Message>) {
        match prepare_repository(&cli.path) {
            Ok(repository) => {
                let revision = RevisionSelection::WorkingCopy;
                let backend_task = Task::perform(
                    load_backend(repository.clone(), revision.clone()),
                    move |result| Message::BackendLoaded(revision, result),
                );
                let theme_task = system::theme().map(Message::SystemThemeChanged);

                (
                    Self {
                        repository: Some(repository),
                        status: LoadStatus::Loading,
                        document: DiffDocument::default(),
                        commits: Vec::new(),
                        selected_revision: RevisionSelection::WorkingCopy,
                        expanded_revision: RevisionSelection::WorkingCopy,
                        pending_revision: Some(RevisionSelection::WorkingCopy),
                        selected_theme: ThemePreference::System,
                        system_theme: theme::Mode::None,
                        selected_file: 0,
                    },
                    Task::batch([backend_task, theme_task]),
                )
            }
            Err(error) => (
                Self {
                    repository: None,
                    status: LoadStatus::Failed(format!("{error:#}")),
                    document: DiffDocument::default(),
                    commits: Vec::new(),
                    selected_revision: RevisionSelection::WorkingCopy,
                    expanded_revision: RevisionSelection::WorkingCopy,
                    pending_revision: None,
                    selected_theme: ThemePreference::System,
                    system_theme: theme::Mode::None,
                    selected_file: 0,
                },
                system::theme().map(Message::SystemThemeChanged),
            ),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::BackendLoaded(revision, Ok(output)) => {
                if self.pending_revision.as_ref() != Some(&revision) {
                    return Task::none();
                }

                let revision_changed = self.expanded_revision != revision;
                self.selected_revision = revision;
                self.expanded_revision = self.selected_revision.clone();
                self.pending_revision = None;
                self.status = LoadStatus::Loaded;
                self.document = output.document;
                self.commits = output.commits;
                self.selected_file = if revision_changed {
                    0
                } else {
                    self.selected_file
                        .min(self.document.files.len().saturating_sub(1))
                };
            }
            Message::BackendLoaded(revision, Err(error)) => {
                if self.pending_revision.as_ref() != Some(&revision) {
                    return Task::none();
                }

                self.pending_revision = None;
                self.status = LoadStatus::Failed(error);
            }
            Message::SelectFile(index) => {
                if index < self.document.files.len() {
                    self.selected_file = index;
                }
            }
            Message::SelectRevision(selection) => {
                if self.selected_revision != selection
                    && self.pending_revision.as_ref() != Some(&selection)
                    && let Some(repository) = self.repository.clone()
                {
                    self.pending_revision = Some(selection.clone());
                    let revision = selection.clone();
                    return Task::perform(load_backend(repository, selection), move |result| {
                        Message::BackendLoaded(revision, result)
                    });
                }
            }
            Message::SelectTheme(theme) => {
                self.selected_theme = theme;
            }
            Message::SystemThemeChanged(theme) => {
                self.system_theme = theme;
            }
            Message::SelectNextFile => {
                if !self.document.files.is_empty() {
                    self.selected_file =
                        (self.selected_file + 1).min(self.document.files.len().saturating_sub(1));
                }
            }
            Message::SelectPreviousFile => {
                let previous = self.selected_file.saturating_sub(1);
                if previous != self.selected_file {
                    self.selected_file = previous;
                }
            }
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let theme = self.resolved_theme().spec();
        let content = row![build_sidebar(self, theme), build_diff_panel(self, theme)]
            .spacing(0)
            .height(Length::Fill);

        container(content)
            .padding(0)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(move |_| app_shell_style(theme))
            .into()
    }

    fn theme(&self) -> Theme {
        self.resolved_theme().iced_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        let keyboard = keyboard::listen().filter_map(|event| match event {
            keyboard::Event::KeyPressed { key, .. } => match key.as_ref() {
                keyboard::Key::Named(keyboard::key::Named::ArrowDown)
                | keyboard::Key::Character("j") => Some(Message::SelectNextFile),
                keyboard::Key::Named(keyboard::key::Named::ArrowUp)
                | keyboard::Key::Character("k") => Some(Message::SelectPreviousFile),
                _ => None,
            },
            _ => None,
        });

        Subscription::batch([
            keyboard,
            system::theme_changes().map(Message::SystemThemeChanged),
        ])
    }

    fn resolved_theme(&self) -> ResolvedTheme {
        self.selected_theme.active(self.system_theme)
    }
}

impl Vcs {
    fn label(self) -> &'static str {
        match self {
            Self::Jj => "Jujutsu",
            Self::Git => "Git",
        }
    }
}

impl DiffFileStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Deleted => "Deleted",
            Self::Modified => "Modified",
            Self::Renamed => "Renamed",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Modified => "M",
            Self::Renamed => "R",
        }
    }

    fn short_badge_color(self, theme: ThemeSpec) -> Color {
        match self {
            Self::Added => theme.added_text,
            Self::Deleted => theme.removed_text,
            Self::Modified => theme.modified_token,
            Self::Renamed => theme.accent,
        }
    }
}

fn prepare_repository(input: &Path) -> Result<Repository> {
    let target = normalize_input_path(input)?;
    let search_start = if target.is_file() {
        target
            .parent()
            .context("target file has no parent directory")?
            .to_path_buf()
    } else {
        target.clone()
    };

    let (root, vcs) = discover_repository(&search_start).with_context(|| {
        format!(
            "could not find a jj or git repository above {}",
            search_start.display()
        )
    })?;

    let scope = target
        .strip_prefix(&root)
        .unwrap_or(target.as_path())
        .to_path_buf();

    Ok(Repository { root, vcs, scope })
}

fn normalize_input_path(input: &Path) -> Result<PathBuf> {
    let input = if input.is_absolute() {
        input.to_path_buf()
    } else {
        env::current_dir()
            .context("failed to read current directory")?
            .join(input)
    };

    input
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", input.display()))
}

fn discover_repository(start: &Path) -> Result<(PathBuf, Vcs)> {
    for directory in start.ancestors() {
        if directory.join(".jj").is_dir() {
            return Ok((directory.to_path_buf(), Vcs::Jj));
        }

        if directory.join(".git").exists() {
            return Ok((directory.to_path_buf(), Vcs::Git));
        }
    }

    bail!("not inside a repository")
}

async fn load_backend(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<BackendOutput, String> {
    run_backend(repository, revision)
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn run_backend(repository: Repository, revision: RevisionSelection) -> Result<BackendOutput> {
    let commits = load_commits(&repository).await?;
    let document = match repository.vcs {
        Vcs::Jj => {
            let repository = repository.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || handle.block_on(load_jj_diff(repository, revision)))
                .await
                .context("jj diff loader task failed")??
        }
        Vcs::Git => {
            let args = git_backend_command(&repository, &revision);
            let output = run_command(&repository.root, "git", args).await?;
            parse_backend_output(&repository, &output)
        }
    };

    Ok(BackendOutput { document, commits })
}

fn git_backend_command(repository: &Repository, revision: &RevisionSelection) -> Vec<OsString> {
    let mut args: Vec<OsString> = match revision {
        RevisionSelection::WorkingCopy => ["diff", "--"].into_iter().map(OsString::from).collect(),
        RevisionSelection::Commit(revision) => {
            vec![
                OsString::from("show"),
                OsString::from("--format="),
                OsString::from("--no-ext-diff"),
                OsString::from("--no-color"),
                OsString::from(revision),
                OsString::from("--"),
            ]
        }
    };

    if !repository.scope.as_os_str().is_empty() {
        args.push(repository.scope.as_os_str().to_owned());
    }

    args
}

async fn run_command(current_dir: &Path, program: &str, args: Vec<OsString>) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to execute {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} exited with {}: {}", output.status, stderr.trim());
    }

    String::from_utf8(output.stdout).with_context(|| format!("{program} emitted non-utf8 output"))
}

async fn load_commits(repository: &Repository) -> Result<Vec<CommitSummary>> {
    match repository.vcs {
        Vcs::Jj => {
            let root = repository.root.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || handle.block_on(load_jj_commits(root)))
                .await
                .context("jj commit loader task failed")?
        }
        Vcs::Git => {
            let output = run_command(
                &repository.root,
                "git",
                vec![
                    OsString::from("log"),
                    OsString::from("--max-count=24"),
                    OsString::from("--pretty=format:%h%x09%H%x09%ae%x09%x09%s"),
                ],
            )
            .await?;

            Ok(parse_commit_log(&output))
        }
    }
}

async fn load_jj_commits(repository_root: PathBuf) -> Result<Vec<CommitSummary>> {
    let settings = UserSettings::from_config(StackedConfig::with_defaults())
        .context("failed to load jj settings")?;
    let workspace = Workspace::load(
        &settings,
        &repository_root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let workspace_name = workspace.workspace_name();
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(workspace_name)
        .context("jj workspace has no working-copy commit")?;
    let wc_commit = repo
        .store()
        .get_commit_async(wc_commit_id)
        .await
        .context("failed to load jj working-copy commit")?;

    let mut commits = Vec::new();
    let mut stack = wc_commit.parent_ids().to_vec();

    while let Some(commit_id) = stack.pop() {
        if commits.len() >= 24 {
            break;
        }

        let commit = repo
            .store()
            .get_commit_async(&commit_id)
            .await
            .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;

        let description = commit.description().lines().next().unwrap_or("").trim();
        let is_empty = commit
            .is_empty(repo.as_ref())
            .await
            .with_context(|| format!("failed to inspect jj commit {}", commit.id().hex()))?;
        let shortest_change_id_len = repo
            .shortest_unique_change_id_prefix_len(commit.change_id())
            .with_context(|| {
                format!(
                    "failed to resolve shortest unique jj change id for {}",
                    commit.change_id().hex()
                )
            })?;

        commits.push(CommitSummary {
            change_id: commit.change_id().to_string(),
            commit_id: commit.id().hex(),
            revision_id: commit.id().hex(),
            shortest_change_id_len: Some(shortest_change_id_len),
            description: if description.is_empty() {
                "(no description set)".to_owned()
            } else {
                description.to_owned()
            },
            author: commit.author().email.clone(),
            has_description: !description.is_empty(),
            is_empty: Some(is_empty),
        });

        stack.extend(commit.parent_ids().iter().cloned());
    }

    Ok(commits)
}

async fn load_jj_diff(repository: Repository, revision: RevisionSelection) -> Result<DiffDocument> {
    let settings = UserSettings::from_config(StackedConfig::with_defaults())
        .context("failed to load jj settings")?;
    let workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let workspace_name = workspace.workspace_name();
    let repo = workspace
        .repo_loader()
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let commit_id = match revision {
        RevisionSelection::WorkingCopy => repo
            .view()
            .get_wc_commit_id(workspace_name)
            .context("jj workspace has no working-copy commit")?
            .clone(),
        RevisionSelection::Commit(revision) => CommitId::try_from_hex(&revision)
            .with_context(|| format!("invalid jj commit id {revision}"))?,
    };
    let commit = repo
        .store()
        .get_commit_async(&commit_id)
        .await
        .with_context(|| format!("failed to load jj commit {}", commit_id.hex()))?;
    let old_tree = commit
        .parent_tree(repo.as_ref())
        .await
        .with_context(|| format!("failed to load jj parent tree for {}", commit_id.hex()))?;
    let new_tree = commit.tree();
    let matcher = repo_scope_matcher(&repository)?;
    let copy_records = CopyRecords::default();
    let tree_diff = old_tree.diff_stream_with_copies(&new_tree, matcher.as_ref(), &copy_records);
    let labels = Diff::new(old_tree.labels(), new_tree.labels());
    let mut stream = materialized_diff_stream(repo.store(), tree_diff, labels);
    let materialize_options = ConflictMaterializeOptions {
        marker_style: ConflictMarkerStyle::Diff,
        marker_len: None,
        merge: MergeOptions {
            hunk_level: FileMergeHunkLevel::Line,
            same_change: SameChange::Accept,
        },
    };
    let mut files = Vec::new();

    while let Some(entry) = stream.next().await {
        let values = entry.values.with_context(|| {
            format!(
                "failed to read jj diff for {}",
                repo_path_label(entry.path.target())
            )
        })?;
        let old_path = entry
            .path
            .to_diff()
            .map(|paths| repo_path_label(paths.before));
        let path = repo_path_label(entry.path.target());
        let before_absent = values.before.is_absent();
        let after_absent = values.after.is_absent();
        let status = if before_absent {
            DiffFileStatus::Added
        } else if after_absent {
            DiffFileStatus::Deleted
        } else if old_path.is_some() {
            DiffFileStatus::Renamed
        } else {
            DiffFileStatus::Modified
        };
        let before = git_diff_part(entry.path.source(), values.before, &materialize_options)
            .await
            .with_context(|| {
                format!(
                    "failed to read previous content for {}",
                    repo_path_label(entry.path.source())
                )
            })?;
        let after = git_diff_part(entry.path.target(), values.after, &materialize_options)
            .await
            .with_context(|| {
                format!(
                    "failed to read current content for {}",
                    repo_path_label(entry.path.target())
                )
            })?;

        let mut file = DiffFile {
            path,
            old_path,
            status,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        };

        if before.content.is_binary || after.content.is_binary {
            file.hunks.push(DiffHunkView {
                header: "binary files differ".to_owned(),
                lines: Vec::new(),
            });
        } else {
            let hunks = unified_diff_hunks(
                Diff::new(
                    BStr::new(before.content.contents.as_slice()),
                    BStr::new(after.content.contents.as_slice()),
                ),
                3,
                LineCompareMode::Exact,
            );
            for hunk in hunks {
                let mut rows = Vec::new();
                let mut old_line = hunk.left_line_range.start + 1;
                let mut new_line = hunk.right_line_range.start + 1;
                for (line_type, tokens) in hunk.lines {
                    let content = diff_tokens_to_string(tokens);
                    match line_type {
                        DiffLineType::Context => {
                            rows.push(DiffLine {
                                kind: DiffLineKind::Context,
                                old_line: Some(old_line),
                                new_line: Some(new_line),
                                content,
                                syntax: Vec::new(),
                            });
                            old_line += 1;
                            new_line += 1;
                        }
                        DiffLineType::Removed => {
                            file.deletions += 1;
                            rows.push(DiffLine {
                                kind: DiffLineKind::Deletion,
                                old_line: Some(old_line),
                                new_line: None,
                                content,
                                syntax: Vec::new(),
                            });
                            old_line += 1;
                        }
                        DiffLineType::Added => {
                            file.additions += 1;
                            rows.push(DiffLine {
                                kind: DiffLineKind::Addition,
                                old_line: None,
                                new_line: Some(new_line),
                                content,
                                syntax: Vec::new(),
                            });
                            new_line += 1;
                        }
                    }
                }
                file.hunks.push(DiffHunkView {
                    header: format_hunk_header(&hunk.left_line_range, &hunk.right_line_range),
                    lines: rows,
                });
            }
        }

        apply_syntax_highlighting(&mut file);
        files.push(file);
    }

    if files.is_empty() {
        files.push(DiffFile {
            path: display_scope(&repository),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        });
    }

    let total_additions = files.iter().map(|file| file.additions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();

    Ok(DiffDocument {
        files,
        total_additions,
        total_deletions,
    })
}

fn repo_scope_matcher(repository: &Repository) -> Result<Box<dyn Matcher>> {
    if repository.scope.as_os_str().is_empty() {
        return Ok(Box::new(EverythingMatcher));
    }

    let repo_path =
        RepoPathBuf::parse_fs_path(&repository.root, &repository.root, &repository.scope)
            .with_context(|| format!("failed to parse jj path {}", repository.scope.display()))?;
    Ok(Box::new(PrefixMatcher::new([repo_path])))
}

fn repo_path_label(path: &RepoPath) -> String {
    path.as_internal_file_string().to_owned()
}

fn diff_tokens_to_string(tokens: Vec<(jj_lib::diff_presentation::DiffTokenType, &[u8])>) -> String {
    let mut bytes = Vec::new();
    for (_, token) in tokens {
        bytes.extend_from_slice(token);
    }

    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }

    String::from_utf8_lossy(&bytes).into_owned()
}

fn format_hunk_header(
    old_range: &std::ops::Range<usize>,
    new_range: &std::ops::Range<usize>,
) -> String {
    format!(
        "@@ -{} +{} @@",
        format_hunk_range(old_range),
        format_hunk_range(new_range)
    )
}

fn format_hunk_range(range: &std::ops::Range<usize>) -> String {
    let len = range.end.saturating_sub(range.start);
    let start = if len == 0 {
        range.start
    } else {
        range.start + 1
    };

    if len == 1 {
        start.to_string()
    } else {
        format!("{start},{len}")
    }
}

fn parse_commit_log(output: &str) -> Vec<CommitSummary> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let change_id = parts.next()?.trim();
            let commit_id = parts.next()?.trim();
            let author = parts.next()?.trim();
            let remainder = parts.next().unwrap_or("");
            let (empty, description) =
                if let Some((empty, description)) = remainder.split_once('\t') {
                    (parse_optional_bool(empty.trim()), description.trim())
                } else {
                    (None, remainder.trim())
                };

            if change_id.is_empty() || commit_id.is_empty() {
                return None;
            }

            let has_description = !description.is_empty();
            Some(CommitSummary {
                change_id: change_id.to_owned(),
                commit_id: commit_id.to_owned(),
                revision_id: commit_id.to_owned(),
                shortest_change_id_len: None,
                description: if description.is_empty() {
                    "(no description set)".to_owned()
                } else {
                    description.to_owned()
                },
                author: author.to_owned(),
                has_description,
                is_empty: empty,
            })
        })
        .collect()
}

fn parse_optional_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_backend_output(repository: &Repository, output: &str) -> DiffDocument {
    let mut files = Vec::new();
    let mut current_file: Option<DiffFile> = None;
    let mut current_hunk: Option<PendingHunk> = None;

    for line in output.lines() {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            flush_current_file(&mut files, &mut current_file, &mut current_hunk);

            let (old_path, path) = parse_diff_git_paths(paths);
            current_file = Some(DiffFile {
                path,
                old_path,
                status: DiffFileStatus::Modified,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            });
            continue;
        }

        let Some(file) = current_file.as_mut() else {
            continue;
        };

        if line.starts_with("@@") {
            flush_current_hunk(file, &mut current_hunk);
            let (next_old_line, next_new_line) = parse_hunk_header(line);
            current_hunk = Some(PendingHunk {
                header: line.to_owned(),
                rows: Vec::new(),
                next_old_line,
                next_new_line,
            });
            continue;
        }

        if let Some(hunk) = current_hunk.as_mut() {
            push_hunk_row(file, hunk, line);
        } else {
            update_file_metadata(file, line);
        }
    }

    flush_current_file(&mut files, &mut current_file, &mut current_hunk);

    if files.is_empty() {
        files.push(DiffFile {
            path: display_scope(repository),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        });
    }

    let total_additions = files.iter().map(|file| file.additions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();

    DiffDocument {
        files,
        total_additions,
        total_deletions,
    }
}

fn flush_current_file(
    files: &mut Vec<DiffFile>,
    current_file: &mut Option<DiffFile>,
    current_hunk: &mut Option<PendingHunk>,
) {
    if let Some(file) = current_file.as_mut() {
        flush_current_hunk(file, current_hunk);
    }

    if let Some(mut file) = current_file.take() {
        apply_syntax_highlighting(&mut file);
        files.push(file);
    }
}

fn flush_current_hunk(file: &mut DiffFile, current_hunk: &mut Option<PendingHunk>) {
    if let Some(hunk) = current_hunk.take() {
        file.hunks.push(DiffHunkView {
            header: hunk.header,
            lines: hunk.rows,
        });
    }
}

fn update_file_metadata(file: &mut DiffFile, line: &str) {
    if let Some(path) = line.strip_prefix("rename from ") {
        file.old_path = Some(path.to_owned());
        file.status = DiffFileStatus::Renamed;
    } else if let Some(path) = line.strip_prefix("rename to ") {
        file.path = path.to_owned();
        file.status = DiffFileStatus::Renamed;
    } else if line.starts_with("new file mode ") || line == "--- /dev/null" {
        file.status = DiffFileStatus::Added;
    } else if line.starts_with("deleted file mode ") || line == "+++ /dev/null" {
        file.status = DiffFileStatus::Deleted;
    } else if let Some(path) = line.strip_prefix("--- a/") {
        file.old_path = Some(path.to_owned());
    } else if let Some(path) = line.strip_prefix("+++ b/") {
        file.path = path.to_owned();
    }
}

fn push_hunk_row(file: &mut DiffFile, hunk: &mut PendingHunk, line: &str) {
    match line.chars().next() {
        Some('+') => {
            file.additions += 1;
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Addition,
                old_line: None,
                new_line: Some(hunk.next_new_line),
                content: line[1..].to_owned(),
                syntax: Vec::new(),
            });
            hunk.next_new_line += 1;
        }
        Some('-') => {
            file.deletions += 1;
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Deletion,
                old_line: Some(hunk.next_old_line),
                new_line: None,
                content: line[1..].to_owned(),
                syntax: Vec::new(),
            });
            hunk.next_old_line += 1;
        }
        Some(' ') => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(hunk.next_old_line),
                new_line: Some(hunk.next_new_line),
                content: line[1..].to_owned(),
                syntax: Vec::new(),
            });
            hunk.next_old_line += 1;
            hunk.next_new_line += 1;
        }
        Some('\\') => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Note,
                old_line: None,
                new_line: None,
                content: line.to_owned(),
                syntax: Vec::new(),
            });
        }
        _ => {
            let kind = if is_conflict_marker(line) {
                DiffLineKind::Conflict
            } else {
                DiffLineKind::Note
            };

            hunk.rows.push(DiffLine {
                kind,
                old_line: None,
                new_line: None,
                content: line.to_owned(),
                syntax: Vec::new(),
            });
        }
    }
}

fn is_conflict_marker(line: &str) -> bool {
    line.starts_with("<<<<<<<")
        || line.starts_with("|||||||")
        || line.starts_with("=======")
        || line.starts_with(">>>>>>>")
}

fn apply_syntax_highlighting(file: &mut DiffFile) {
    static GRAMMAR_STORE: OnceLock<GrammarStore> = OnceLock::new();

    let Some(language) = arborium::detect_language(&file.path) else {
        return;
    };
    let store = GRAMMAR_STORE.get_or_init(GrammarStore::new);
    let Some(grammar) = store.get(language) else {
        return;
    };
    let mut contexts = HashMap::new();

    for hunk in &mut file.hunks {
        for line in &mut hunk.lines {
            if matches!(line.kind, DiffLineKind::Note | DiffLineKind::Conflict) {
                continue;
            }

            line.syntax = highlight_line(language, grammar.clone(), &mut contexts, &line.content);
        }
    }
}

fn highlight_line(
    language: &str,
    grammar: Arc<CompiledGrammar>,
    contexts: &mut HashMap<String, ParseContext>,
    content: &str,
) -> Vec<SyntaxSpan> {
    if content.trim().is_empty() {
        return Vec::new();
    }

    let context = match contexts.entry(language.to_owned()) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            let Ok(context) = ParseContext::for_grammar(&grammar) else {
                return Vec::new();
            };
            entry.insert(context)
        }
    };
    let result = grammar.parse(context, content);
    let mut spans = Vec::new();

    for span in result.spans {
        let Some(kind) = syntax_kind_for_capture(&span.capture) else {
            continue;
        };

        let (Ok(start), Ok(end)) = (usize::try_from(span.start), usize::try_from(span.end)) else {
            continue;
        };
        if start < end && content.is_char_boundary(start) && content.is_char_boundary(end) {
            spans.push(SyntaxSpan { start, end, kind });
        }
    }

    normalize_syntax_spans(spans)
}

fn syntax_kind_for_capture(capture: &str) -> Option<SyntaxKind> {
    if capture.starts_with("comment") {
        Some(SyntaxKind::Comment)
    } else if capture.starts_with("string") || capture == "character" {
        Some(SyntaxKind::String)
    } else if capture.starts_with("number")
        || capture.starts_with("constant")
        || capture == "boolean"
    {
        Some(SyntaxKind::Number)
    } else if capture.starts_with("keyword")
        || capture == "operator"
        || capture == "include"
        || capture == "storageclass"
    {
        Some(SyntaxKind::Keyword)
    } else if capture.starts_with("function") || capture == "constructor" || capture == "method" {
        Some(SyntaxKind::Function)
    } else if capture.starts_with("type") || capture == "variable.builtin" {
        Some(SyntaxKind::Type)
    } else if capture.starts_with("property")
        || capture == "variable.parameter"
        || capture == "field"
        || capture == "attribute"
        || capture == "tag"
    {
        Some(SyntaxKind::Property)
    } else if capture.starts_with("punctuation") {
        Some(SyntaxKind::Punctuation)
    } else {
        None
    }
}

fn normalize_syntax_spans(mut spans: Vec<SyntaxSpan>) -> Vec<SyntaxSpan> {
    spans.sort_by_key(|span| (span.start, span.end));

    let mut normalized: Vec<SyntaxSpan> = Vec::with_capacity(spans.len());
    for mut span in spans {
        if let Some(previous) = normalized.last()
            && span.start < previous.end
        {
            span.start = previous.end;
        }

        if span.start < span.end {
            normalized.push(span);
        }
    }

    normalized
}

fn parse_diff_git_paths(paths: &str) -> (Option<String>, String) {
    let mut parts = paths.split_whitespace();
    let old_path = parts.next().map(clean_git_diff_path);
    let path = parts
        .next()
        .map(clean_git_diff_path)
        .or_else(|| old_path.clone())
        .unwrap_or_else(|| "<unknown>".to_owned());

    (old_path, path)
}

fn clean_git_diff_path(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_owned()
}

fn parse_hunk_header(header: &str) -> (usize, usize) {
    let mut parts = header.split_whitespace();
    let _marker = parts.next();
    let old = parts.next().unwrap_or_default();
    let new = parts.next().unwrap_or_default();

    (
        parse_hunk_range(old, '-').unwrap_or(0),
        parse_hunk_range(new, '+').unwrap_or(0),
    )
}

fn parse_hunk_range(part: &str, prefix: char) -> Option<usize> {
    part.strip_prefix(prefix)
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.parse::<usize>().ok())
}

fn build_sidebar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let repo_label = ui
        .repository
        .as_ref()
        .map(|repository| format!("{} / {}", repository.vcs.label(), display_scope(repository)))
        .unwrap_or_else(|| "Outside Repository".to_owned());
    let metrics = row![
        text(format_count(ui.document.files.len(), "File", "Files"))
            .size(CAPTION_TEXT_SIZE)
            .color(theme.accent),
        text(format!("+{}", ui.document.total_additions))
            .size(CAPTION_TEXT_SIZE)
            .color(theme.added_text),
        text(format!("-{}", ui.document.total_deletions))
            .size(CAPTION_TEXT_SIZE)
            .color(theme.removed_text),
    ]
    .spacing(10)
    .align_y(alignment::Vertical::Center);

    let mut header_content = column![
        row![
            text("Changes")
                .size(TITLE_TEXT_SIZE)
                .color(theme.text)
                .width(Length::Fill),
            build_theme_switcher(ui.selected_theme, theme),
        ]
        .spacing(10),
        metrics,
        text(repo_label)
            .size(CAPTION_TEXT_SIZE)
            .color(theme.subtle_text),
    ]
    .spacing(7);

    if let LoadStatus::Failed(error) = &ui.status {
        header_content = header_content.push(
            text(format!("Failed: {error}"))
                .size(CAPTION_TEXT_SIZE)
                .color(theme.removed_text),
        );
    }

    let sidebar_header = container(header_content)
        .padding([12, 12])
        .style(move |_| sidebar_header_style(theme));

    let mut items = column![sidebar_header, build_working_copy_button(ui, theme),].spacing(0);

    for commit in &ui.commits {
        items = items.push(build_commit_button(commit, &ui.commits, ui, theme));
    }

    container(
        scrollable(items.spacing(0))
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::new()
                    .width(SIDEBAR_SCROLLBAR_WIDTH)
                    .scroller_width(SIDEBAR_SCROLLBAR_SCROLLER_WIDTH)
                    .spacing(SIDEBAR_SCROLLBAR_SPACING),
            ))
            .style(move |iced_theme, status| diff_scrollable_style(iced_theme, status, theme))
            .height(Length::Fill),
    )
    .width(Length::Fixed(SIDEBAR_WIDTH))
    .height(Length::Fill)
    .style(move |_| panel_style(theme.panel_background, theme))
    .into()
}

fn build_theme_switcher(
    selected_theme: ThemePreference,
    theme: ThemeSpec,
) -> Element<'static, Message> {
    let mut controls = row![].spacing(3);

    for candidate in ThemePreference::ALL {
        let selected = candidate == selected_theme;
        controls = controls.push(
            button(text(candidate.label()).size(CAPTION_TEXT_SIZE))
                .padding([5, 7])
                .style(move |_, status| sidebar_button_style(status, selected, theme))
                .on_press(Message::SelectTheme(candidate)),
        );
    }

    controls.into()
}

fn build_working_copy_button(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let mut indicators = vec![RevisionIndicator::WorkingCopy];
    let revision = RevisionSelection::WorkingCopy;

    if revision == ui.expanded_revision
        && matches!(ui.status, LoadStatus::Loaded)
        && !ui.document.has_changes()
    {
        indicators.push(RevisionIndicator::Empty);
    }

    let title = row![
        text("Working Copy")
            .size(SMALL_TEXT_SIZE)
            .color(theme.text)
            .wrapping(Wrapping::Glyph),
        build_revision_metadata(indicators, &revision, ui, theme),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);

    build_revision_item_with_content(
        title.into(),
        "Uncommitted Changes",
        None,
        revision,
        &ui.selected_revision,
        ui,
        theme,
    )
}

#[derive(Debug, Clone, Copy)]
enum RevisionIndicator {
    WorkingCopy,
    Empty,
    NoDescription,
}

impl RevisionIndicator {
    fn label(self) -> &'static str {
        match self {
            Self::WorkingCopy => "wip",
            Self::Empty => "empty",
            Self::NoDescription => "no desc",
        }
    }

    fn colors(self, theme: ThemeSpec) -> (Color, Color) {
        match self {
            Self::WorkingCopy => (theme.selected_file, theme.accent),
            Self::Empty => (theme.panel_background, theme.subtle_text),
            Self::NoDescription => (theme.note_background, theme.note_text),
        }
    }
}

fn build_commit_button<'a>(
    commit: &'a CommitSummary,
    commits: &'a [CommitSummary],
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let unique_len = shortest_unique_prefix_len(&commit.change_id, commits);
    let label_len = revision_id_display_len(unique_len, &commit.change_id);
    let id_prefix = commit
        .change_id
        .chars()
        .take(unique_len)
        .collect::<String>();
    let id_suffix = commit
        .change_id
        .chars()
        .skip(unique_len)
        .take(label_len.saturating_sub(unique_len))
        .collect::<String>();
    let revision = RevisionSelection::Commit(commit.revision_id.clone());
    let commit_id = truncate_end(&commit.commit_id, SIDEBAR_COMMIT_ID_CHARS);
    let detail = format!("{commit_id} · {}", commit.author);
    let mut indicators = Vec::new();
    if !commit.has_description {
        indicators.push(RevisionIndicator::NoDescription);
    }
    if let Some(is_empty) = commit.is_empty
        && is_empty
    {
        indicators.push(RevisionIndicator::Empty);
    }

    let title = row![
        row![
            text(id_prefix).size(SMALL_TEXT_SIZE).color(theme.accent),
            text(id_suffix)
                .size(SMALL_TEXT_SIZE)
                .color(theme.subtle_text),
        ]
        .spacing(0),
        build_revision_metadata(indicators, &revision, ui, theme),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);

    build_revision_item_with_content(
        title.into(),
        &commit.description,
        Some(detail),
        revision,
        &ui.selected_revision,
        ui,
        theme,
    )
}

fn revision_id_display_len(unique_len: usize, revision_id: &str) -> usize {
    SIDEBAR_REVISION_ID_CHARS
        .max(unique_len)
        .min(revision_id.chars().count())
}

fn truncate_end(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn build_revision_metadata<'a>(
    indicators: impl IntoIterator<Item = RevisionIndicator>,
    revision: &RevisionSelection,
    ui: &Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let mut row = row![].spacing(4).align_y(alignment::Vertical::Center);

    for indicator in indicators {
        let (background, text_color) = indicator.colors(theme);
        row = row.push(build_revision_chip(
            indicator.label(),
            background,
            text_color,
            theme,
        ));
    }

    if revision == &ui.expanded_revision && matches!(ui.status, LoadStatus::Loaded) {
        row = row.push(build_revision_chip(
            format_count(ui.document.files.len(), "file", "files"),
            theme.panel_background,
            theme.accent,
            theme,
        ));
    }

    container(row)
        .height(Length::Fixed(REVISION_CHIP_HEIGHT))
        .into()
}

fn build_revision_chip<'a>(
    label: impl Into<String>,
    background: Color,
    text_color: Color,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    container(
        text(label.into())
            .size(CAPTION_TEXT_SIZE)
            .color(text_color)
            .wrapping(Wrapping::None),
    )
    .height(Length::Fixed(REVISION_CHIP_HEIGHT))
    .padding([1, 6])
    .style(move |_| indicator_chip_style(background, theme))
    .into()
}

fn build_revision_item_with_content<'a>(
    title: Element<'a, Message>,
    description: impl Into<String>,
    detail: Option<String>,
    revision: RevisionSelection,
    selected_revision: &RevisionSelection,
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let selected = &revision == selected_revision;
    let expanded = revision == ui.expanded_revision;
    let description = description.into();
    let loaded = matches!(ui.status, LoadStatus::Loaded);

    let mut labels = column![
        title,
        text(description)
            .size(SMALL_TEXT_SIZE)
            .color(theme.text)
            .width(Length::Fill)
            .wrapping(Wrapping::Glyph),
    ]
    .spacing(4);

    if let Some(detail) = detail {
        labels = labels.push(
            text(detail)
                .size(CAPTION_TEXT_SIZE)
                .color(theme.subtle_text)
                .width(Length::Fill),
        );
    }

    let revision_button = button(container(
        row![
            build_selection_gutter(selected, theme),
            container(labels,).padding([9, 10]).width(Length::Fill),
        ]
        .spacing(0),
    ))
    .width(Length::Fill)
    .padding(0)
    .style(move |_, status| sidebar_button_style(status, selected, theme))
    .on_press(Message::SelectRevision(revision));

    let mut item = column![revision_button, build_sidebar_divider(theme)].spacing(0);

    if expanded && loaded && !ui.document.files.is_empty() {
        let mut files = column![].spacing(0);
        let stat_width = sidebar_file_stat_widths(&ui.document.files);
        let display_width = sidebar_file_display_width(stat_width);
        let display_models = sidebar_file_display_models(&ui.document.files, display_width);

        for (index, file) in ui.document.files.iter().enumerate() {
            files = files.push(
                column![
                    build_nested_file_button(
                        index,
                        file,
                        display_models[index].clone(),
                        index == ui.selected_file,
                        stat_width,
                        theme
                    ),
                    build_sidebar_divider(theme),
                ]
                .spacing(0),
            );
        }

        item = item.push(files.width(Length::Fill));
    }

    item.into()
}

fn shortest_unique_prefix_len(change_id: &str, commits: &[CommitSummary]) -> usize {
    if let Some(prefix_len) = commits
        .iter()
        .find(|commit| commit.change_id == change_id)
        .and_then(|commit| commit.shortest_change_id_len)
    {
        return prefix_len.min(change_id.chars().count());
    }

    let total_len = change_id.chars().count();

    (1..=total_len)
        .find(|prefix_len| {
            let prefix = change_id.chars().take(*prefix_len).collect::<String>();
            commits
                .iter()
                .filter(|commit| commit.change_id.starts_with(&prefix))
                .count()
                == 1
        })
        .unwrap_or(total_len)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarFileDisplay {
    primary: String,
    secondary: String,
    raw_path: String,
}

fn sidebar_file_display_models(
    files: &[DiffFile],
    available_width: f32,
) -> Vec<SidebarFileDisplay> {
    let mut basename_counts = HashMap::<&str, usize>::new();
    let split_paths = files
        .iter()
        .map(|file| split_display_path(&file.path))
        .inspect(|(_, basename)| {
            *basename_counts.entry(*basename).or_default() += 1;
        })
        .collect::<Vec<_>>();

    files
        .iter()
        .zip(split_paths.iter())
        .map(|(file, (directories, basename))| {
            let (primary, secondary) = if basename_counts.get(basename).copied() == Some(1) {
                (
                    (*basename).to_owned(),
                    secondary_display_path(directories, available_width),
                )
            } else {
                let group = split_paths
                    .iter()
                    .filter(|(_, other_basename)| other_basename == basename)
                    .map(|(other_directories, _)| other_directories.as_slice())
                    .collect::<Vec<_>>();
                let suffix_len = collision_directory_suffix_len(directories, &group);
                let split_at = directories.len().saturating_sub(suffix_len);
                let primary_segments = directories[split_at..]
                    .iter()
                    .copied()
                    .chain(std::iter::once(*basename))
                    .collect::<Vec<_>>();

                (
                    primary_segments.join("/"),
                    secondary_display_path(&common_directory_prefix(&group), available_width),
                )
            };

            SidebarFileDisplay {
                primary: truncate_primary_display(&primary, basename, available_width),
                secondary,
                raw_path: file.path.clone(),
            }
        })
        .collect()
}

fn split_display_path(path: &str) -> (Vec<&str>, &str) {
    let mut segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    match segments.pop() {
        Some(basename) => (segments, basename),
        None => (Vec::new(), path),
    }
}

fn collision_directory_suffix_len(directories: &[&str], group: &[&[&str]]) -> usize {
    let max_depth = group
        .iter()
        .map(|other_directories| other_directories.len())
        .max()
        .unwrap_or(0);

    for depth in 1..=max_depth {
        let mut segments = group.iter().map(|other_directories| {
            other_directories
                .len()
                .checked_sub(depth)
                .and_then(|index| other_directories.get(index).copied())
        });
        let Some(first) = segments.next() else {
            return 0;
        };

        if segments.any(|segment| segment != first) {
            return directories.len().min(depth);
        }
    }

    directories.len()
}

fn common_directory_prefix<'a>(group: &[&[&'a str]]) -> Vec<&'a str> {
    let Some(first) = group.first() else {
        return Vec::new();
    };

    first
        .iter()
        .enumerate()
        .take_while(|(index, segment)| {
            group
                .iter()
                .all(|directories| directories.get(*index) == Some(segment))
        })
        .map(|(_, segment)| *segment)
        .collect()
}

fn secondary_display_path(segments: &[&str], available_width: f32) -> String {
    let path = segments.join("/");
    if path_fits_width(&path, available_width) {
        path
    } else {
        abbreviate_secondary_path(segments)
    }
}

fn path_fits_width(path: &str, available_width: f32) -> bool {
    path.chars().count() <= max_sidebar_file_text_chars(available_width)
}

fn max_sidebar_file_text_chars(available_width: f32) -> usize {
    (available_width / SIDEBAR_FILE_TEXT_CHAR_WIDTH)
        .floor()
        .max(0.0) as usize
}

fn abbreviate_secondary_path(segments: &[&str]) -> String {
    match segments {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [first, rest @ .., last] => {
            let mut abbreviated = Vec::with_capacity(segments.len());
            abbreviated.push((*first).to_owned());
            abbreviated.extend(
                rest.iter()
                    .filter_map(|segment| segment.chars().next())
                    .map(|character| character.to_string()),
            );
            abbreviated.push((*last).to_owned());
            abbreviated.join("/")
        }
    }
}

fn truncate_primary_display(primary: &str, basename: &str, available_width: f32) -> String {
    let max_chars = max_sidebar_file_text_chars(available_width);

    if primary.chars().count() <= max_chars || primary == basename {
        return primary.to_owned();
    }

    let Some(prefix) = primary.strip_suffix(basename) else {
        return primary.to_owned();
    };
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return primary.to_owned();
    }

    let basename_chars = basename.chars().count();
    let separator_chars = 1;
    let ellipsis_chars = 1;
    if max_chars <= basename_chars + separator_chars + ellipsis_chars {
        return basename.to_owned();
    }

    let prefix_budget = max_chars - basename_chars - separator_chars;
    let truncated_prefix = middle_truncate(prefix, prefix_budget);

    format!("{truncated_prefix}/{basename}")
}

fn middle_truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }

    let keep = max_chars - 1;
    let head_chars = keep.div_ceil(2);
    let tail_chars = keep / 2;
    let head = value.chars().take(head_chars).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();

    format!("{head}…{tail}")
}

fn sidebar_file_display_width(stat_width: SidebarFileStatWidth) -> f32 {
    SIDEBAR_WIDTH
        - SIDEBAR_FILE_TEXT_RESERVED_WIDTH
        - stat_width.additions.max(SIDEBAR_FILE_STAT_MIN_WIDTH)
        - stat_width.deletions.max(SIDEBAR_FILE_STAT_MIN_WIDTH)
}

fn build_nested_file_button<'a>(
    index: usize,
    file: &'a DiffFile,
    display: SidebarFileDisplay,
    selected: bool,
    stat_width: SidebarFileStatWidth,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let primary = display.primary;
    let secondary = display.secondary;
    let raw_path = display.raw_path;
    let path_label: Element<'a, Message> = if secondary.is_empty() {
        text(primary)
            .size(CAPTION_TEXT_SIZE)
            .color(theme.text)
            .wrapping(Wrapping::None)
            .ellipsis(Ellipsis::End)
            .width(Length::Fill)
            .into()
    } else {
        column![
            text(primary)
                .size(CAPTION_TEXT_SIZE)
                .color(theme.text)
                .wrapping(Wrapping::None)
                .ellipsis(Ellipsis::End)
                .width(Length::Fill),
            text(secondary)
                .size(CAPTION_TEXT_SIZE - 2.0)
                .color(theme.muted_text)
                .wrapping(Wrapping::None)
                .ellipsis(Ellipsis::End)
                .width(Length::Fill),
        ]
        .spacing(1)
        .width(Length::Fill)
        .into()
    };

    tooltip(
        button(
            container(
                row![
                    container(build_file_status_badge(
                        file.status.short_label(),
                        file.status.short_badge_color(theme),
                        theme,
                    ))
                    .width(Length::Fixed(SIDEBAR_FILE_BADGE_WIDTH)),
                    container(path_label).width(Length::Fill),
                    text(format!("+{}", file.additions))
                        .size(CAPTION_TEXT_SIZE)
                        .color(theme.added_text)
                        .width(Length::Fixed(stat_width.additions)),
                    text(format!("-{}", file.deletions))
                        .size(CAPTION_TEXT_SIZE)
                        .color(theme.removed_text)
                        .width(Length::Fixed(stat_width.deletions)),
                ]
                .spacing(6)
                .align_y(alignment::Vertical::Center),
            )
            .padding([5, 10]),
        )
        .width(Length::Fill)
        .padding(0)
        .style(move |_, status| sidebar_child_button_style(status, selected, theme))
        .on_press(Message::SelectFile(index)),
        container(text(raw_path).size(CAPTION_TEXT_SIZE).color(theme.text))
            .padding([5, 8])
            .style(move |_| tooltip_style(theme)),
        tooltip::Position::Right,
    )
    .into()
}

#[derive(Debug, Clone, Copy)]
struct SidebarFileStatWidth {
    additions: f32,
    deletions: f32,
}

fn sidebar_file_stat_widths(files: &[DiffFile]) -> SidebarFileStatWidth {
    let max_addition_chars = files
        .iter()
        .map(|file| prefixed_count_len(file.additions))
        .max()
        .unwrap_or(2);
    let max_deletion_chars = files
        .iter()
        .map(|file| prefixed_count_len(file.deletions))
        .max()
        .unwrap_or(2);

    SidebarFileStatWidth {
        additions: sidebar_file_stat_width(max_addition_chars),
        deletions: sidebar_file_stat_width(max_deletion_chars),
    }
}

fn sidebar_file_stat_width(chars: usize) -> f32 {
    (chars as f32 * SIDEBAR_FILE_STAT_CHAR_WIDTH + SIDEBAR_FILE_STAT_PADDING)
        .max(SIDEBAR_FILE_STAT_MIN_WIDTH)
}

fn prefixed_count_len(count: usize) -> usize {
    count.to_string().len() + 1
}

fn build_diff_panel(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    if matches!(ui.status, LoadStatus::Loading) && ui.document.files.is_empty() {
        return container(text(""))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(move |_| panel_style(theme.panel_background, theme))
            .into();
    }

    if ui.document.files.is_empty() {
        return container(text("No Changes Loaded").size(16).color(theme.muted_text))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(move |_| panel_style(theme.panel_background, theme))
            .into();
    }

    let files = ui
        .document
        .files
        .iter()
        .map(|file| DiffFileView {
            title: match &file.old_path {
                Some(old_path) if old_path != &file.path => format!("{old_path} -> {}", file.path),
                _ => file.path.clone(),
            },
            status: file.status.label(),
            hunks: &file.hunks,
            additions: file.additions,
            deletions: file.deletions,
        })
        .collect::<Vec<_>>();

    let content = render_diff(
        files,
        ui.selected_file,
        ui.selected_revision.view_key(),
        theme,
    );

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(0)
        .clip(true)
        .style(move |_| panel_style(theme.panel_background, theme))
        .into()
}

fn render_diff<'a>(
    files: Vec<DiffFileView<'a>>,
    selected_file: usize,
    revision_key: String,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    DiffView::new(
        files,
        selected_file,
        revision_key,
        diff_palette(theme),
        CODE_FONT,
        CODE_TEXT_SIZE,
    )
    .into()
}

fn build_selection_gutter(selected: bool, theme: ThemeSpec) -> Element<'static, Message> {
    if selected {
        build_selection_stripe(theme)
    } else {
        container(text(""))
            .width(Length::Fixed(3.0))
            .height(Length::Fixed(28.0))
            .into()
    }
}

fn build_selection_stripe(theme: ThemeSpec) -> Element<'static, Message> {
    container(text(""))
        .width(Length::Fixed(3.0))
        .height(Length::Fill)
        .style(move |_| stripe_style(theme.accent, CONTROL_RADIUS))
        .into()
}

fn build_file_status_badge<'a>(
    label: &'a str,
    background: Color,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    container(text(label).size(CAPTION_TEXT_SIZE).color(theme.background))
        .padding([2, 6])
        .style(move |_| badge_style(background, theme))
        .into()
}

fn build_sidebar_divider(theme: ThemeSpec) -> Element<'static, Message> {
    container(text(""))
        .height(Length::Fixed(1.0))
        .width(Length::Fill)
        .style(move |_| sidebar_divider_style(theme))
        .into()
}

fn format_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn diff_palette(theme: ThemeSpec) -> Palette {
    Palette {
        text: theme.text,
        text_muted: theme.subtle_text,
        addition_text: theme.added_text,
        deletion_text: theme.removed_text,
        modified_token: theme.modified_token,
        conflict_marker: theme.conflict_marker,
        note_text: theme.note_text,
        panel: theme.panel_background_elevated,
        file_header: theme.file_header,
        hunk_header: theme.hunk_header,
        addition_background: theme.added_line,
        deletion_background: theme.removed_line,
        note_background: theme.note_background,
        gutter_background: theme.panel_background,
        border: theme.border,
    }
}

fn app_shell_style(theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(theme.background)
        .color(theme.text)
}

fn sidebar_header_style(theme: ThemeSpec) -> container::Style {
    container::Style::default().background(theme.panel_background)
}

fn panel_style(background: Color, theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(background)
        .border(panel_border(theme))
}

fn badge_style(background: Color, theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(background)
        .border(Border {
            width: 1.0,
            color: theme.border,
            radius: CONTROL_RADIUS.into(),
        })
}

fn stripe_style(background: Color, radius: f32) -> container::Style {
    container::Style::default()
        .background(background)
        .border(Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: radius.into(),
        })
}

fn sidebar_divider_style(theme: ThemeSpec) -> container::Style {
    container::Style::default().background(theme.border)
}

fn indicator_chip_style(background: Color, theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(background)
        .border(Border {
            width: 1.0,
            color: theme.border,
            radius: CONTROL_RADIUS.into(),
        })
}

fn tooltip_style(theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(theme.panel_background_elevated)
        .color(theme.text)
        .border(Border {
            width: 1.0,
            color: theme.border,
            radius: CONTROL_RADIUS.into(),
        })
}

fn sidebar_button_style(status: button::Status, selected: bool, theme: ThemeSpec) -> button::Style {
    let background = if selected {
        theme.selected_file
    } else {
        theme.panel_background_elevated
    };

    let mut style = button::Style {
        background: Some(Background::Color(background)),
        text_color: theme.text,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 0.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    };

    match status {
        button::Status::Hovered => {
            style.background = Some(Background::Color(theme.selected_file));
        }
        button::Status::Pressed => {
            style.background = Some(Background::Color(theme.selected_file));
        }
        button::Status::Disabled => {
            style.text_color = theme.subtle_text;
        }
        button::Status::Active => {}
    }

    style
}

fn sidebar_child_button_style(
    status: button::Status,
    selected: bool,
    theme: ThemeSpec,
) -> button::Style {
    let background = match (selected, status) {
        (true, _) => theme.selected_file,
        (false, button::Status::Hovered) => theme.file_header,
        (false, button::Status::Pressed) => theme.selected_file,
        (false, _) => theme.panel_background,
    };

    let mut style = sidebar_button_style(status, selected, theme);
    style.background = Some(Background::Color(background));

    style
}

fn diff_scrollable_style(
    iced_theme: &Theme,
    status: scrollable::Status,
    theme: ThemeSpec,
) -> scrollable::Style {
    let mut style = scrollable::default(iced_theme, status);
    let hovered = matches!(
        status,
        scrollable::Status::Hovered {
            is_vertical_scrollbar_hovered: true,
            ..
        }
    );
    let dragged = matches!(
        status,
        scrollable::Status::Dragged {
            is_vertical_scrollbar_dragged: true,
            ..
        }
    );
    let thumb_color = if dragged {
        theme.accent
    } else if hovered {
        theme.text
    } else {
        theme.muted_text
    };

    style.container = container::Style::default();
    style.vertical_rail.background = Some(Background::Color(theme.panel_background_elevated));
    style.vertical_rail.border = Border {
        width: 1.0,
        color: theme.border,
        radius: 0.0.into(),
    };
    style.vertical_rail.scroller.background = Background::Color(thumb_color);
    style.vertical_rail.scroller.border = Border {
        width: 1.0,
        color: if dragged { theme.accent } else { theme.border },
        radius: CONTROL_RADIUS.into(),
    };
    style.horizontal_rail = style.vertical_rail;
    style.gap = Some(Background::Color(theme.panel_background_elevated));
    style
}

fn panel_border(theme: ThemeSpec) -> Border {
    Border {
        width: 1.0,
        color: theme.border,
        radius: PANEL_RADIUS.into(),
    }
}

fn display_scope(repository: &Repository) -> String {
    if repository.scope.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        repository.scope.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff_file(path: &str) -> DiffFile {
        DiffFile {
            path: path.to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn git_commit_diff_uses_selected_revision() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Git,
            scope: PathBuf::new(),
        };

        let args =
            git_backend_command(&repository, &RevisionSelection::Commit("abc123".to_owned()));

        assert!(args.contains(&OsString::from("show")));
        assert!(args.contains(&OsString::from("abc123")));
        assert_eq!(args.last(), Some(&OsString::from("--")));
    }

    #[test]
    fn parses_commit_log_rows() {
        let commits = parse_commit_log("abc\tdef\tme@example.com\tfalse\tadd commit sidebar\n");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].change_id, "abc");
        assert_eq!(commits[0].commit_id, "def");
        assert_eq!(commits[0].author, "me@example.com");
        assert_eq!(commits[0].description, "add commit sidebar");
        assert!(commits[0].has_description);
        assert_eq!(commits[0].is_empty, Some(false));
    }

    #[test]
    fn parses_commit_log_rows_without_description() {
        let commits = parse_commit_log("abc\tdef\tme@example.com\ttrue\t\n");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].description, "(no description set)");
        assert!(!commits[0].has_description);
        assert_eq!(commits[0].is_empty, Some(true));
    }

    #[test]
    fn revision_id_prefix_uses_shortest_unique_change_id() {
        let commits = parse_commit_log(
            "abc\tone\tme@example.com\tfalse\tfirst\nabd\ttwo\tme@example.com\tfalse\tsecond\nz\three\tme@example.com\ttrue\tthird\n",
        );

        assert_eq!(shortest_unique_prefix_len("abc", &commits), 3);
        assert_eq!(shortest_unique_prefix_len("abd", &commits), 3);
        assert_eq!(shortest_unique_prefix_len("z", &commits), 1);
    }

    #[test]
    fn commit_log_rows_select_full_revision_id() {
        let commits = parse_commit_log(
            "abc\tdef123456789abcdef\tme@example.com\tfalse\tadd commit sidebar\n",
        );

        assert_eq!(commits[0].change_id, "abc");
        assert_eq!(commits[0].commit_id, "def123456789abcdef");
        assert_eq!(commits[0].revision_id, "def123456789abcdef");
    }

    #[test]
    fn revision_id_display_len_keeps_shortest_unique_prefix() {
        let long_id = "abcdefghijklmnopqrstuvwxyz";

        assert_eq!(
            revision_id_display_len(3, long_id),
            SIDEBAR_REVISION_ID_CHARS
        );
        assert_eq!(revision_id_display_len(16, long_id), 16);
        assert_eq!(revision_id_display_len(99, long_id), long_id.len());
    }

    #[test]
    fn parses_git_diff_into_hunks_and_rows() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Jj,
            scope: PathBuf::new(),
        };

        let document = parse_backend_output(
            &repository,
            "diff --git a/src/main.rs b/src/main.rs\nindex 123..456 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,2 +10,3 @@ fn demo()\n let old_value = 0;\n-let old_value = 1;\n+let new_value = 1;\n+let second_line = 2;\n",
        );

        assert_eq!(document.files.len(), 1);
        assert_eq!(document.total_additions, 2);
        assert_eq!(document.total_deletions, 1);

        let file = &document.files[0];
        assert_eq!(file.path, "src/main.rs");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines[0].old_line, Some(10));
        assert_eq!(file.hunks[0].lines[0].new_line, Some(10));
        assert_eq!(file.hunks[0].lines[1].kind, DiffLineKind::Deletion);
        assert_eq!(file.hunks[0].lines[1].old_line, Some(11));
        assert_eq!(file.hunks[0].lines[1].new_line, None);
        assert_eq!(file.hunks[0].lines[2].kind, DiffLineKind::Addition);
        assert_eq!(file.hunks[0].lines[2].old_line, None);
        assert_eq!(file.hunks[0].lines[2].new_line, Some(11));
        assert!(!file.hunks[0].lines[2].syntax.is_empty());
    }

    #[test]
    fn parses_conflict_markers() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Jj,
            scope: PathBuf::new(),
        };

        let document = parse_backend_output(
            &repository,
            "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,1 @@\n<<<<<<< mine\n",
        );

        assert_eq!(
            document.files[0].hunks[0].lines[0].kind,
            DiffLineKind::Conflict
        );
    }

    #[test]
    fn parses_rename_metadata() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Git,
            scope: PathBuf::new(),
        };

        let document = parse_backend_output(
            &repository,
            "diff --git a/src/old.rs b/src/new.rs\nsimilarity index 100%\nrename from src/old.rs\nrename to src/new.rs\n",
        );

        let file = &document.files[0];
        assert_eq!(file.status, DiffFileStatus::Renamed);
        assert_eq!(file.old_path.as_deref(), Some("src/old.rs"));
        assert_eq!(file.path, "src/new.rs");
    }

    #[test]
    fn empty_diff_uses_root_scope_label() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Jj,
            scope: PathBuf::new(),
        };

        let document = parse_backend_output(&repository, "");

        assert_eq!(document.files[0].path, ".");
    }

    #[test]
    fn sidebar_display_keeps_unique_basename_primary_and_full_secondary_when_it_fits() {
        let files = vec![diff_file("packages/frontend/src/components/Button.rs")];

        let display = sidebar_file_display_models(&files, 400.0);

        assert_eq!(
            display,
            [SidebarFileDisplay {
                primary: "Button.rs".to_owned(),
                secondary: "packages/frontend/src/components".to_owned(),
                raw_path: "packages/frontend/src/components/Button.rs".to_owned(),
            }]
        );
    }

    #[test]
    fn sidebar_display_abbreviates_secondary_only_when_width_is_tight() {
        let files = vec![diff_file("packages/frontend/src/components/Button.rs")];

        let display = sidebar_file_display_models(&files, SIDEBAR_FILE_TEXT_CHAR_WIDTH * 16.0);

        assert_eq!(display[0].primary, "Button.rs");
        assert_eq!(display[0].secondary, "packages/f/s/components");
    }

    #[test]
    fn sidebar_display_uses_shortest_unique_suffix_for_colliding_basenames() {
        let files = vec![
            diff_file("crates/ui/src/main.rs"),
            diff_file("crates/cli/src/main.rs"),
            diff_file("crates/worker/src/lib.rs"),
        ];

        let display = sidebar_file_display_models(&files, 400.0);

        assert_eq!(display[0].primary, "ui/src/main.rs");
        assert_eq!(display[0].secondary, "crates");
        assert_eq!(display[1].primary, "cli/src/main.rs");
        assert_eq!(display[1].secondary, "crates");
        assert_eq!(display[2].primary, "lib.rs");
        assert_eq!(display[2].secondary, "crates/worker/src");
    }

    #[test]
    fn sidebar_display_collision_secondary_is_common_root_only() {
        let files = vec![
            diff_file("workspace/package-a/src/Button.rs"),
            diff_file("workspace/package-b/test/Button.rs"),
        ];

        let display = sidebar_file_display_models(&files, 400.0);

        assert_eq!(display[0].primary, "src/Button.rs");
        assert_eq!(display[1].primary, "test/Button.rs");
        assert_eq!(display[0].secondary, "workspace");
        assert_eq!(display[1].secondary, "workspace");
    }

    #[test]
    fn sidebar_display_handles_collision_at_repository_root() {
        let files = vec![diff_file("src/main.rs"), diff_file("tests/main.rs")];

        let display = sidebar_file_display_models(&files, 400.0);

        assert_eq!(display[0].primary, "src/main.rs");
        assert_eq!(display[0].secondary, "");
        assert_eq!(display[1].primary, "tests/main.rs");
        assert_eq!(display[1].secondary, "");
    }

    #[test]
    fn sidebar_display_root_file_has_empty_secondary() {
        let files = vec![diff_file("Cargo.lock")];

        let display = sidebar_file_display_models(&files, 400.0);

        assert_eq!(display[0].primary, "Cargo.lock");
        assert_eq!(display[0].secondary, "");
    }

    #[test]
    fn sidebar_display_preserves_root_and_leaf_secondary_segments() {
        assert_eq!(
            abbreviate_secondary_path(&["workspace", "packages", "frontend", "src"]),
            "workspace/p/f/src"
        );
        assert_eq!(abbreviate_secondary_path(&["src"]), "src");
        assert_eq!(abbreviate_secondary_path(&[]), "");
    }

    #[test]
    fn sidebar_display_middle_truncates_only_prepended_primary_directories() {
        let primary = truncate_primary_display(
            "very/long/generated/module/path/component/Button.rs",
            "Button.rs",
            SIDEBAR_FILE_TEXT_CHAR_WIDTH * 24.0,
        );

        assert_eq!(primary, "very/lo…ponent/Button.rs");
        assert_eq!(primary.chars().count(), 24);
        assert!(primary.ends_with("/Button.rs"));
    }

    #[test]
    fn sidebar_display_protects_basename_when_width_is_tiny() {
        assert_eq!(
            truncate_primary_display(
                "deeply/nested/source/Button.rs",
                "Button.rs",
                SIDEBAR_FILE_TEXT_CHAR_WIDTH * 6.0,
            ),
            "Button.rs"
        );
    }
}

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
    time::Duration,
};

mod config;
mod diff_view;
mod graph;
mod graph_view;
mod resize_handle;
mod revision_list;
mod scrollbar;

use anyhow::{Context, Result, bail};
use arborium::{
    GrammarStore,
    advanced::{CompiledGrammar, ParseContext},
};
use bstr::BStr;
use clap::Parser;
use config::AppConfig;
use diff_view::{
    DiffFileView, DiffHunkView, DiffLine, DiffLineKind, DiffView, Palette, SyntaxKind, SyntaxSpan,
};
use futures::StreamExt;
use graph::{LaneFrame, assign_lanes};
use graph_view::RevisionGraphStyle;
use iced::{
    Background, Border, Color, Element, Font, Length, Shadow, Subscription, Task, Theme, alignment,
    event::{self, Event},
    keyboard, system, theme, time,
    widget::{button, column, container, row, stack, text},
    window,
};
use jj_lib::{
    backend::CommitId,
    config::{ConfigSource, StackedConfig},
    conflicts::{ConflictMarkerStyle, ConflictMaterializeOptions, materialized_diff_stream},
    copies::CopyRecords,
    diff_presentation::{
        LineCompareMode,
        unified::{DiffLineType, git_diff_part, unified_diff_hunks},
    },
    files::FileMergeHunkLevel,
    fileset::{
        FilesetAliasesMap, FilesetDiagnostics, FilesetExpression, FilesetParseContext,
        parse as parse_fileset,
    },
    gitignore::GitIgnoreFile,
    graph::{GraphEdge, GraphNode, TopoGroupedGraphIterator},
    matchers::{EverythingMatcher, Matcher, NothingMatcher, PrefixMatcher},
    merge::{Diff, SameChange},
    object_id::ObjectId,
    repo::{Repo, StoreFactories},
    repo_path::{RepoPath, RepoPathBuf, RepoPathUiConverter},
    revset::{RevsetExpression, SymbolResolver},
    settings::{HumanByteSize, UserSettings},
    tree_merge::MergeOptions,
    working_copy::SnapshotOptions,
    workspace::{Workspace, default_working_copy_factories},
};
use resize_handle::ResizeHandle;
use revision_list::{
    FileRowView, IndicatorChip, Item as RevisionListItem, RevisionList, RevisionListStyle,
    RevisionRowView, RowSelectionKey,
};
use tokio::process::Command;

const CODE_TEXT_SIZE: f32 = 13.0;
const CAPTION_TEXT_SIZE: f32 = 14.0;
const SMALL_TEXT_SIZE: f32 = 15.0;
const TITLE_TEXT_SIZE: f32 = 20.0;
const SIDEBAR_REVISION_ID_CHARS: usize = 12;
const SIDEBAR_COMMIT_ID_CHARS: usize = 12;
const CONTROL_RADIUS: f32 = 5.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 360.0;
const SIDEBAR_MIN_WIDTH: f32 = 220.0;
const SIDEBAR_MAX_WIDTH: f32 = 800.0;
const SIDEBAR_RESIZE_HIT_PADDING: f32 = 2.0;
// Floor for the badge column. We measure the actual status labels to size the
// column, but a single-character label like "M" can render thinner than the
// chip looks tasteful at, so we keep a small visual minimum.
const SIDEBAR_FILE_BADGE_MIN_WIDTH: f32 = 22.0;
const SIDEBAR_FILE_BADGE_HORIZONTAL_PADDING: f32 = 6.0;
// Horizontal padding flanking the `+N` / `-N` numeric columns.
const SIDEBAR_FILE_STAT_HORIZONTAL_PADDING: f32 = 4.0;
const SIDEBAR_FILE_STAT_MIN_WIDTH: f32 = 24.0;
const SIDEBAR_FILE_ROW_GAP: f32 = 6.0;
const SIDEBAR_FILE_ROW_HORIZONTAL_PADDING: f32 = 20.0;
const SIDEBAR_GRAPH_LANE_WIDTH: f32 = 10.0;
const SIDEBAR_GRAPH_GUTTER_PADDING: f32 = 8.0;
const SIDEBAR_GRAPH_LEFT_PADDING: f32 = 8.0;
const SCROLLBAR_WIDTH: f32 = 12.0;
const SCROLLBAR_PADDING: f32 = 2.0;
const REPOSITORY_REFRESH_INTERVAL: Duration = Duration::from_millis(1_000);

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
    repository_snapshot: Option<RepositorySnapshot>,
    snapshot_pending: bool,
    app_focused: bool,
    selected_theme: ThemePreference,
    system_theme: theme::Mode,
    selected_file: usize,
    sidebar_width: f32,
    config: AppConfig,
    revision_details: Option<RevisionDetails>,
}

#[derive(Debug, Clone)]
struct Repository {
    root: PathBuf,
    vcs: Vcs,
    scope: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepositorySnapshot {
    fingerprint: String,
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
    lane_frame: LaneFrame,
    is_working_copy: bool,
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
    BackendLoaded(RevisionSelection, Box<Result<BackendOutput, String>>),
    RepositorySnapshotLoaded(Result<RepositorySnapshot, String>),
    SelectFile(usize),
    SelectRowKey(revision_list::RowSelectionKey),
    SelectTheme(ThemePreference),
    SystemThemeChanged(theme::Mode),
    WindowFocusChanged(bool),
    RefreshRepository,
    SelectNextFile,
    SelectPreviousFile,
    CopyToClipboard(String),
    SidebarWidthChanged(f32),
}

#[derive(Debug, Clone)]
struct BackendOutput {
    document: DiffDocument,
    commits: Vec<CommitSummary>,
    snapshot: RepositorySnapshot,
    details: Option<RevisionDetails>,
}

/// `jj show`-style summary of a single revision, used to render the header
/// strip above the diff view.
#[derive(Debug, Clone, Default)]
struct RevisionDetails {
    commit_id: String,
    change_id: Option<String>,
    bookmarks: Vec<String>,
    author: SignatureInfo,
    committer: Option<SignatureInfo>,
    signature: Option<String>,
    description: String,
}

#[derive(Debug, Clone, Default)]
struct SignatureInfo {
    name: String,
    email: String,
    timestamp: Option<String>,
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
        let config = AppConfig::load();
        match prepare_repository(&cli.path) {
            Ok(repository) => {
                let revision = RevisionSelection::WorkingCopy;
                let backend_task = Task::perform(
                    load_backend(repository.clone(), revision.clone()),
                    move |result| Message::BackendLoaded(revision, Box::new(result)),
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
                        repository_snapshot: None,
                        snapshot_pending: false,
                        app_focused: true,
                        selected_theme: ThemePreference::System,
                        system_theme: theme::Mode::None,
                        selected_file: 0,
                        sidebar_width: SIDEBAR_DEFAULT_WIDTH,
                        config,
                        revision_details: None,
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
                    repository_snapshot: None,
                    snapshot_pending: false,
                    app_focused: true,
                    selected_theme: ThemePreference::System,
                    system_theme: theme::Mode::None,
                    selected_file: 0,
                    sidebar_width: SIDEBAR_DEFAULT_WIDTH,
                    config,
                    revision_details: None,
                },
                system::theme().map(Message::SystemThemeChanged),
            ),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::BackendLoaded(revision, result) => match *result {
                Ok(output) => {
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
                    self.repository_snapshot = Some(output.snapshot);
                    self.revision_details = output.details;
                    self.selected_file = if revision_changed {
                        0
                    } else {
                        self.selected_file
                            .min(self.document.files.len().saturating_sub(1))
                    };
                }
                Err(error) => {
                    if self.pending_revision.as_ref() != Some(&revision) {
                        return Task::none();
                    }

                    self.pending_revision = None;
                    self.status = LoadStatus::Failed(error);
                }
            },
            Message::RepositorySnapshotLoaded(Ok(snapshot)) => {
                self.snapshot_pending = false;
                if self.repository_snapshot.as_ref() != Some(&snapshot)
                    && self.pending_revision.is_none()
                    && let Some(repository) = self.repository.clone()
                {
                    let revision = self.selected_revision.clone();
                    self.pending_revision = Some(revision.clone());
                    return Task::perform(
                        load_backend(repository, revision.clone()),
                        move |result| Message::BackendLoaded(revision, Box::new(result)),
                    );
                }
            }
            Message::RepositorySnapshotLoaded(Err(error)) => {
                self.snapshot_pending = false;
                self.status = LoadStatus::Failed(error);
            }
            Message::SelectFile(index) => {
                if index < self.document.files.len() {
                    self.selected_file = index;
                    return scroll_sidebar_to_file(index, self);
                }
            }
            Message::SelectRowKey(key) => {
                let selection = match key {
                    revision_list::RowSelectionKey::WorkingCopy => RevisionSelection::WorkingCopy,
                    revision_list::RowSelectionKey::Commit(id) => RevisionSelection::Commit(id),
                };
                if self.selected_revision != selection
                    && self.pending_revision.as_ref() != Some(&selection)
                    && let Some(repository) = self.repository.clone()
                {
                    self.pending_revision = Some(selection.clone());
                    let revision = selection.clone();
                    return Task::perform(load_backend(repository, selection), move |result| {
                        Message::BackendLoaded(revision, Box::new(result))
                    });
                }
            }
            Message::SelectTheme(theme) => {
                self.selected_theme = theme;
            }
            Message::SystemThemeChanged(theme) => {
                self.system_theme = theme;
            }
            Message::WindowFocusChanged(focused) => {
                let gained_focus = focused && !self.app_focused;
                self.app_focused = focused;

                if gained_focus {
                    return self.start_repository_snapshot();
                }
            }
            Message::RefreshRepository => {
                if self.app_focused {
                    return self.start_repository_snapshot();
                }
            }
            Message::SelectNextFile => {
                if !self.document.files.is_empty() {
                    self.selected_file =
                        (self.selected_file + 1).min(self.document.files.len().saturating_sub(1));
                    return scroll_sidebar_to_file(self.selected_file, self);
                }
            }
            Message::SelectPreviousFile => {
                let previous = self.selected_file.saturating_sub(1);
                if previous != self.selected_file {
                    self.selected_file = previous;
                    return scroll_sidebar_to_file(self.selected_file, self);
                }
            }
            Message::CopyToClipboard(text) => {
                return iced::clipboard::write(text).discard();
            }
            Message::SidebarWidthChanged(width) => {
                self.sidebar_width = width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
            }
        }

        Task::none()
    }

    fn start_repository_snapshot(&mut self) -> Task<Message> {
        if self.snapshot_pending {
            return Task::none();
        }

        let Some(repository) = self.repository.clone() else {
            return Task::none();
        };

        self.snapshot_pending = true;
        Task::perform(load_repository_snapshot(repository), |result| {
            Message::RepositorySnapshotLoaded(result.map_err(|error| format!("{error:#}")))
        })
    }

    fn view(&self) -> Element<'_, Message> {
        let theme = self.resolved_theme().spec();
        let panels = row![
            build_sidebar(self, theme),
            vertical_divider(theme),
            build_diff_panel(self, theme),
        ]
        .spacing(0)
        .height(Length::Fill);
        let resize_overlay = ResizeHandle::new(
            self.sidebar_width,
            SIDEBAR_MIN_WIDTH,
            SIDEBAR_MAX_WIDTH,
            SIDEBAR_RESIZE_HIT_PADDING,
            Message::SidebarWidthChanged,
        );
        let content = stack![panels, resize_overlay]
            .width(Length::Fill)
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

        let focus = event::listen().filter_map(|event| match event {
            Event::Window(window::Event::Focused) => Some(Message::WindowFocusChanged(true)),
            Event::Window(window::Event::Unfocused) => Some(Message::WindowFocusChanged(false)),
            _ => None,
        });
        let refresh = if self.app_focused {
            time::every(REPOSITORY_REFRESH_INTERVAL).map(|_| Message::RefreshRepository)
        } else {
            Subscription::none()
        };

        Subscription::batch([
            keyboard,
            focus,
            refresh,
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
    let (document, details) = match repository.vcs {
        Vcs::Jj => {
            let repository = repository.clone();
            let revision = revision.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || handle.block_on(load_jj_diff(repository, revision)))
                .await
                .context("jj diff loader task failed")??
        }
        Vcs::Git => {
            let args = git_backend_command(&repository, &revision);
            let output = run_command(&repository.root, "git", args).await?;
            let document = parse_backend_output(&repository, &output);
            let details = load_git_revision_details(&repository, &revision).await.ok();
            (document, details)
        }
    };
    let snapshot = run_repository_snapshot(repository).await?;

    Ok(BackendOutput {
        document,
        commits,
        snapshot,
        details,
    })
}

async fn load_repository_snapshot(repository: Repository) -> Result<RepositorySnapshot> {
    run_repository_snapshot(repository).await
}

async fn run_repository_snapshot(repository: Repository) -> Result<RepositorySnapshot> {
    match repository.vcs {
        Vcs::Jj => {
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(load_jj_repository_snapshot(repository))
            })
            .await
            .context("jj repository snapshot task failed")?
        }
        Vcs::Git => {
            let output = run_command(
                &repository.root,
                "git",
                vec![
                    OsString::from("status"),
                    OsString::from("--porcelain=v1"),
                    OsString::from("--branch"),
                    OsString::from("--untracked-files=normal"),
                ],
            )
            .await?;
            Ok(RepositorySnapshot {
                fingerprint: output,
            })
        }
    }
}

fn git_backend_command(repository: &Repository, revision: &RevisionSelection) -> Vec<OsString> {
    let mut args: Vec<OsString> = match revision {
        RevisionSelection::WorkingCopy => {
            // `git diff HEAD` covers both staged and unstaged changes
            // against the last committed state — the closest analog to
            // jj's @ working-copy diff. Untracked files are not included
            // (git diff only walks tracked paths).
            ["diff", "HEAD", "--no-ext-diff", "--no-color", "--"]
                .into_iter()
                .map(OsString::from)
                .collect()
        }
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
                    OsString::from("--topo-order"),
                    OsString::from("--pretty=format:%h%x09%H%x09%P%x09%an%x09%x09%s"),
                ],
            )
            .await?;

            let mut rows = parse_commit_log_rows(&output);
            // Git has no native @ commit, so synthesize a working-copy row at
            // the top whenever the tree differs from HEAD. The row's parent
            // is HEAD so the graph keeps a continuous lane.
            if !rows.is_empty() && git_has_uncommitted_changes(&repository.root).await? {
                let head_id = rows[0].commit_id.clone();
                rows.insert(
                    0,
                    ParsedCommitRow {
                        change_id: GIT_WORKING_COPY_ID.to_owned(),
                        commit_id: GIT_WORKING_COPY_ID.to_owned(),
                        parents: vec![head_id],
                        author: String::new(),
                        is_empty: None,
                        description: "Working copy".to_owned(),
                        has_description: true,
                        is_working_copy: true,
                    },
                );
            }
            Ok(build_commit_summaries(rows))
        }
    }
}

async fn git_has_uncommitted_changes(repository_root: &Path) -> Result<bool> {
    // `git status --porcelain` prints one line per change (staged, unstaged,
    // or untracked) and nothing on a clean tree.
    let output = run_command(
        repository_root,
        "git",
        vec![OsString::from("status"), OsString::from("--porcelain")],
    )
    .await?;
    Ok(!output.trim().is_empty())
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
        .context("jj workspace has no working-copy commit")?
        .clone();

    // Default revset: all ancestors of the working copy (inclusive). Once the
    // CLI grows a -r flag, parse it here instead.
    let expr = RevsetExpression::commit(wc_commit_id.clone()).ancestors();
    let symbol_resolver = SymbolResolver::new(
        repo.as_ref(),
        &[] as &[Box<dyn jj_lib::revset::SymbolResolverExtension>],
    );
    let resolved = expr
        .resolve_user_expression(repo.as_ref(), &symbol_resolver)
        .context("failed to resolve jj revset")?;
    let revset = resolved
        .evaluate(repo.as_ref())
        .context("failed to evaluate jj revset")?;

    let nodes: Vec<GraphNode<CommitId>> = {
        let mut topo = TopoGroupedGraphIterator::new(revset.iter_graph(), |id: &CommitId| id);
        topo.prioritize_branch(wc_commit_id.clone());
        topo.collect::<Result<Vec<_>, _>>()
            .context("failed to walk jj revset graph")?
    };
    drop(revset);

    let lane_rows = assign_lanes(nodes.iter().map(|(id, edges)| (id.clone(), edges.clone())));

    let mut commits = Vec::with_capacity(nodes.len());
    for ((id, _edges), lane_row) in nodes.into_iter().zip(lane_rows) {
        let commit = repo
            .store()
            .get_commit_async(&id)
            .await
            .with_context(|| format!("failed to load jj commit {}", id.hex()))?;

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

        let commit_id_hex = commit.id().hex();
        let is_working_copy = commit.id() == &wc_commit_id;
        commits.push(CommitSummary {
            change_id: commit.change_id().to_string(),
            commit_id: commit_id_hex.clone(),
            revision_id: commit_id_hex,
            shortest_change_id_len: Some(shortest_change_id_len),
            description: if description.is_empty() {
                "(no description set)".to_owned()
            } else {
                description.to_owned()
            },
            author: commit.author().name.clone(),
            has_description: !description.is_empty(),
            is_empty: Some(is_empty),
            lane_frame: LaneFrame::from_lane_row(&lane_row),
            is_working_copy,
        });
    }

    Ok(commits)
}

async fn load_jj_diff(
    repository: Repository,
    revision: RevisionSelection,
) -> Result<(DiffDocument, Option<RevisionDetails>)> {
    let settings = jj_settings(&repository.root)?;
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
    let details = jj_revision_details(repo.as_ref(), &commit);
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

    let total_additions = files.iter().map(|file| file.additions).sum();
    let total_deletions = files.iter().map(|file| file.deletions).sum();

    Ok((
        DiffDocument {
            files,
            total_additions,
            total_deletions,
        },
        Some(details),
    ))
}

fn jj_revision_details(repo: &dyn Repo, commit: &jj_lib::commit::Commit) -> RevisionDetails {
    let commit_id = commit.id().clone();
    let change_id = commit.change_id().to_string();

    // Build a flat list mirroring `jj show`'s "Bookmarks:" line:
    // local name first ("main"), then each remote tracking ref as
    // `name@remote` ("main@git", "main@origin"). Skip names whose targets
    // don't actually point at this commit.
    let mut bookmarks: Vec<String> = Vec::new();
    for (name, target) in repo.view().bookmarks() {
        if target.local_target.added_ids().any(|id| id == &commit_id) {
            bookmarks.push(name.as_str().to_owned());
        }
        for (remote, remote_ref) in &target.remote_refs {
            if remote_ref.target.added_ids().any(|id| id == &commit_id) {
                bookmarks.push(format!("{}@{}", name.as_str(), remote.as_str()));
            }
        }
    }

    let author = jj_signature_info(commit.author());
    let committer = jj_signature_info(commit.committer());

    RevisionDetails {
        commit_id: commit.id().hex(),
        change_id: Some(change_id),
        bookmarks,
        author,
        committer: Some(committer),
        signature: None,
        description: commit.description().to_owned(),
    }
}

fn jj_signature_info(signature: &jj_lib::backend::Signature) -> SignatureInfo {
    SignatureInfo {
        name: signature.name.clone(),
        email: signature.email.clone(),
        timestamp: Some(format_jj_timestamp(&signature.timestamp)),
    }
}

fn format_jj_timestamp(ts: &jj_lib::backend::Timestamp) -> String {
    // jj_lib::backend::Timestamp is a (millis_since_epoch, tz_offset_minutes)
    // pair. We render it using the recorded offset so the timestamp matches
    // what the author actually saw on their clock.
    let total_minutes = ts.tz_offset;
    let total_secs = ts.timestamp.0 / 1000 + total_minutes as i64 * 60;
    let secs = total_secs.rem_euclid(86_400);
    let day = total_secs.div_euclid(86_400);
    let (year, month, mday) = civil_date_from_days(day);
    let hour = (secs / 3600) as u32;
    let minute = ((secs / 60) % 60) as u32;
    let second = (secs % 60) as u32;
    let sign = if total_minutes >= 0 { '+' } else { '-' };
    let offset_hours = total_minutes.abs() / 60;
    let offset_mins = total_minutes.abs() % 60;
    format!(
        "{year:04}-{month:02}-{mday:02} {hour:02}:{minute:02}:{second:02} {sign}{offset_hours:02}{offset_mins:02}"
    )
}

/// Convert a count of days since the Unix epoch (1970-01-01) into a
/// proleptic Gregorian (year, month, day) tuple. Used so we don't have to
/// pull in `chrono`/`time` just to print timestamps in the revision header.
fn civil_date_from_days(days: i64) -> (i32, u32, u32) {
    // Algorithm from Howard Hinnant's "chrono-Compatible Low-Level Date
    // Algorithms" — converts shifted-era days into year/month/day, then
    // rotates back to a calendar starting in January.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year as i32, month, day)
}

// Fallback used only when the user has not configured `snapshot.max-new-file-size`.
// Matches jj-cli's shipped default (1 MiB).
const DEFAULT_SNAPSHOT_MAX_NEW_FILE_SIZE: u64 = 1024 * 1024;

fn jj_settings(repo_root: &Path) -> Result<UserSettings> {
    let mut config = StackedConfig::with_defaults();

    for path in jj_user_config_paths() {
        load_jj_user_config_path(&mut config, &path)?;
    }

    let repo_config = repo_root.join(".jj").join("repo").join("config.toml");
    if repo_config.is_file() {
        config
            .load_file(ConfigSource::Repo, repo_config.clone())
            .with_context(|| format!("failed to load jj repo config {}", repo_config.display()))?;
    }

    UserSettings::from_config(config).context("failed to build jj settings")
}

fn jj_user_config_paths() -> Vec<PathBuf> {
    if let Ok(env_paths) = env::var("JJ_CONFIG")
        && !env_paths.is_empty()
    {
        let sep = if cfg!(windows) { ';' } else { ':' };
        return env_paths
            .split(sep)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
    }

    let mut paths = Vec::new();
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        paths.push(PathBuf::from(xdg).join("jj"));
    }
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        paths.push(home.join(".config").join("jj"));
        if cfg!(target_os = "macos") {
            paths.push(home.join("Library").join("Application Support").join("jj"));
        }
    }
    paths
}

fn load_jj_user_config_path(config: &mut StackedConfig, path: &Path) -> Result<()> {
    if path.is_file() {
        config
            .load_file(ConfigSource::User, path.to_path_buf())
            .with_context(|| format!("failed to load jj config file {}", path.display()))?;
    } else if path.is_dir() {
        config
            .load_dir(ConfigSource::User, path)
            .with_context(|| format!("failed to load jj config dir {}", path.display()))?;
    }
    Ok(())
}

fn snapshot_max_new_file_size(settings: &UserSettings) -> Result<u64> {
    use jj_lib::config::ConfigGetError;
    match settings.get_value_with("snapshot.max-new-file-size", HumanByteSize::try_from) {
        Ok(size) => Ok(size.0),
        Err(ConfigGetError::NotFound { .. }) => Ok(DEFAULT_SNAPSHOT_MAX_NEW_FILE_SIZE),
        Err(err) => Err(err).context("invalid snapshot.max-new-file-size"),
    }
}

fn snapshot_auto_track_matcher(
    settings: &UserSettings,
    repo_root: &Path,
) -> Result<Box<dyn Matcher>> {
    use jj_lib::config::ConfigGetError;
    let raw = match settings.get_string("snapshot.auto-track") {
        Ok(value) => value,
        Err(ConfigGetError::NotFound { .. }) => "all()".to_string(),
        Err(err) => return Err(err).context("invalid snapshot.auto-track"),
    };
    let aliases = FilesetAliasesMap::new();
    let path_converter = RepoPathUiConverter::Fs {
        cwd: repo_root.to_path_buf(),
        base: repo_root.to_path_buf(),
    };
    let context = FilesetParseContext {
        aliases_map: &aliases,
        path_converter: &path_converter,
    };
    let mut diagnostics = FilesetDiagnostics::new();
    let expr: FilesetExpression = parse_fileset(&mut diagnostics, &raw, &context)
        .with_context(|| format!("failed to parse snapshot.auto-track {raw:?}"))?;
    Ok(expr.to_matcher())
}

// `LocalWorkingCopy` walks the repo tree and reads in-tree `.gitignore` files
// itself, so we only need to provide the *out-of-tree* ignores: the user's
// global git ignore and (for git-backed repos) `.git/info/exclude`.
fn snapshot_base_ignores(repo_root: &Path) -> Result<Arc<GitIgnoreFile>> {
    let mut ignores = GitIgnoreFile::empty();

    if let Some(global) = user_global_git_ignore_path() {
        ignores = ignores
            .chain_with_file("", global.clone())
            .with_context(|| format!("failed to read user gitignore {}", global.display()))?;
    }

    let info_exclude = repo_root.join(".git").join("info").join("exclude");
    ignores = ignores
        .chain_with_file("", info_exclude.clone())
        .with_context(|| format!("failed to read {}", info_exclude.display()))?;

    Ok(ignores)
}

fn user_global_git_ignore_path() -> Option<PathBuf> {
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("git").join("ignore"));
    }
    let home = env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("git")
            .join("ignore"),
    )
}

async fn load_jj_repository_snapshot(repository: Repository) -> Result<RepositorySnapshot> {
    let settings = jj_settings(&repository.root)?;
    let mut workspace = Workspace::load(
        &settings,
        &repository.root,
        &StoreFactories::default(),
        &default_working_copy_factories(),
    )
    .context("failed to load jj workspace")?;
    let workspace_name = workspace.workspace_name().to_owned();

    let auto_track = snapshot_auto_track_matcher(&settings, &repository.root)?;
    let base_ignores = snapshot_base_ignores(&repository.root)?;
    let max_new_file_size = snapshot_max_new_file_size(&settings)?;

    // Take the working-copy lock *before* reading the repo head. Otherwise a
    // jj-cli command running between `load_at_head` and the lock can rewrite
    // the wc commit out from under us, and our snapshot tx — still parented on
    // the stale op — lands as a sibling of the cli's op. Both ops touch the
    // same change_id with different commit_ids, which jj's concurrent-op
    // resolver presents as a divergent change.
    let repo_loader = workspace.repo_loader().clone();
    let mut locked_ws = workspace
        .start_working_copy_mutation()
        .context("failed to lock jj working copy")?;

    let base_repo = repo_loader
        .load_at_head()
        .await
        .context("failed to load jj repo")?;
    let wc_commit_id = base_repo
        .view()
        .get_wc_commit_id(&workspace_name)
        .context("jj workspace has no working-copy commit")?
        .clone();
    let wc_commit = base_repo
        .store()
        .get_commit_async(&wc_commit_id)
        .await
        .with_context(|| {
            format!(
                "failed to load jj working-copy commit {}",
                wc_commit_id.hex()
            )
        })?;
    let old_tree = wc_commit.tree();

    let snapshot_options = SnapshotOptions {
        base_ignores,
        progress: None,
        start_tracking_matcher: auto_track.as_ref(),
        force_tracking_matcher: &NothingMatcher,
        max_new_file_size,
    };
    let (new_tree, _stats) = locked_ws
        .locked_wc()
        .snapshot(&snapshot_options)
        .await
        .context("failed to snapshot jj working copy")?;

    if new_tree.tree_ids_and_labels() == old_tree.tree_ids_and_labels() {
        // No file changes: drop the lock without writing an op. This is the
        // common case on idle ticks and keeps `jj op log` clean.
        return Ok(RepositorySnapshot {
            fingerprint: base_repo.op_id().hex(),
        });
    }

    let mut tx = base_repo.start_transaction();
    tx.set_is_snapshot(true);
    let new_commit = tx
        .repo_mut()
        .rewrite_commit(&wc_commit)
        .set_tree(new_tree)
        .write()
        .await
        .context("failed to rewrite jj working-copy commit with new tree")?;
    tx.repo_mut()
        .set_wc_commit(workspace_name, new_commit.id().clone())
        .context("failed to update jj working-copy pointer")?;
    // `rewrite_commit` records a rewrite that the transaction insists on
    // resolving before commit, even when the wc commit has no descendants.
    tx.repo_mut()
        .rebase_descendants()
        .await
        .context("failed to rebase descendants after jj snapshot")?;
    let new_repo = tx
        .commit("snapshot working copy")
        .await
        .context("failed to commit jj snapshot transaction")?;
    let new_op_id = new_repo.op_id().clone();
    locked_ws
        .finish(new_op_id.clone())
        .await
        .context("failed to finish jj working-copy mutation")?;

    Ok(RepositorySnapshot {
        fingerprint: new_op_id.hex(),
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

/// Sentinel commit_id used for the synthetic git working-copy row.
/// Selection short-circuits on `is_working_copy` before this value is ever
/// passed to git, so it just needs to be visually distinct and not collide
/// with a real hex hash.
const GIT_WORKING_COPY_ID: &str = "wc";

struct ParsedCommitRow {
    change_id: String,
    commit_id: String,
    parents: Vec<String>,
    author: String,
    is_empty: Option<bool>,
    description: String,
    has_description: bool,
    is_working_copy: bool,
}

#[cfg(test)]
fn parse_commit_log(output: &str) -> Vec<CommitSummary> {
    build_commit_summaries(parse_commit_log_rows(output))
}

fn parse_commit_log_rows(output: &str) -> Vec<ParsedCommitRow> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(5, '\t');
            let change_id = parts.next()?.trim();
            let commit_id = parts.next()?.trim();
            let parents_field = parts.next().unwrap_or("");
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

            let parents: Vec<String> = parents_field
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let has_description = !description.is_empty();
            Some(ParsedCommitRow {
                change_id: change_id.to_owned(),
                commit_id: commit_id.to_owned(),
                parents,
                author: author.to_owned(),
                is_empty: empty,
                description: if description.is_empty() {
                    "(no description set)".to_owned()
                } else {
                    description.to_owned()
                },
                has_description,
                is_working_copy: false,
            })
        })
        .collect()
}

fn build_commit_summaries(rows: Vec<ParsedCommitRow>) -> Vec<CommitSummary> {
    // Walk the rows in their existing topo order and assign lanes from
    // parent edges. Parents not present in the listing (shallow clone, etc.)
    // become Missing edges so the renderer can draw a stub.
    let known: std::collections::HashSet<&str> =
        rows.iter().map(|row| row.commit_id.as_str()).collect();
    let lane_inputs = rows.iter().map(|row| {
        let edges: Vec<GraphEdge<String>> = row
            .parents
            .iter()
            .map(|parent| {
                if known.contains(parent.as_str()) {
                    GraphEdge::direct(parent.clone())
                } else {
                    GraphEdge::missing(parent.clone())
                }
            })
            .collect();
        (row.commit_id.clone(), edges)
    });
    let lane_rows = assign_lanes(lane_inputs);

    rows.into_iter()
        .zip(lane_rows)
        .map(|(row, lane_row)| CommitSummary {
            change_id: row.change_id,
            commit_id: row.commit_id.clone(),
            revision_id: row.commit_id,
            shortest_change_id_len: None,
            description: row.description,
            author: row.author,
            has_description: row.has_description,
            is_empty: row.is_empty,
            lane_frame: LaneFrame::from_lane_row(&lane_row),
            is_working_copy: row.is_working_copy,
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

fn parse_backend_output(_repository: &Repository, output: &str) -> DiffDocument {
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

/// Apply syntax highlighting to all visible diff lines for `file`.
///
/// We previously fed each line to tree-sitter individually, which was
/// fundamentally wrong: tree-sitter expects a complete document, so a line
/// like `fn foo(` parses as an error, `}` on its own gets no captures, and
/// every multi-line construct (string literals, function bodies, doc
/// comments, raw strings) is invisible to the parser. The result was that
/// keywords mid-block went un-highlighted and noisy single-character lines
/// would silently fall back to plain text.
///
/// The fix: reconstruct each "side" (old and new) of the file as a single
/// contiguous document, parse it once, and map the resulting spans back to
/// individual lines. Blank lines fill the gaps between hunks so each
/// surviving line still sits at its original line number — the parser
/// won't see the surrounding code, but tree-sitter is reasonably tolerant
/// of missing top-level constructs and will still recover local syntax
/// (literals, identifiers, comments, keywords) correctly within each hunk.
///
/// Context lines are highlighted from the new side (they're identical on
/// both sides, but we only need to look them up once); deletions come from
/// the old side; additions from the new side. Note/Conflict lines are
/// rendered as plain text — they aren't real source content.
fn apply_syntax_highlighting(file: &mut DiffFile) {
    static GRAMMAR_STORE: OnceLock<GrammarStore> = OnceLock::new();

    let Some(language) = arborium::detect_language(&file.path) else {
        return;
    };
    let store = GRAMMAR_STORE.get_or_init(GrammarStore::new);
    let Some(grammar) = store.get(language) else {
        return;
    };

    let new_spans = parse_side(&grammar, file, Side::New);
    let old_spans = parse_side(&grammar, file, Side::Old);

    for (hunk_index, hunk) in file.hunks.iter_mut().enumerate() {
        for (line_index, line) in hunk.lines.iter_mut().enumerate() {
            if matches!(line.kind, DiffLineKind::Note | DiffLineKind::Conflict) {
                continue;
            }

            let key = (hunk_index, line_index);
            let spans = match line.kind {
                DiffLineKind::Deletion => old_spans.get(&key),
                DiffLineKind::Addition | DiffLineKind::Context => new_spans.get(&key),
                DiffLineKind::Note | DiffLineKind::Conflict => None,
            };

            if let Some(spans) = spans {
                line.syntax = spans.clone();
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

/// Reconstruct one side of `file` as a single document, parse it, and slice
/// the resulting captures into per-line span lists keyed by
/// `(hunk_index, line_index)`.
fn parse_side(
    grammar: &Arc<CompiledGrammar>,
    file: &DiffFile,
    side: Side,
) -> HashMap<(usize, usize), Vec<SyntaxSpan>> {
    // For each line we keep on this side, record its byte range in the
    // reconstructed buffer so we can map captures back later.
    struct LineRange {
        hunk_index: usize,
        line_index: usize,
        start: usize,
        end: usize,
    }

    let mut buf = String::new();
    let mut ranges: Vec<LineRange> = Vec::new();
    // 1-indexed cursor over the source-file line numbers we've reached so far.
    let mut current_source_line: usize = 1;

    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        for (line_index, line) in hunk.lines.iter().enumerate() {
            let included = matches!(
                (side, line.kind),
                (Side::Old, DiffLineKind::Context | DiffLineKind::Deletion)
                    | (Side::New, DiffLineKind::Context | DiffLineKind::Addition)
            );
            if !included {
                continue;
            }

            let source_line = match side {
                Side::Old => line.old_line,
                Side::New => line.new_line,
            };
            let Some(target) = source_line else {
                continue;
            };

            // Pad blank lines so this content sits at its true line number.
            // Tree-sitter will see structurally-meaningless gaps but its
            // error-recovery handles that cleanly for most languages.
            while current_source_line < target {
                buf.push('\n');
                current_source_line += 1;
            }

            let start = buf.len();
            buf.push_str(&line.content);
            let end = buf.len();
            buf.push('\n');
            current_source_line += 1;

            ranges.push(LineRange {
                hunk_index,
                line_index,
                start,
                end,
            });
        }
    }

    if buf.trim().is_empty() || ranges.is_empty() {
        return HashMap::new();
    }

    let Ok(mut context) = ParseContext::for_grammar(grammar) else {
        return HashMap::new();
    };
    let result = grammar.parse(&mut context, &buf);

    let mut per_line: HashMap<(usize, usize), Vec<SyntaxSpan>> = HashMap::new();

    for span in result.spans {
        let Some(kind) = syntax_kind_for_capture(&span.capture) else {
            continue;
        };
        let (Ok(start), Ok(end)) = (usize::try_from(span.start), usize::try_from(span.end)) else {
            continue;
        };
        if start >= end || end > buf.len() {
            continue;
        }
        if !buf.is_char_boundary(start) || !buf.is_char_boundary(end) {
            continue;
        }

        // Walk every line that the span overlaps. Most spans live entirely
        // within one line so this loop usually fires once, but multi-line
        // constructs (block comments, raw strings) need to highlight every
        // covered line.
        for range in &ranges {
            if range.end <= start || range.start >= end {
                continue;
            }
            let local_start = start.saturating_sub(range.start);
            let local_end = (end - range.start).min(range.end - range.start);
            if local_start >= local_end {
                continue;
            }
            per_line
                .entry((range.hunk_index, range.line_index))
                .or_default()
                .push(SyntaxSpan {
                    start: local_start,
                    end: local_end,
                    kind,
                });
        }
    }

    for spans in per_line.values_mut() {
        *spans = normalize_syntax_spans(std::mem::take(spans));
    }
    per_line
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
        .map(|repository| match scope_label(repository) {
            Some(scope) => format!("{} · {scope}", repository.vcs.label()),
            None => repository.vcs.label().to_owned(),
        })
        .unwrap_or_else(|| "Outside Repository".to_owned());

    let title_row = row![
        text("Changes")
            .size(TITLE_TEXT_SIZE)
            .color(theme.text)
            .width(Length::Fill),
        build_theme_switcher(ui.selected_theme, theme),
    ]
    .spacing(10)
    .align_y(alignment::Vertical::Center);

    let mut header_content = column![title_row].spacing(7);

    if !ui.document.files.is_empty() {
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
        header_content = header_content.push(metrics);
    }

    header_content = header_content.push(
        text(repo_label)
            .size(CAPTION_TEXT_SIZE)
            .color(theme.subtle_text),
    );

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

    let revision_list = build_revision_list(ui, theme);

    let body = column![sidebar_header, revision_list].spacing(0);

    container(body)
        .width(Length::Fixed(ui.sidebar_width))
        .height(Length::Fill)
        .style(move |_| sidebar_panel_style(theme))
        .into()
}

fn build_revision_list(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let mut items: Vec<RevisionListItem> = Vec::with_capacity(ui.commits.len());
    let metrics = sidebar_text_metrics(ui.config);

    let (file_widgets, file_badge_width): (Option<Vec<FileRowTemplate>>, f32) =
        if matches!(ui.status, LoadStatus::Loaded) && !ui.document.files.is_empty() {
            let widest_addition = ui
                .document
                .files
                .iter()
                .map(|file| format!("+{}", file.additions))
                .max_by(|a, b| {
                    metrics
                        .measure(a)
                        .partial_cmp(&metrics.measure(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or_else(|| "+0".to_owned());
            let widest_deletion = ui
                .document
                .files
                .iter()
                .map(|file| format!("-{}", file.deletions))
                .max_by(|a, b| {
                    metrics
                        .measure(a)
                        .partial_cmp(&metrics.measure(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or_else(|| "-0".to_owned());
            let additions_w = sidebar_file_stat_width(&widest_addition, &metrics);
            let deletions_w = sidebar_file_stat_width(&widest_deletion, &metrics);
            let badge_w = sidebar_file_badge_width(&ui.document.files, &metrics);

            // Mirror `draw_file`'s layout exactly so truncation kicks in at
            // the same threshold the renderer clips at:
            //   [gutter] [badge] gap [path] gap [+N] gap [-N] right_pad
            // Previously this used 4 gaps and the full horizontal padding,
            // and ignored the graph gutter entirely — so paths bled past
            // the +N/-N columns whenever the expanded commit had any lanes.
            let expanded_lane_count = ui
                .commits
                .iter()
                .find(
                    |commit| match (&ui.expanded_revision, commit.is_working_copy) {
                        (RevisionSelection::WorkingCopy, true) => true,
                        (RevisionSelection::Commit(id), false) => id == &commit.revision_id,
                        _ => false,
                    },
                )
                .map(|commit| commit.lane_frame.after.len())
                .unwrap_or(0);
            let gutter_total = SIDEBAR_GRAPH_LEFT_PADDING
                + expanded_lane_count as f32 * SIDEBAR_GRAPH_LANE_WIDTH
                + SIDEBAR_GRAPH_GUTTER_PADDING;
            let right_pad = SIDEBAR_FILE_ROW_HORIZONTAL_PADDING / 2.0;
            let reserved = gutter_total
                + badge_w
                + additions_w
                + deletions_w
                + SIDEBAR_FILE_ROW_GAP * 3.0
                + right_pad;
            let display_width = (ui.sidebar_width - reserved).max(0.0);
            let display_models =
                sidebar_file_display_models(&ui.document.files, display_width, &metrics);
            (
                Some(
                    ui.document
                        .files
                        .iter()
                        .enumerate()
                        .map(|(idx, file)| FileRowTemplate {
                            primary: display_models[idx].primary.clone(),
                            secondary: display_models[idx].secondary.clone(),
                            raw_path: display_models[idx].raw_path.clone(),
                            status_label: file.status.short_label().to_owned(),
                            status_background: file.status.short_badge_color(theme),
                            additions: file.additions,
                            deletions: file.deletions,
                            file_index: idx,
                            additions_width: additions_w,
                            deletions_width: deletions_w,
                        })
                        .collect(),
                ),
                badge_w,
            )
        } else {
            (None, SIDEBAR_FILE_BADGE_MIN_WIDTH)
        };

    for commit in &ui.commits {
        let unique_len = shortest_unique_prefix_len(&commit.change_id, &ui.commits);
        let label_len = revision_id_display_len(unique_len, &commit.change_id);
        let id_prefix: String = commit.change_id.chars().take(unique_len).collect();
        let id_suffix: String = commit
            .change_id
            .chars()
            .skip(unique_len)
            .take(label_len.saturating_sub(unique_len))
            .collect();
        let commit_id_short = truncate_end(&commit.commit_id, SIDEBAR_COMMIT_ID_CHARS);

        let mut indicators = Vec::new();
        if commit.is_working_copy {
            indicators.push(IndicatorChip {
                label: "@".to_owned(),
                background: chip_background(theme.accent),
                text_color: theme.accent,
            });
        }
        if commit.is_empty == Some(true) {
            indicators.push(IndicatorChip {
                label: "empty".to_owned(),
                background: chip_background(theme.subtle_text),
                text_color: theme.subtle_text,
            });
        }

        let selection_key = if commit.is_working_copy {
            RowSelectionKey::WorkingCopy
        } else {
            RowSelectionKey::Commit(commit.revision_id.clone())
        };

        let is_expanded = match (&ui.expanded_revision, commit.is_working_copy) {
            (RevisionSelection::WorkingCopy, true) => true,
            (RevisionSelection::Commit(id), false) => id == &commit.revision_id,
            _ => false,
        };

        items.push(RevisionListItem::Revision(RevisionRowView {
            selection_key,
            change_id_prefix: id_prefix,
            change_id_suffix: id_suffix,
            commit_id_short,
            author: commit.author.clone(),
            description: commit.description.clone(),
            description_color: commit_description_color(commit, theme),
            indicators,
            frame: commit.lane_frame.clone(),
        }));

        if is_expanded && let Some(files) = &file_widgets {
            let continuation = commit.lane_frame.after.clone();
            for file in files {
                items.push(RevisionListItem::File(FileRowView {
                    primary: file.primary.clone(),
                    secondary: file.secondary.clone(),
                    raw_path: file.raw_path.clone(),
                    status_label: file.status_label.clone(),
                    status_background: file.status_background,
                    status_text: theme.background,
                    additions: file.additions,
                    deletions: file.deletions,
                    additions_text: theme.added_text,
                    deletions_text: theme.removed_text,
                    continuation: continuation.clone(),
                    additions_width: file.additions_width,
                    deletions_width: file.deletions_width,
                    primary_color: theme.text,
                    secondary_color: theme.muted_text,
                    file_index: file.file_index,
                }));
            }
        }
    }

    let selected_row = match &ui.selected_revision {
        RevisionSelection::WorkingCopy => Some(RowSelectionKey::WorkingCopy),
        RevisionSelection::Commit(id) => Some(RowSelectionKey::Commit(id.clone())),
    };

    RevisionList::new(
        items,
        selected_row,
        Some(ui.selected_file),
        revision_list_style(theme, ui.config, file_badge_width),
        Message::SelectRowKey,
        Message::SelectFile,
    )
    .width(Length::Fill)
    .into()
}

struct FileRowTemplate {
    primary: String,
    secondary: String,
    raw_path: String,
    status_label: String,
    status_background: Color,
    additions: usize,
    deletions: usize,
    file_index: usize,
    additions_width: f32,
    deletions_width: f32,
}

fn revision_list_style(
    theme: ThemeSpec,
    config: AppConfig,
    file_badge_width: f32,
) -> RevisionListStyle {
    RevisionListStyle {
        graph: RevisionGraphStyle {
            lane_width: SIDEBAR_GRAPH_LANE_WIDTH,
            line_thickness: 1.5,
            node_radius: 3.5,
            // Lane 0 (the trunk) wears the theme accent; subsequent lanes
            // and the node discs that sit on them derive their hue from
            // this — see `RevisionGraphStyle::lane_color`.
            lane_base_color: theme.accent,
            missing_color: theme.subtle_text,
        },
        revision_row_height: 46.0,
        file_row_height: 42.0,
        gutter_left_padding: SIDEBAR_GRAPH_LEFT_PADDING,
        gutter_padding: SIDEBAR_GRAPH_GUTTER_PADDING,
        content_padding: 12.0,
        background: theme.panel_background,
        selected_background: theme.selected_file,
        border: theme.border,
        muted_text: theme.muted_text,
        subtle_text: theme.subtle_text,
        accent_text: theme.accent,
        indicator_radius: CONTROL_RADIUS,
        small_text_size: SMALL_TEXT_SIZE,
        caption_text_size: CAPTION_TEXT_SIZE,
        primary_font: config.ui_font,
        mono_font: config.mono_font,
        file_badge_width,
        file_row_gap: SIDEBAR_FILE_ROW_GAP,
        file_row_right_pad: SIDEBAR_FILE_ROW_HORIZONTAL_PADDING / 2.0,
        tooltip_background: theme.panel_background_elevated,
        tooltip_text: theme.text,
        tooltip_border: theme.border,
        tooltip_radius: CONTROL_RADIUS,
        tooltip_padding: 6.0,
        tooltip_gap: 8.0,
        scrollbar: scrollbar_style(theme),
    }
}

/// Translucent chip background derived from the chip's text color. This way
/// the chip reads independently of whether the row is selected — its visual
/// frame comes from the tint rather than from the row's solid background.
fn chip_background(color: Color) -> Color {
    Color { a: 0.20, ..color }
}

fn scroll_sidebar_to_file(_file_index: usize, _ui: &Diffui) -> Task<Message> {
    // TODO: re-implement scroll-to-reveal against `RevisionList`'s internal
    // scroll state once the widget exposes a scrollable operation.
    Task::none()
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
                .style(move |_, status| theme_switcher_button_style(status, selected, theme))
                .on_press(Message::SelectTheme(candidate)),
        );
    }

    controls.into()
}

fn revision_id_display_len(unique_len: usize, revision_id: &str) -> usize {
    SIDEBAR_REVISION_ID_CHARS
        .max(unique_len)
        .min(revision_id.chars().count())
}

fn truncate_end(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn commit_description_color(commit: &CommitSummary, theme: ThemeSpec) -> Color {
    if commit.has_description {
        return theme.text;
    }

    match commit.is_empty {
        Some(true) => theme.added_text,
        Some(false) => theme.note_text,
        None => theme.note_text,
    }
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

/// Pixel-accurate text width measurement for layout decisions made outside
/// the renderer (path truncation, badge column sizing, etc.).
///
/// We previously approximated text width with `chars * 7px` heuristics, which
/// silently misbehaved for any glyph wider or narrower than the assumed
/// average — `@` clipped into `…` in revision IDs, abbreviated paths over- or
/// under-shot the available room, and badges would clip if the user ever
/// switched to a larger font. Going through real `cosmic_text` shaping fixes
/// the entire class of bug because we use the same engine the wgpu renderer
/// uses, so the measurements match what gets drawn.
///
/// Why headless `iced::advanced::graphics::text::Paragraph` rather than the
/// renderer's `R::Paragraph`:
///
/// - Path-truncation runs in `view()` (in `build_revision_list`), well before
///   any `draw()` call, and we need exact widths to decide *which* string to
///   hand to the widget. The renderer's `Paragraph` type isn't reachable from
///   here without threading renderer generics through `main.rs`.
/// - The wgpu renderer is built on top of `iced_graphics`, so the headless
///   `Paragraph` shapes text identically — the answer matches the pixels.
///
/// Alternatives considered:
///
/// (a) Pass a `Fn(&str) -> R::Paragraph` closure down from the widget side.
///     Most accurate per-renderer, but spreads renderer generics into
///     `main.rs` for no real win — the underlying engine is the same.
/// (b) Move all truncation into `RevisionList::layout()` so the widget owns
///     it. Clean separation, but the same display models also feed the
///     jump-to-file selection state in `main.rs`, so they'd have to leak
///     back out of the widget anyway.
/// (c) Keep the `chars * px` heuristic. Cheap but wrong for any non-default
///     font and even wrong for the default font on wide glyphs (CJK,
///     emoji, `@`, `_`). Source of multiple past bugs.
///
/// Tests use `Self::fixed_per_char` to stay deterministic regardless of which
/// system fonts happen to be installed on the host.
#[derive(Clone)]
enum TextMetrics {
    Iced {
        font: Font,
        size: f32,
    },
    #[cfg(test)]
    FixedPerChar {
        width: f32,
    },
}

impl TextMetrics {
    fn iced(font: Font, size: f32) -> Self {
        Self::Iced { font, size }
    }

    #[cfg(test)]
    fn fixed_per_char(width: f32) -> Self {
        Self::FixedPerChar { width }
    }

    fn measure(&self, content: &str) -> f32 {
        if content.is_empty() {
            return 0.0;
        }
        match self {
            Self::Iced { font, size } => {
                use iced::advanced::graphics::text::Paragraph;
                use iced::advanced::text::{LineHeight, Paragraph as _, Shaping, Text, Wrapping};
                use iced::{Pixels, Size};

                let line_height = (*size * 1.4).max(1.0);
                let paragraph = Paragraph::with_text(Text {
                    content,
                    bounds: Size::new(f32::INFINITY, line_height),
                    size: Pixels(*size),
                    line_height: LineHeight::Absolute(Pixels(line_height)),
                    font: *font,
                    align_x: iced::advanced::text::Alignment::Left,
                    align_y: alignment::Vertical::Top,
                    shaping: Shaping::Advanced,
                    wrapping: Wrapping::None,
                    ellipsis: iced::advanced::text::Ellipsis::None,
                    hint_factor: None,
                });
                paragraph.min_width()
            }
            #[cfg(test)]
            Self::FixedPerChar { width } => content.chars().count() as f32 * width,
        }
    }
}

fn sidebar_text_metrics(config: AppConfig) -> TextMetrics {
    TextMetrics::iced(config.ui_font, CAPTION_TEXT_SIZE)
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
    metrics: &TextMetrics,
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
                    secondary_display_path(directories, available_width, metrics),
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
                    secondary_display_path(
                        &common_directory_prefix(&group),
                        available_width,
                        metrics,
                    ),
                )
            };

            SidebarFileDisplay {
                primary: truncate_primary_display(&primary, basename, available_width, metrics),
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

fn secondary_display_path(
    segments: &[&str],
    available_width: f32,
    metrics: &TextMetrics,
) -> String {
    let path = segments.join("/");
    if metrics.measure(&path) <= available_width {
        path
    } else {
        abbreviate_secondary_path(segments)
    }
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

fn truncate_primary_display(
    primary: &str,
    basename: &str,
    available_width: f32,
    metrics: &TextMetrics,
) -> String {
    if metrics.measure(primary) <= available_width || primary == basename {
        return primary.to_owned();
    }

    let Some(prefix) = primary.strip_suffix(basename) else {
        return primary.to_owned();
    };
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return primary.to_owned();
    }

    // Always preserve the basename — the user needs to know what file this is.
    // The prefix is what gets squeezed.
    let suffix = format!("/{basename}");
    let suffix_w = metrics.measure(&suffix);
    if suffix_w >= available_width {
        return basename.to_owned();
    }
    let prefix_budget = available_width - suffix_w;

    let truncated_prefix = middle_truncate_to_width(prefix, prefix_budget, metrics);
    if truncated_prefix.is_empty() {
        return basename.to_owned();
    }
    format!("{truncated_prefix}{suffix}")
}

/// Middle-truncate `value` so it fits in `max_width` pixels under `metrics`.
///
/// Reads char-by-char from each side and stops as soon as the rendered width
/// of `head + "…" + tail` exceeds the budget. Linear in chars; fine since
/// these strings are short path segments and we run this once per file.
fn middle_truncate_to_width(value: &str, max_width: f32, metrics: &TextMetrics) -> String {
    if metrics.measure(value) <= max_width {
        return value.to_owned();
    }
    let ellipsis_w = metrics.measure("…");
    if ellipsis_w > max_width {
        return String::new();
    }

    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return String::new();
    }

    let mut head = String::new();
    let mut tail = String::new();
    let mut head_len = 0;
    let mut tail_len = 0;
    // Bias: take from head first when budget allows odd counts.
    let mut take_head = true;

    while head_len + tail_len < chars.len() {
        let next_char = if take_head {
            chars[head_len]
        } else {
            chars[chars.len() - 1 - tail_len]
        };

        let mut candidate_head = head.clone();
        let mut candidate_tail = tail.clone();
        if take_head {
            candidate_head.push(next_char);
        } else {
            candidate_tail.insert(0, next_char);
        }

        let candidate = format!("{candidate_head}…{candidate_tail}");
        if metrics.measure(&candidate) > max_width {
            break;
        }

        head = candidate_head;
        tail = candidate_tail;
        if take_head {
            head_len += 1;
        } else {
            tail_len += 1;
        }
        take_head = !take_head;
    }

    if head_len == 0 && tail_len == 0 {
        // We can fit the ellipsis but not even one neighbouring character.
        return "…".to_owned();
    }
    format!("{head}…{tail}")
}

fn sidebar_file_stat_width(text: &str, metrics: &TextMetrics) -> f32 {
    (metrics.measure(text) + SIDEBAR_FILE_STAT_HORIZONTAL_PADDING * 2.0)
        .max(SIDEBAR_FILE_STAT_MIN_WIDTH)
}

/// Width of the status badge column ("M", "A", "D", "R", …). We measure the
/// widest label in the document and add padding so two-letter labels like
/// "MM" still fit comfortably.
fn sidebar_file_badge_width(files: &[DiffFile], metrics: &TextMetrics) -> f32 {
    let widest = files
        .iter()
        .map(|file| metrics.measure(file.status.short_label()))
        .fold(0.0_f32, f32::max);
    (widest + SIDEBAR_FILE_BADGE_HORIZONTAL_PADDING * 2.0).max(SIDEBAR_FILE_BADGE_MIN_WIDTH)
}

fn build_diff_panel(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let body: Element<'_, Message> =
        if matches!(ui.status, LoadStatus::Loading) && ui.document.files.is_empty() {
            container(text(""))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        } else if ui.document.files.is_empty() && ui.revision_details.is_none() {
            let message = match &ui.status {
                LoadStatus::Failed(_) => "Failed to load changes",
                _ => "No file changes in this revision",
            };
            container(text(message).size(15).color(theme.subtle_text))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        } else {
            let files = ui
                .document
                .files
                .iter()
                .map(|file| DiffFileView {
                    title: match &file.old_path {
                        Some(old_path) if old_path != &file.path => {
                            format!("{old_path} -> {}", file.path)
                        }
                        _ => file.path.clone(),
                    },
                    status: file.status.label(),
                    hunks: &file.hunks,
                    additions: file.additions,
                    deletions: file.deletions,
                })
                .collect::<Vec<_>>();

            let header_lines = ui
                .revision_details
                .as_ref()
                .map(build_header_lines)
                .unwrap_or_default();

            render_diff(
                files,
                ui.selected_file,
                ui.selected_revision.view_key(),
                header_lines,
                theme,
                ui.config,
            )
        };

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(0)
        .clip(true)
        .style(move |_| diff_panel_style(theme))
        .into()
}

fn render_diff<'a>(
    files: Vec<DiffFileView<'a>>,
    selected_file: usize,
    revision_key: String,
    header_lines: Vec<diff_view::HeaderLine>,
    theme: ThemeSpec,
    config: AppConfig,
) -> Element<'a, Message> {
    DiffView::new(
        files,
        selected_file,
        revision_key,
        diff_palette(theme),
        config.mono_font,
        CODE_TEXT_SIZE,
        config.multi_click_ms,
        Message::SelectFile,
    )
    .with_header(header_lines)
    .on_copy(Message::CopyToClipboard)
    .into()
}

/// Format a `RevisionDetails` value into the line-by-line layout the diff
/// view renders at the top of its scroll area. Mirrors `jj show`'s
/// formatting: labels padded to 9 chars, blank line between the metadata
/// block and the indented description.
fn build_header_lines(details: &RevisionDetails) -> Vec<diff_view::HeaderLine> {
    use diff_view::HeaderLine;
    let mut lines: Vec<HeaderLine> = Vec::new();

    lines.push(HeaderLine::field("Commit ID", &details.commit_id));
    if let Some(change_id) = &details.change_id {
        lines.push(HeaderLine::field("Change ID", change_id));
    }
    if !details.bookmarks.is_empty() {
        lines.push(HeaderLine::field("Bookmarks", &details.bookmarks.join(" ")));
    }
    lines.push(HeaderLine::field(
        "Author",
        &format_signature_line(&details.author),
    ));
    if let Some(committer) = &details.committer {
        lines.push(HeaderLine::field(
            "Committer",
            &format_signature_line(committer),
        ));
    }
    if let Some(sig) = &details.signature {
        lines.push(HeaderLine::field("Signature", sig));
    }

    if !details.description.is_empty() {
        lines.push(HeaderLine::blank());
        for line in details.description.lines() {
            lines.push(HeaderLine::description(line));
        }
    }

    lines
}

fn format_signature_line(sig: &SignatureInfo) -> String {
    let mut parts = String::new();
    if !sig.name.is_empty() {
        parts.push_str(&sig.name);
    }
    if !sig.email.is_empty() {
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push('<');
        parts.push_str(&sig.email);
        parts.push('>');
    }
    if let Some(ts) = &sig.timestamp
        && !ts.is_empty()
    {
        parts.push_str(" (");
        parts.push_str(ts);
        parts.push(')');
    }
    parts
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
        // Translucent accent so the underlying syntax-highlighted text and
        // line-change tints stay readable under the selection.
        selection: Color {
            a: 0.30,
            ..theme.accent
        },
        scrollbar: scrollbar_style(theme),
    }
}

fn scrollbar_style(theme: ThemeSpec) -> scrollbar::ScrollbarStyle {
    scrollbar::ScrollbarStyle {
        width: SCROLLBAR_WIDTH,
        padding: SCROLLBAR_PADDING,
        // Soft pill behind the thumb, lighter than the thumb so the two
        // read as distinct without looking heavy on light themes.
        track_color: Color {
            a: 0.18,
            ..theme.muted_text
        },
        thumb_color: Color {
            a: 0.55,
            ..theme.muted_text
        },
    }
}

fn app_shell_style(theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(theme.background)
        .color(theme.text)
}

fn vertical_divider(theme: ThemeSpec) -> Element<'static, Message> {
    container(text(""))
        .width(Length::Fixed(1.0))
        .height(Length::Fill)
        .style(move |_| {
            container::Style::default()
                .background(theme.border)
                .border(Border {
                    width: 0.0,
                    color: Color::TRANSPARENT,
                    radius: 0.0.into(),
                })
        })
        .into()
}

fn sidebar_header_style(theme: ThemeSpec) -> container::Style {
    container::Style::default().background(theme.panel_background)
}

fn sidebar_panel_style(theme: ThemeSpec) -> container::Style {
    container::Style::default().background(theme.panel_background)
}

fn diff_panel_style(theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(theme.panel_background)
        .border(Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 0.0.into(),
        })
}

fn theme_switcher_button_style(
    status: button::Status,
    selected: bool,
    theme: ThemeSpec,
) -> button::Style {
    let background = match (selected, status) {
        (true, _) => theme.selected_file,
        (false, button::Status::Hovered | button::Status::Pressed) => theme.selected_file,
        (false, _) => theme.panel_background_elevated,
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: theme.text,
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: 0.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

fn scope_label(repository: &Repository) -> Option<String> {
    if repository.scope.as_os_str().is_empty() {
        None
    } else {
        Some(repository.scope.display().to_string())
    }
}

async fn load_git_revision_details(
    repository: &Repository,
    revision: &RevisionSelection,
) -> Result<RevisionDetails> {
    // %x1f is a unit-separator byte chosen to be unlikely in commit metadata,
    // so we can split the fields cleanly even when names or descriptions
    // contain tabs/newlines.
    const SEP: &str = "\x1f";
    let target = match revision {
        RevisionSelection::WorkingCopy => "HEAD".to_owned(),
        RevisionSelection::Commit(id) => id.clone(),
    };
    let format = format!("%H{SEP}%an{SEP}%ae{SEP}%aI{SEP}%cn{SEP}%ce{SEP}%cI{SEP}%D{SEP}%B");
    let output = run_command(
        &repository.root,
        "git",
        vec![
            OsString::from("show"),
            OsString::from("--no-patch"),
            OsString::from(format!("--format={format}")),
            OsString::from(target),
        ],
    )
    .await?;

    let mut parts = output.splitn(9, '\x1f');
    let commit_id = parts.next().unwrap_or("").trim().to_owned();
    let author_name = parts.next().unwrap_or("").to_owned();
    let author_email = parts.next().unwrap_or("").to_owned();
    let author_date = parts.next().unwrap_or("").to_owned();
    let committer_name = parts.next().unwrap_or("").to_owned();
    let committer_email = parts.next().unwrap_or("").to_owned();
    let committer_date = parts.next().unwrap_or("").to_owned();
    let refs = parts.next().unwrap_or("").to_owned();
    let description = parts.next().unwrap_or("").trim_end().to_owned();

    let bookmarks: Vec<String> = refs
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(RevisionDetails {
        commit_id,
        change_id: None,
        bookmarks,
        author: SignatureInfo {
            name: author_name,
            email: author_email,
            timestamp: Some(author_date).filter(|s| !s.is_empty()),
        },
        committer: Some(SignatureInfo {
            name: committer_name,
            email: committer_email,
            timestamp: Some(committer_date).filter(|s| !s.is_empty()),
        }),
        signature: None,
        description,
    })
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
        let commits = parse_commit_log("abc\tdef\t\tme@example.com\tfalse\tadd commit sidebar\n");

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
        let commits = parse_commit_log("abc\tdef\t\tme@example.com\ttrue\t\n");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].description, "(no description set)");
        assert!(!commits[0].has_description);
        assert_eq!(commits[0].is_empty, Some(true));
    }

    #[test]
    fn revision_id_prefix_uses_shortest_unique_change_id() {
        let commits = parse_commit_log(
            "abc\tone\t\tme@example.com\tfalse\tfirst\nabd\ttwo\t\tme@example.com\tfalse\tsecond\nz\three\t\tme@example.com\ttrue\tthird\n",
        );

        assert_eq!(shortest_unique_prefix_len("abc", &commits), 3);
        assert_eq!(shortest_unique_prefix_len("abd", &commits), 3);
        assert_eq!(shortest_unique_prefix_len("z", &commits), 1);
    }

    #[test]
    fn commit_log_rows_select_full_revision_id() {
        let commits = parse_commit_log(
            "abc\tdef123456789abcdef\t\tme@example.com\tfalse\tadd commit sidebar\n",
        );

        assert_eq!(commits[0].change_id, "abc");
        assert_eq!(commits[0].commit_id, "def123456789abcdef");
        assert_eq!(commits[0].revision_id, "def123456789abcdef");
    }

    #[test]
    fn git_log_merge_assigns_distinct_lanes_for_second_parent() {
        // Topo order, descendants first:
        //   M (merge of T and W) - parents: T W
        //   T - parent: A
        //   W - parent: A
        //   A - root, no parents
        let commits = parse_commit_log(
            "M\tM\tT W\tme@example.com\tfalse\tmerge\n\
             T\tT\tA\tme@example.com\tfalse\ttrunk\n\
             W\tW\tA\tme@example.com\tfalse\tside\n\
             A\tA\t\tme@example.com\tfalse\troot\n",
        );

        assert_eq!(commits.len(), 4);
        assert_eq!(commits[0].lane_frame.node_lane, 0);
        assert_eq!(commits[1].lane_frame.node_lane, 0);
        // Second parent of the merge spawns a new lane to the right.
        assert_eq!(commits[2].lane_frame.node_lane, 1);
        // Both lanes converge back at A.
        assert_eq!(commits[3].lane_frame.node_lane, 0);
        assert_eq!(commits[3].lane_frame.merging_lanes, vec![0, 1]);
    }

    #[test]
    fn git_log_marks_unknown_parents_as_missing() {
        // Single commit whose parent isn't in the listing — e.g. a shallow clone.
        let commits = parse_commit_log("abc\tabc\tdeadbeef\tme@example.com\tfalse\thead\n");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].lane_frame.missing_parents, 1);
        assert!(commits[0].lane_frame.after.is_empty());
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
    fn empty_diff_yields_no_files() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Jj,
            scope: PathBuf::new(),
        };

        let document = parse_backend_output(&repository, "");

        assert!(document.files.is_empty());
        assert_eq!(document.total_additions, 0);
        assert_eq!(document.total_deletions, 0);
    }

    #[test]
    fn sidebar_display_keeps_unique_basename_primary_and_full_secondary_when_it_fits() {
        let files = vec![diff_file("packages/frontend/src/components/Button.rs")];

        let display = sidebar_file_display_models(&files, 400.0, &test_metrics());

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

        // Width tight enough that "packages/frontend/src/components" (32 chars
        // at 7px under the test metrics → 224px) doesn't fit, forcing the
        // abbreviation path.
        let display = sidebar_file_display_models(&files, 7.0 * 16.0, &test_metrics());

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

        let display = sidebar_file_display_models(&files, 400.0, &test_metrics());

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

        let display = sidebar_file_display_models(&files, 400.0, &test_metrics());

        assert_eq!(display[0].primary, "src/Button.rs");
        assert_eq!(display[1].primary, "test/Button.rs");
        assert_eq!(display[0].secondary, "workspace");
        assert_eq!(display[1].secondary, "workspace");
    }

    #[test]
    fn sidebar_display_handles_collision_at_repository_root() {
        let files = vec![diff_file("src/main.rs"), diff_file("tests/main.rs")];

        let display = sidebar_file_display_models(&files, 400.0, &test_metrics());

        assert_eq!(display[0].primary, "src/main.rs");
        assert_eq!(display[0].secondary, "");
        assert_eq!(display[1].primary, "tests/main.rs");
        assert_eq!(display[1].secondary, "");
    }

    #[test]
    fn sidebar_display_root_file_has_empty_secondary() {
        let files = vec![diff_file("Cargo.lock")];

        let display = sidebar_file_display_models(&files, 400.0, &test_metrics());

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
        // Budget = 24 chars × 7px under test metrics = 168px.
        let primary = truncate_primary_display(
            "very/long/generated/module/path/component/Button.rs",
            "Button.rs",
            7.0 * 24.0,
            &test_metrics(),
        );

        assert_eq!(primary, "very/lo…ponent/Button.rs");
        assert_eq!(primary.chars().count(), 24);
        assert!(primary.ends_with("/Button.rs"));
    }

    #[test]
    fn sidebar_display_protects_basename_when_width_is_tiny() {
        // Even 6 chars of budget is too tight for "/Button.rs" (10 chars), so
        // the truncator should bail out and just hand back the basename.
        assert_eq!(
            truncate_primary_display(
                "deeply/nested/source/Button.rs",
                "Button.rs",
                7.0 * 6.0,
                &test_metrics(),
            ),
            "Button.rs"
        );
    }

    /// Deterministic metrics for tests: each character is 7px wide, matching
    /// the old `SIDEBAR_FILE_TEXT_CHAR_WIDTH` heuristic so the existing
    /// fixture widths still trigger truncation at the same boundaries. We
    /// don't go through real cosmic_text in tests because system font
    /// availability differs across hosts and would make the assertions
    /// flaky in CI.
    fn test_metrics() -> TextMetrics {
        TextMetrics::fixed_per_char(7.0)
    }
}

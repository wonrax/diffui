use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};

mod diff_view;

use anyhow::{Context, Result, bail};
use clap::Parser;
use diff_view::{
    DiffFileView, DiffHunkView, DiffLine, DiffLineKind, DiffView, Palette, SyntaxKind, SyntaxSpan,
};
use iced::{
    Background, Border, Color, Element, Length, Shadow, Subscription, Task, Theme, alignment,
    keyboard, system, theme,
    widget::{button, column, container, row, scrollable, text},
};
use tokio::process::Command;
use tree_sitter::{Parser as SyntaxParser, Query, QueryCursor, StreamingIterator};

const CODE_FONT: iced::Font = iced::Font::new("Cascadia Code");
const CODE_TEXT_SIZE: f32 = 14.0;
const CAPTION_TEXT_SIZE: f32 = 12.0;
const SMALL_TEXT_SIZE: f32 = 13.0;
const TITLE_TEXT_SIZE: f32 = 17.0;
const PANEL_RADIUS: f32 = 3.0;
const CONTROL_RADIUS: f32 = 5.0;
const SIDEBAR_WIDTH: f32 = 360.0;
const SIDEBAR_FILE_RAIL_WIDTH: f32 = 5.0;
const SIDEBAR_FILE_BADGE_WIDTH: f32 = 24.0;
const SIDEBAR_FILE_STAT_MIN_WIDTH: f32 = 24.0;
const SIDEBAR_FILE_STAT_CHAR_WIDTH: f32 = 7.0;
const SIDEBAR_FILE_STAT_PADDING: f32 = 8.0;
const SIDEBAR_SCROLLBAR_WIDTH: f32 = 10.0;
const SIDEBAR_SCROLLBAR_SCROLLER_WIDTH: f32 = 7.0;
const SIDEBAR_SCROLLBAR_SPACING: f32 = 10.0;

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
    Loaded(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RevisionSelection {
    WorkingCopy,
    Commit(String),
}

impl RevisionSelection {
    fn label(&self) -> &'static str {
        match self {
            Self::WorkingCopy => "Working Copy",
            Self::Commit(_) => "Selected Commit",
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
    metadata: Vec<String>,
    hunks: Vec<DiffHunkView>,
    additions: usize,
    deletions: usize,
}

#[derive(Debug, Clone)]
struct CommitSummary {
    change_id: String,
    commit_id: String,
    description: String,
    author: String,
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
    summary: String,
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

                let revision_changed = self.selected_revision != revision;
                self.selected_revision = revision;
                self.pending_revision = None;
                self.status = LoadStatus::Loaded(output.summary);
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

    fn command(self) -> &'static str {
        match self {
            Self::Jj => "jj",
            Self::Git => "git",
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
    let (program, args) = backend_command(&repository, &revision);
    let output = run_command(&repository.root, program, args).await?;
    let document = parse_backend_output(&repository, &output);

    Ok(BackendOutput {
        summary: format!(
            "Loaded {} {}: {} file(s), {} addition(s), {} deletion(s)",
            repository.vcs.label(),
            revision.label(),
            document.files.len(),
            document.total_additions,
            document.total_deletions,
        ),
        document,
        commits,
    })
}

fn backend_command(
    repository: &Repository,
    revision: &RevisionSelection,
) -> (&'static str, Vec<OsString>) {
    let mut args: Vec<OsString> = match repository.vcs {
        Vcs::Jj => {
            let mut args = vec![
                OsString::from("diff"),
                OsString::from("--git"),
                OsString::from("--color"),
                OsString::from("never"),
            ];

            if let RevisionSelection::Commit(revision) = revision {
                args.push(OsString::from("-r"));
                args.push(OsString::from(revision));
            }

            args.push(OsString::from("--"));
            args
        }
        Vcs::Git => match revision {
            RevisionSelection::WorkingCopy => {
                ["diff", "--"].into_iter().map(OsString::from).collect()
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
        },
    };

    if !repository.scope.as_os_str().is_empty() {
        args.push(repository.scope.as_os_str().to_owned());
    }

    (repository.vcs.command(), args)
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
            let output = run_command(
                &repository.root,
                "jj",
                vec![
                    OsString::from("log"),
                    OsString::from("--no-graph"),
                    OsString::from("-r"),
                    OsString::from("ancestors(@-, 24)"),
                    OsString::from("-T"),
                    OsString::from(
                        "change_id.short() ++ \"\\t\" ++ commit_id.short() ++ \"\\t\" ++ author.email() ++ \"\\t\" ++ description.first_line() ++ \"\\n\"",
                    ),
                ],
            )
            .await?;

            Ok(parse_commit_log(&output))
        }
        Vcs::Git => {
            let output = run_command(
                &repository.root,
                "git",
                vec![
                    OsString::from("log"),
                    OsString::from("--max-count=24"),
                    OsString::from("--pretty=format:%h%x09%H%x09%ae%x09%s"),
                ],
            )
            .await?;

            Ok(parse_commit_log(&output))
        }
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
            let description = parts.next().unwrap_or("").trim();

            if change_id.is_empty() || commit_id.is_empty() {
                return None;
            }

            Some(CommitSummary {
                change_id: change_id.to_owned(),
                commit_id: commit_id.to_owned(),
                description: if description.is_empty() {
                    "(no description set)".to_owned()
                } else {
                    description.to_owned()
                },
                author: author.to_owned(),
            })
        })
        .collect()
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
                metadata: Vec::new(),
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
            metadata: vec!["no changes detected for this scope".to_owned()],
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

    if !matches!(line, "--- /dev/null" | "+++ /dev/null") {
        file.metadata.push(line.to_owned());
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
    if !file.path.ends_with(".rs") {
        return;
    }

    let language = tree_sitter_rust::LANGUAGE.into();
    let Ok(query) = Query::new(&language, tree_sitter_rust::HIGHLIGHTS_QUERY) else {
        return;
    };

    let mut parser = SyntaxParser::new();
    if parser.set_language(&language).is_err() {
        return;
    }

    for hunk in &mut file.hunks {
        for line in &mut hunk.lines {
            if matches!(line.kind, DiffLineKind::Note | DiffLineKind::Conflict) {
                continue;
            }

            line.syntax = highlight_rust_line(&mut parser, &query, &line.content);
        }
    }
}

fn highlight_rust_line(parser: &mut SyntaxParser, query: &Query, content: &str) -> Vec<SyntaxSpan> {
    if content.trim().is_empty() {
        return Vec::new();
    }

    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };

    let mut cursor = QueryCursor::new();
    let mut captures = cursor.captures(query, tree.root_node(), content.as_bytes());
    let capture_names = query.capture_names();
    let mut spans = Vec::new();

    while let Some((query_match, capture_index)) = captures.next() {
        let Some(capture) = query_match.captures.get(*capture_index) else {
            continue;
        };
        let Some(capture_name) = capture_names.get(capture.index as usize) else {
            continue;
        };
        let Some(kind) = syntax_kind_for_capture(capture_name) else {
            continue;
        };

        let start = capture.node.start_byte();
        let end = capture.node.end_byte();
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
    } else if capture.starts_with("number") || capture == "constant.builtin" {
        Some(SyntaxKind::Number)
    } else if capture.starts_with("keyword") || capture == "operator" {
        Some(SyntaxKind::Keyword)
    } else if capture.starts_with("function") || capture == "constructor" {
        Some(SyntaxKind::Function)
    } else if capture.starts_with("type") || capture == "variable.builtin" {
        Some(SyntaxKind::Type)
    } else if capture.starts_with("property") || capture == "variable.parameter" {
        Some(SyntaxKind::Property)
    } else if capture == "punctuation.bracket" || capture == "punctuation.delimiter" {
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
    let status = match &ui.status {
        LoadStatus::Loading => "Loading Changes...".to_owned(),
        LoadStatus::Loaded(summary) => summary.clone(),
        LoadStatus::Failed(error) => format!("Failed: {error}"),
    };
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

    let sidebar_header = container(
        column![
            row![
                text("Changes")
                    .size(TITLE_TEXT_SIZE)
                    .color(theme.text)
                    .width(Length::Fill),
                build_theme_switcher(ui.selected_theme, theme),
            ]
            .spacing(10),
            row![
                build_badge(ui.selected_revision.label(), theme.selected_file, theme),
                metrics,
            ]
            .spacing(8)
            .align_y(alignment::Vertical::Center),
            text(repo_label)
                .size(CAPTION_TEXT_SIZE)
                .color(theme.subtle_text),
            text(status).size(CAPTION_TEXT_SIZE).color(theme.muted_text),
        ]
        .spacing(7),
    )
    .padding([12, 12])
    .style(move |_| sidebar_header_style(theme));

    let mut items = column![
        sidebar_header,
        build_revision_item(
            "Working Copy",
            "Uncommitted Changes",
            RevisionSelection::WorkingCopy,
            &ui.selected_revision,
            ui,
            theme
        ),
    ]
    .spacing(0);

    for commit in &ui.commits {
        items = items.push(build_commit_button(commit, ui, theme));
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

fn build_commit_button<'a>(
    commit: &'a CommitSummary,
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let title = format!("{} {}", commit.change_id, commit.description);
    let subtitle = format!("{} · {}", commit.commit_id, commit.author);

    build_revision_item(
        title,
        subtitle,
        RevisionSelection::Commit(commit.change_id.clone()),
        &ui.selected_revision,
        ui,
        theme,
    )
}

fn build_revision_item<'a>(
    title: impl Into<String>,
    subtitle: impl Into<String>,
    revision: RevisionSelection,
    selected_revision: &RevisionSelection,
    ui: &'a Diffui,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let selected = &revision == selected_revision;
    let title = title.into();
    let subtitle = subtitle.into();
    let loaded = matches!(ui.status, LoadStatus::Loaded(_));
    let file_count = if loaded { ui.document.files.len() } else { 0 };

    let revision_button = button(container(
        row![
            build_selection_gutter(selected, theme),
            container(
                column![
                    row![
                        text(title)
                            .size(SMALL_TEXT_SIZE)
                            .color(theme.text)
                            .width(Length::Fill),
                        if selected {
                            build_count_chip(file_count, theme)
                        } else {
                            container(text(""))
                                .width(Length::Fixed(SIDEBAR_FILE_STAT_MIN_WIDTH))
                                .into()
                        },
                    ]
                    .spacing(8),
                    text(subtitle)
                        .size(CAPTION_TEXT_SIZE)
                        .color(if selected {
                            theme.muted_text
                        } else {
                            theme.subtle_text
                        })
                        .width(Length::Fill),
                ]
                .spacing(4),
            )
            .padding([9, 10])
            .width(Length::Fill),
        ]
        .spacing(0),
    ))
    .width(Length::Fill)
    .padding(0)
    .style(move |_, status| sidebar_button_style(status, selected, theme))
    .on_press(Message::SelectRevision(revision));

    let mut item = column![revision_button, build_sidebar_divider(theme)].spacing(0);

    if selected && loaded && !ui.document.files.is_empty() {
        let mut files = column![].spacing(0);
        let stat_width = sidebar_file_stat_widths(&ui.document.files);

        for (index, file) in ui.document.files.iter().enumerate() {
            files = files.push(
                column![
                    build_nested_file_button(
                        index,
                        file,
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

fn build_count_chip(file_count: usize, theme: ThemeSpec) -> Element<'static, Message> {
    let label = format_count(file_count, "File", "Files");

    container(text(label).size(CAPTION_TEXT_SIZE).color(theme.accent))
        .padding([2, 7])
        .style(move |_| count_chip_style(theme))
        .into()
}

fn build_nested_file_button<'a>(
    index: usize,
    file: &'a DiffFile,
    selected: bool,
    stat_width: SidebarFileStatWidth,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    button(
        container(
            row![
                build_child_tree_rail(selected, theme),
                container(build_file_status_badge(
                    file.status.short_label(),
                    file.status.short_badge_color(theme),
                    theme,
                ))
                .width(Length::Fixed(SIDEBAR_FILE_BADGE_WIDTH)),
                text(file.path.as_str())
                    .size(CAPTION_TEXT_SIZE)
                    .color(theme.text)
                    .width(Length::Fill),
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
    .on_press(Message::SelectFile(index))
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
    if matches!(ui.status, LoadStatus::Loading) {
        return container(text("Loading Changes...").size(16).color(theme.muted_text))
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
            metadata: &file.metadata,
            hunks: &file.hunks,
            additions: file.additions,
            deletions: file.deletions,
        })
        .collect::<Vec<_>>();

    let content = render_diff(files, ui.selected_file, theme);

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
    theme: ThemeSpec,
) -> Element<'a, Message> {
    DiffView::new(
        files,
        selected_file,
        diff_palette(theme),
        CODE_FONT,
        CODE_TEXT_SIZE,
    )
    .into()
}

fn build_badge<'a>(label: &'a str, background: Color, theme: ThemeSpec) -> Element<'a, Message> {
    container(text(label).size(CAPTION_TEXT_SIZE).color(theme.text))
        .padding([4, 10])
        .style(move |_| badge_style(background, theme))
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

fn build_child_tree_rail(selected: bool, theme: ThemeSpec) -> Element<'static, Message> {
    container(text(""))
        .width(Length::Fixed(SIDEBAR_FILE_RAIL_WIDTH))
        .height(Length::Fill)
        .style(move |_| {
            stripe_style(
                if selected { theme.accent } else { theme.border },
                CONTROL_RADIUS,
            )
        })
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
    container::Style::default()
        .background(theme.panel_background)
        .border(panel_border(theme))
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

fn count_chip_style(theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(theme.panel_background)
        .border(Border {
            width: 1.0,
            color: theme.accent,
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

    #[test]
    fn jj_root_scope_does_not_pass_empty_fileset() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Jj,
            scope: PathBuf::new(),
        };

        let (program, args) = backend_command(&repository, &RevisionSelection::WorkingCopy);

        assert_eq!(program, "jj");
        assert_eq!(
            args,
            ["diff", "--git", "--color", "never", "--"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn jj_subdir_scope_is_passed_as_fileset() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Jj,
            scope: PathBuf::from("src"),
        };

        let (_program, args) = backend_command(&repository, &RevisionSelection::WorkingCopy);

        assert_eq!(args.last(), Some(&OsString::from("src")));
    }

    #[test]
    fn jj_commit_diff_uses_selected_revision() {
        let repository = Repository {
            root: PathBuf::from("/repo"),
            vcs: Vcs::Jj,
            scope: PathBuf::new(),
        };

        let (_program, args) =
            backend_command(&repository, &RevisionSelection::Commit("abc123".to_owned()));

        assert!(args.contains(&OsString::from("-r")));
        assert!(args.contains(&OsString::from("abc123")));
        assert_eq!(args.last(), Some(&OsString::from("--")));
    }

    #[test]
    fn parses_commit_log_rows() {
        let commits = parse_commit_log("abc\tdef\tme@example.com\tadd commit sidebar\n");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].change_id, "abc");
        assert_eq!(commits[0].commit_id, "def");
        assert_eq!(commits[0].author, "me@example.com");
        assert_eq!(commits[0].description, "add commit sidebar");
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
}

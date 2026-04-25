use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};

mod diff_view;

use anyhow::{Context, Result, bail};
use clap::Parser;
use diff_view::{DiffHunkView, DiffLine, DiffLineKind, DiffView, Palette, SyntaxKind, SyntaxSpan};
use iced::{
    Background, Border, Color, Element, Length, Shadow, Subscription, Task, Theme, keyboard,
    widget::{button, column, container, row, scrollable, text, text::Wrapping},
};
use tokio::process::Command;
use tree_sitter::{Parser as SyntaxParser, Query, QueryCursor, StreamingIterator};

const CODE_FONT: iced::Font = iced::Font::new("DejaVu Sans Mono");
const CODE_TEXT_SIZE: f32 = 14.0;
const CAPTION_TEXT_SIZE: f32 = 12.0;

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
    target: PathBuf,
    repository: Option<Repository>,
    status: LoadStatus,
    document: DiffDocument,
    commits: Vec<CommitSummary>,
    selected_revision: RevisionSelection,
    selected_theme: BuiltinTheme,
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
    fn label(&self) -> &str {
        match self {
            Self::WorkingCopy => "working copy",
            Self::Commit(_) => "selected commit",
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
    BackendLoaded(Result<BackendOutput, String>),
    SelectFile(usize),
    SelectRevision(RevisionSelection),
    SelectTheme(BuiltinTheme),
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
enum BuiltinTheme {
    Dark,
    Light,
    HighContrast,
}

impl BuiltinTheme {
    const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::HighContrast];

    fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::HighContrast => "contrast",
        }
    }

    fn spec(self) -> ThemeSpec {
        match self {
            Self::Dark => ThemeSpec {
                background: Color::from_rgb(0.020, 0.026, 0.040),
                panel_background: Color::from_rgb(0.050, 0.060, 0.083),
                panel_background_elevated: Color::from_rgb(0.072, 0.085, 0.118),
                selected_file: Color::from_rgb(0.085, 0.145, 0.215),
                text: Color::from_rgb(0.940, 0.965, 1.000),
                muted_text: Color::from_rgb(0.700, 0.760, 0.840),
                subtle_text: Color::from_rgb(0.520, 0.585, 0.680),
                accent: Color::from_rgb(0.170, 0.760, 1.000),
                added_line: Color::from_rgb(0.025, 0.205, 0.125),
                removed_line: Color::from_rgb(0.285, 0.055, 0.095),
                added_text: Color::from_rgb(0.440, 1.000, 0.650),
                removed_text: Color::from_rgb(1.000, 0.465, 0.535),
                modified_token: Color::from_rgb(1.000, 0.805, 0.275),
                file_header: Color::from_rgb(0.075, 0.092, 0.130),
                hunk_header: Color::from_rgb(0.055, 0.190, 0.295),
                conflict_marker: Color::from_rgb(1.000, 0.235, 0.315),
                border: Color::from_rgb(0.240, 0.295, 0.385),
                note_background: Color::from_rgb(0.245, 0.160, 0.035),
                note_text: Color::from_rgb(1.000, 0.890, 0.520),
            },
            Self::Light => ThemeSpec {
                background: Color::from_rgb(0.948, 0.944, 0.928),
                panel_background: Color::from_rgb(0.995, 0.992, 0.975),
                panel_background_elevated: Color::from_rgb(0.965, 0.960, 0.935),
                selected_file: Color::from_rgb(0.875, 0.918, 0.945),
                text: Color::from_rgb(0.110, 0.130, 0.160),
                muted_text: Color::from_rgb(0.380, 0.430, 0.485),
                subtle_text: Color::from_rgb(0.555, 0.585, 0.620),
                accent: Color::from_rgb(0.015, 0.415, 0.605),
                added_line: Color::from_rgb(0.865, 0.950, 0.895),
                removed_line: Color::from_rgb(0.985, 0.875, 0.875),
                added_text: Color::from_rgb(0.070, 0.405, 0.205),
                removed_text: Color::from_rgb(0.675, 0.115, 0.135),
                modified_token: Color::from_rgb(0.690, 0.410, 0.020),
                file_header: Color::from_rgb(0.930, 0.928, 0.900),
                hunk_header: Color::from_rgb(0.820, 0.895, 0.935),
                conflict_marker: Color::from_rgb(0.760, 0.090, 0.100),
                border: Color::from_rgb(0.765, 0.775, 0.790),
                note_background: Color::from_rgb(0.980, 0.930, 0.780),
                note_text: Color::from_rgb(0.490, 0.310, 0.050),
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
            Self::Dark => Theme::TokyoNight,
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
                let task = Task::perform(
                    load_backend(repository.clone(), RevisionSelection::WorkingCopy),
                    Message::BackendLoaded,
                );

                (
                    Self {
                        target: cli.path,
                        repository: Some(repository),
                        status: LoadStatus::Loading,
                        document: loading_document(),
                        commits: Vec::new(),
                        selected_revision: RevisionSelection::WorkingCopy,
                        selected_theme: BuiltinTheme::Dark,
                        selected_file: 0,
                    },
                    task,
                )
            }
            Err(error) => (
                Self {
                    target: cli.path,
                    repository: None,
                    status: LoadStatus::Failed(format!("{error:#}")),
                    document: DiffDocument::default(),
                    commits: Vec::new(),
                    selected_revision: RevisionSelection::WorkingCopy,
                    selected_theme: BuiltinTheme::Dark,
                    selected_file: 0,
                },
                Task::none(),
            ),
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::BackendLoaded(Ok(output)) => {
                self.status = LoadStatus::Loaded(output.summary);
                self.document = output.document;
                self.commits = output.commits;
                self.selected_file = self
                    .selected_file
                    .min(self.document.files.len().saturating_sub(1));
            }
            Message::BackendLoaded(Err(error)) => {
                self.status = LoadStatus::Failed(error);
            }
            Message::SelectFile(index) => {
                if index < self.document.files.len() {
                    self.selected_file = index;
                }
            }
            Message::SelectRevision(selection) => {
                if self.selected_revision != selection {
                    self.selected_revision = selection;
                    self.status = LoadStatus::Loading;
                    self.selected_file = 0;

                    if let Some(repository) = self.repository.clone() {
                        let revision = self.selected_revision.clone();
                        return Task::perform(
                            load_backend(repository, revision),
                            Message::BackendLoaded,
                        );
                    }
                }
            }
            Message::SelectTheme(theme) => {
                self.selected_theme = theme;
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
        let theme = self.selected_theme.spec();
        let status = match &self.status {
            LoadStatus::Loading => "loading backend output...".to_owned(),
            LoadStatus::Loaded(summary) => summary.clone(),
            LoadStatus::Failed(error) => format!("failed: {error}"),
        };

        let repo = match &self.repository {
            Some(repository) => format!(
                "{} repo: {} | scope: {}",
                repository.vcs.label(),
                repository.root.display(),
                display_scope(repository)
            ),
            None => "no repository detected".to_owned(),
        };

        let header = build_header(
            &self.target,
            status,
            repo,
            self.document.files.len(),
            self.document.total_additions,
            self.document.total_deletions,
            theme,
        );

        let content = row![build_sidebar(self, theme), build_diff_panel(self, theme)]
            .spacing(20)
            .height(Length::Fill);

        container(column![header, content].spacing(20))
            .padding(20)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(move |_| app_shell_style(theme))
            .into()
    }

    fn theme(&self) -> Theme {
        self.selected_theme.iced_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        keyboard::listen().filter_map(|event| match event {
            keyboard::Event::KeyPressed { key, .. } => match key.as_ref() {
                keyboard::Key::Named(keyboard::key::Named::ArrowDown)
                | keyboard::Key::Character("j") => Some(Message::SelectNextFile),
                keyboard::Key::Named(keyboard::key::Named::ArrowUp)
                | keyboard::Key::Character("k") => Some(Message::SelectPreviousFile),
                _ => None,
            },
            _ => None,
        })
    }
}

impl Vcs {
    fn label(self) -> &'static str {
        match self {
            Self::Jj => "jj",
            Self::Git => "git",
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
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
            Self::Renamed => "renamed",
        }
    }

    fn badge_color(self, theme: ThemeSpec) -> Color {
        match self {
            Self::Added => theme.added_line,
            Self::Deleted => theme.removed_line,
            Self::Modified => theme.selected_file,
            Self::Renamed => theme.note_background,
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
            "loaded {} {} diff: {} file(s), {} additions, {} deletions",
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
                    OsString::from("ancestors(@, 24)"),
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

fn build_header<'a>(
    target: &Path,
    status: String,
    repo: String,
    file_count: usize,
    additions: usize,
    deletions: usize,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let file_count = file_count.to_string();
    let additions = format!("+{additions}");
    let deletions = format!("-{deletions}");

    let metrics = row![
        build_metric_card("files", file_count, theme.accent, theme),
        build_metric_card("additions", additions, theme.added_text, theme),
        build_metric_card("deletions", deletions, theme.removed_text, theme),
    ]
    .spacing(12);

    container(
        row![
            column![
                text("diffui").size(34).color(theme.text),
                text(format!("target: {}", target.display()))
                    .size(15)
                    .color(theme.muted_text),
                text(status).size(14).color(theme.text),
                text(repo).size(CAPTION_TEXT_SIZE).color(theme.subtle_text),
            ]
            .spacing(6)
            .width(Length::Fill),
            metrics,
        ]
        .spacing(20),
    )
    .padding(20)
    .style(move |_| hero_panel_style(theme))
    .into()
}

fn build_metric_card<'a>(
    label: &'a str,
    value: String,
    value_color: Color,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    container(
        column![
            text(label).size(CAPTION_TEXT_SIZE).color(theme.subtle_text),
            text(value).size(22).color(value_color),
        ]
        .spacing(4),
    )
    .padding([12, 14])
    .style(move |_| secondary_panel_style(theme))
    .into()
}

fn build_sidebar(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let repo_label = ui
        .repository
        .as_ref()
        .map(|repository| format!("{} / {}", repository.vcs.label(), display_scope(repository)))
        .unwrap_or_else(|| "outside repository".to_owned());

    let mut files = column![
        text("theme").size(13).color(theme.subtle_text),
        build_theme_switcher(ui.selected_theme, theme),
        text("commits").size(18).color(theme.text),
        build_revision_button(
            "working copy",
            "uncommitted diff",
            RevisionSelection::WorkingCopy,
            &ui.selected_revision,
            theme
        ),
    ]
    .spacing(10);

    for commit in &ui.commits {
        files = files.push(build_commit_button(commit, &ui.selected_revision, theme));
    }

    files = files.push(text("changed files").size(18).color(theme.text));
    files = files.push(
        text(repo_label)
            .size(CAPTION_TEXT_SIZE)
            .color(theme.subtle_text),
    );

    for (index, file) in ui.document.files.iter().enumerate() {
        let selected = index == ui.selected_file;
        let subtitle = match &file.old_path {
            Some(old_path) if old_path != &file.path => format!("from {old_path}"),
            _ => format!("{} hunk(s)", file.hunks.len()),
        };

        files = files.push(
            button(
                column![
                    row![
                        text(file.path.as_str())
                            .size(14)
                            .color(theme.text)
                            .width(Length::Fill),
                        build_badge(file.status.label(), file.status.badge_color(theme), theme),
                    ]
                    .spacing(10),
                    row![
                        text(subtitle)
                            .size(CAPTION_TEXT_SIZE)
                            .color(theme.muted_text),
                        text(format!("+{}", file.additions))
                            .size(CAPTION_TEXT_SIZE)
                            .color(theme.added_text),
                        text(format!("-{}", file.deletions))
                            .size(CAPTION_TEXT_SIZE)
                            .color(theme.removed_text),
                    ]
                    .spacing(10),
                ]
                .spacing(8),
            )
            .width(Length::Fill)
            .padding([12, 14])
            .style(move |_, status| sidebar_button_style(status, selected, theme))
            .on_press(Message::SelectFile(index)),
        );
    }

    container(
        scrollable(files.spacing(12))
            .style(move |iced_theme, status| diff_scrollable_style(iced_theme, status, theme))
            .height(Length::Fill),
    )
    .width(Length::Fixed(320.0))
    .height(Length::Fill)
    .padding(16)
    .style(move |_| panel_style(theme.panel_background, theme))
    .into()
}

fn build_theme_switcher(
    selected_theme: BuiltinTheme,
    theme: ThemeSpec,
) -> Element<'static, Message> {
    let mut controls = row![].spacing(8);

    for candidate in BuiltinTheme::ALL {
        let selected = candidate == selected_theme;
        controls = controls.push(
            button(text(candidate.label()).size(CAPTION_TEXT_SIZE))
                .padding([7, 10])
                .style(move |_, status| sidebar_button_style(status, selected, theme))
                .on_press(Message::SelectTheme(candidate)),
        );
    }

    controls.into()
}

fn build_commit_button<'a>(
    commit: &'a CommitSummary,
    selected_revision: &RevisionSelection,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let title = format!("{} {}", commit.change_id, commit.description);
    let subtitle = format!("{} | {}", commit.commit_id, commit.author);

    build_revision_button(
        title,
        subtitle,
        RevisionSelection::Commit(commit.change_id.clone()),
        selected_revision,
        theme,
    )
}

fn build_revision_button<'a>(
    title: impl Into<String>,
    subtitle: impl Into<String>,
    revision: RevisionSelection,
    selected_revision: &RevisionSelection,
    theme: ThemeSpec,
) -> Element<'a, Message> {
    let selected = &revision == selected_revision;

    button(
        column![
            text(title.into())
                .size(13)
                .color(theme.text)
                .width(Length::Fill),
            text(subtitle.into())
                .size(CAPTION_TEXT_SIZE)
                .color(theme.muted_text)
                .width(Length::Fill),
        ]
        .spacing(4),
    )
    .width(Length::Fill)
    .padding([10, 12])
    .style(move |_, status| sidebar_button_style(status, selected, theme))
    .on_press(Message::SelectRevision(revision))
    .into()
}

fn build_diff_panel(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(file) = ui.document.files.get(ui.selected_file) else {
        return container(text("no diff loaded").size(16).color(theme.muted_text))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(move |_| panel_style(theme.panel_background, theme))
            .into();
    };

    let title = match &file.old_path {
        Some(old_path) if old_path != &file.path => format!("{old_path} -> {}", file.path),
        _ => file.path.clone(),
    };

    let mut content = column![
        container(
            column![
                row![
                    text(title).size(24).color(theme.text).width(Length::Fill),
                    build_badge(file.status.label(), file.status.badge_color(theme), theme),
                ]
                .spacing(12),
                row![
                    text(format!("{} hunk(s)", file.hunks.len()))
                        .size(CAPTION_TEXT_SIZE)
                        .color(theme.muted_text),
                    text(format!("+{} additions", file.additions))
                        .size(CAPTION_TEXT_SIZE)
                        .color(theme.added_text),
                    text(format!("-{} deletions", file.deletions))
                        .size(CAPTION_TEXT_SIZE)
                        .color(theme.removed_text),
                    text(format!("mode: {}", ui.selected_revision.label()))
                        .size(CAPTION_TEXT_SIZE)
                        .color(theme.subtle_text),
                ]
                .spacing(12),
            ]
            .spacing(10),
        )
        .padding(18)
        .style(move |_| secondary_panel_style(theme)),
    ]
    .spacing(16);

    if !file.metadata.is_empty() {
        let mut metadata = column![];

        for line in &file.metadata {
            metadata = metadata.push(
                text(line.as_str())
                    .font(CODE_FONT)
                    .size(CAPTION_TEXT_SIZE)
                    .wrapping(Wrapping::None)
                    .color(theme.muted_text),
            );
        }

        content = content.push(
            container(metadata.spacing(6))
                .padding(14)
                .style(move |_| panel_style(theme.panel_background_elevated, theme)),
        );
    }

    if file.hunks.is_empty() {
        content = content.push(
            container(
                text("no hunks for this file")
                    .size(15)
                    .color(theme.muted_text),
            )
            .padding(18)
            .style(move |_| panel_style(theme.panel_background_elevated, theme)),
        );
    } else {
        content = content.push(
            container(render_diff(file, ui.selected_file, theme))
                .width(Length::Fill)
                .height(Length::Fill)
                .clip(true),
        );
    }

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(16)
        .clip(true)
        .style(move |_| panel_style(theme.panel_background, theme))
        .into()
}

fn render_diff<'a>(file: &'a DiffFile, file_key: usize, theme: ThemeSpec) -> Element<'a, Message> {
    DiffView::new(
        &file.hunks,
        file_key,
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

fn hero_panel_style(theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(theme.file_header)
        .border(panel_border(theme))
}

fn panel_style(background: Color, theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(background)
        .border(panel_border(theme))
}

fn secondary_panel_style(theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(theme.selected_file)
        .border(panel_border(theme))
}

fn badge_style(background: Color, theme: ThemeSpec) -> container::Style {
    container::Style::default()
        .background(background)
        .border(Border {
            width: 1.0,
            color: theme.border,
            ..Border::default()
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
        border: if selected {
            Border {
                width: 1.0,
                color: theme.accent,
                ..Border::default()
            }
        } else {
            panel_border(theme)
        },
        shadow: Shadow::default(),
        snap: true,
    };

    match status {
        button::Status::Hovered => {
            style.border = Border {
                width: 1.0,
                color: if selected {
                    theme.accent
                } else {
                    theme.subtle_text
                },
                ..Border::default()
            };
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

fn diff_scrollable_style(
    iced_theme: &Theme,
    status: scrollable::Status,
    theme: ThemeSpec,
) -> scrollable::Style {
    let mut style = scrollable::default(iced_theme, status);
    style.container = container::Style::default();
    style.vertical_rail.background = Some(Background::Color(theme.panel_background));
    style.horizontal_rail.background = Some(Background::Color(theme.panel_background));
    style.vertical_rail.scroller.background = Background::Color(theme.subtle_text);
    style.horizontal_rail.scroller.background = Background::Color(theme.subtle_text);
    style
}

fn panel_border(theme: ThemeSpec) -> Border {
    Border {
        width: 1.0,
        color: theme.border,
        ..Border::default()
    }
}

fn loading_document() -> DiffDocument {
    DiffDocument {
        files: vec![DiffFile {
            path: "loading".to_owned(),
            old_path: None,
            status: DiffFileStatus::Modified,
            metadata: vec!["backend command is running".to_owned()],
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        }],
        total_additions: 0,
        total_deletions: 0,
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

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};

mod diff_view;

use anyhow::{Context, Result, bail};
use clap::Parser;
use diff_view::{DiffHunkView, DiffLine, DiffLineKind, DiffView, Palette};
use iced::{
    Background, Border, Color, Element, Length, Shadow, Subscription, Task, Theme, keyboard,
    widget::{button, column, container, row, scrollable, text, text::Wrapping},
};
use tokio::process::Command;

const CODE_FONT: iced::Font = iced::Font::new("DejaVu Sans Mono");
const CODE_TEXT_SIZE: f32 = 14.0;
const CAPTION_TEXT_SIZE: f32 = 12.0;
const PANEL_BORDER: Color = Color::from_rgb(0.16, 0.19, 0.25);
const PAGE_BACKGROUND: Color = Color::from_rgb(0.04, 0.06, 0.10);
const PANEL_BACKGROUND: Color = Color::from_rgb(0.08, 0.10, 0.15);
const PANEL_BACKGROUND_ELEVATED: Color = Color::from_rgb(0.10, 0.12, 0.18);
const PANEL_BACKGROUND_SELECTED: Color = Color::from_rgb(0.11, 0.15, 0.24);
const TEXT_PRIMARY: Color = Color::from_rgb(0.90, 0.93, 0.98);
const TEXT_MUTED: Color = Color::from_rgb(0.56, 0.63, 0.75);
const TEXT_SUBTLE: Color = Color::from_rgb(0.42, 0.48, 0.58);
const ACCENT: Color = Color::from_rgb(0.32, 0.63, 0.98);
const ADDITION_TEXT: Color = Color::from_rgb(0.80, 0.95, 0.86);
const DELETION_TEXT: Color = Color::from_rgb(0.98, 0.83, 0.86);
const ADDITION_BACKGROUND: Color = Color::from_rgb(0.07, 0.18, 0.13);
const DELETION_BACKGROUND: Color = Color::from_rgb(0.24, 0.10, 0.12);
const HUNK_HEADER_BACKGROUND: Color = Color::from_rgb(0.08, 0.15, 0.25);
const NOTE_BACKGROUND: Color = Color::from_rgb(0.18, 0.13, 0.06);
const NOTE_TEXT: Color = Color::from_rgb(0.97, 0.86, 0.68);

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
    SelectNextFile,
    SelectPreviousFile,
}

#[derive(Debug, Clone)]
struct BackendOutput {
    summary: String,
    document: DiffDocument,
}

#[derive(Debug, Clone)]
struct PendingHunk {
    header: String,
    rows: Vec<DiffLine>,
    next_old_line: usize,
    next_new_line: usize,
}

impl Diffui {
    fn new(cli: Cli) -> (Self, Task<Message>) {
        match prepare_repository(&cli.path) {
            Ok(repository) => {
                let task = Task::perform(load_backend(repository.clone()), Message::BackendLoaded);

                (
                    Self {
                        target: cli.path,
                        repository: Some(repository),
                        status: LoadStatus::Loading,
                        document: loading_document(),
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
        );

        let content = row![build_sidebar(self), build_diff_panel(self)]
            .spacing(20)
            .height(Length::Fill);

        container(column![header, content].spacing(20))
            .padding(20)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(|_| app_shell_style())
            .into()
    }

    fn theme(&self) -> Theme {
        Theme::TokyoNight
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

    fn badge_color(self) -> Color {
        match self {
            Self::Added => Color::from_rgb(0.12, 0.35, 0.22),
            Self::Deleted => Color::from_rgb(0.40, 0.13, 0.18),
            Self::Modified => Color::from_rgb(0.14, 0.22, 0.39),
            Self::Renamed => Color::from_rgb(0.29, 0.21, 0.10),
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

async fn load_backend(repository: Repository) -> Result<BackendOutput, String> {
    run_backend(repository)
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn run_backend(repository: Repository) -> Result<BackendOutput> {
    let (program, args) = backend_command(&repository);
    let output = run_command(&repository.root, program, args).await?;
    let document = parse_backend_output(&repository, &output);

    Ok(BackendOutput {
        summary: format!(
            "loaded {} diff: {} file(s), {} additions, {} deletions",
            repository.vcs.label(),
            document.files.len(),
            document.total_additions,
            document.total_deletions,
        ),
        document,
    })
}

fn backend_command(repository: &Repository) -> (&'static str, Vec<OsString>) {
    let mut args: Vec<OsString> = match repository.vcs {
        Vcs::Jj => ["diff", "--git", "--color", "never", "--"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        Vcs::Git => ["diff", "--"].into_iter().map(OsString::from).collect(),
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

    if let Some(file) = current_file.take() {
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
            });
            hunk.next_old_line += 1;
        }
        Some(' ') => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(hunk.next_old_line),
                new_line: Some(hunk.next_new_line),
                content: line[1..].to_owned(),
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
            });
        }
        _ => {
            hunk.rows.push(DiffLine {
                kind: DiffLineKind::Note,
                old_line: None,
                new_line: None,
                content: line.to_owned(),
            });
        }
    }
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
) -> Element<'a, Message> {
    let file_count = file_count.to_string();
    let additions = format!("+{additions}");
    let deletions = format!("-{deletions}");

    let metrics = row![
        build_metric_card("files", file_count, ACCENT),
        build_metric_card("additions", additions, ADDITION_TEXT),
        build_metric_card("deletions", deletions, DELETION_TEXT),
    ]
    .spacing(12);

    container(
        row![
            column![
                text("diffui").size(34).color(TEXT_PRIMARY),
                text(format!("target: {}", target.display()))
                    .size(15)
                    .color(TEXT_MUTED),
                text(status).size(14).color(TEXT_PRIMARY),
                text(repo).size(CAPTION_TEXT_SIZE).color(TEXT_SUBTLE),
            ]
            .spacing(6)
            .width(Length::Fill),
            metrics,
        ]
        .spacing(20),
    )
    .padding(20)
    .style(|_| hero_panel_style())
    .into()
}

fn build_metric_card<'a>(
    label: &'a str,
    value: String,
    value_color: Color,
) -> Element<'a, Message> {
    container(
        column![
            text(label).size(CAPTION_TEXT_SIZE).color(TEXT_SUBTLE),
            text(value).size(22).color(value_color),
        ]
        .spacing(4),
    )
    .padding([12, 14])
    .style(|_| secondary_panel_style())
    .into()
}

fn build_sidebar(ui: &Diffui) -> Element<'_, Message> {
    let repo_label = ui
        .repository
        .as_ref()
        .map(|repository| format!("{} / {}", repository.vcs.label(), display_scope(repository)))
        .unwrap_or_else(|| "outside repository".to_owned());

    let mut files = column![
        text("changed files").size(20).color(TEXT_PRIMARY),
        text(repo_label).size(CAPTION_TEXT_SIZE).color(TEXT_SUBTLE),
    ]
    .spacing(8);

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
                            .color(TEXT_PRIMARY)
                            .width(Length::Fill),
                        build_badge(file.status.label(), file.status.badge_color()),
                    ]
                    .spacing(10),
                    row![
                        text(subtitle).size(CAPTION_TEXT_SIZE).color(TEXT_MUTED),
                        text(format!("+{}", file.additions))
                            .size(CAPTION_TEXT_SIZE)
                            .color(ADDITION_TEXT),
                        text(format!("-{}", file.deletions))
                            .size(CAPTION_TEXT_SIZE)
                            .color(DELETION_TEXT),
                    ]
                    .spacing(10),
                ]
                .spacing(8),
            )
            .width(Length::Fill)
            .padding([12, 14])
            .style(move |_theme, status| sidebar_button_style(status, selected))
            .on_press(Message::SelectFile(index)),
        );
    }

    container(
        scrollable(files.spacing(12))
            .style(diff_scrollable_style)
            .height(Length::Fill),
    )
    .width(Length::Fixed(320.0))
    .height(Length::Fill)
    .padding(16)
    .style(|_| panel_style(PANEL_BACKGROUND))
    .into()
}

fn build_diff_panel(ui: &Diffui) -> Element<'_, Message> {
    let Some(file) = ui.document.files.get(ui.selected_file) else {
        return container(text("no diff loaded").size(16).color(TEXT_MUTED))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_| panel_style(PANEL_BACKGROUND))
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
                    text(title).size(24).color(TEXT_PRIMARY).width(Length::Fill),
                    build_badge(file.status.label(), file.status.badge_color()),
                ]
                .spacing(12),
                row![
                    text(format!("{} hunk(s)", file.hunks.len()))
                        .size(CAPTION_TEXT_SIZE)
                        .color(TEXT_MUTED),
                    text(format!("+{} additions", file.additions))
                        .size(CAPTION_TEXT_SIZE)
                        .color(ADDITION_TEXT),
                    text(format!("-{} deletions", file.deletions))
                        .size(CAPTION_TEXT_SIZE)
                        .color(DELETION_TEXT),
                ]
                .spacing(12),
            ]
            .spacing(10),
        )
        .padding(18)
        .style(|_| secondary_panel_style()),
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
                    .color(TEXT_MUTED),
            );
        }

        content = content.push(
            container(metadata.spacing(6))
                .padding(14)
                .style(|_| panel_style(PANEL_BACKGROUND_ELEVATED)),
        );
    }

    if file.hunks.is_empty() {
        content = content.push(
            container(text("no hunks for this file").size(15).color(TEXT_MUTED))
                .padding(18)
                .style(|_| panel_style(PANEL_BACKGROUND_ELEVATED)),
        );
    } else {
        content = content.push(
            container(render_diff(file, ui.selected_file))
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
        .style(|_| panel_style(PANEL_BACKGROUND))
        .into()
}

fn render_diff<'a>(file: &'a DiffFile, file_key: usize) -> Element<'a, Message> {
    DiffView::new(
        &file.hunks,
        file_key,
        diff_palette(),
        CODE_FONT,
        CODE_TEXT_SIZE,
    )
    .into()
}

fn build_badge<'a>(label: &'a str, background: Color) -> Element<'a, Message> {
    container(text(label).size(CAPTION_TEXT_SIZE).color(TEXT_PRIMARY))
        .padding([4, 10])
        .style(move |_| badge_style(background))
        .into()
}

fn diff_palette() -> Palette {
    Palette {
        text: TEXT_PRIMARY,
        text_muted: TEXT_SUBTLE,
        addition_text: ADDITION_TEXT,
        deletion_text: DELETION_TEXT,
        note_text: NOTE_TEXT,
        panel: PANEL_BACKGROUND_ELEVATED,
        hunk_header: HUNK_HEADER_BACKGROUND,
        addition_background: ADDITION_BACKGROUND,
        deletion_background: DELETION_BACKGROUND,
        note_background: NOTE_BACKGROUND,
        gutter_background: PANEL_BACKGROUND,
        border: PANEL_BORDER,
    }
}

fn app_shell_style() -> container::Style {
    container::Style::default()
        .background(PAGE_BACKGROUND)
        .color(TEXT_PRIMARY)
}

fn hero_panel_style() -> container::Style {
    container::Style::default()
        .background(PANEL_BACKGROUND_ELEVATED)
        .border(panel_border())
}

fn panel_style(background: Color) -> container::Style {
    container::Style::default()
        .background(background)
        .border(panel_border())
}

fn secondary_panel_style() -> container::Style {
    container::Style::default()
        .background(PANEL_BACKGROUND_SELECTED)
        .border(panel_border())
}

fn badge_style(background: Color) -> container::Style {
    container::Style::default()
        .background(background)
        .border(Border {
            width: 1.0,
            color: Color {
                a: 1.0,
                ..background
            },
            ..Border::default()
        })
}

fn sidebar_button_style(status: button::Status, selected: bool) -> button::Style {
    let background = if selected {
        PANEL_BACKGROUND_SELECTED
    } else {
        PANEL_BACKGROUND_ELEVATED
    };

    let mut style = button::Style {
        background: Some(Background::Color(background)),
        text_color: TEXT_PRIMARY,
        border: if selected {
            Border {
                width: 1.0,
                color: Color::from_rgb(0.30, 0.48, 0.72),
                ..Border::default()
            }
        } else {
            panel_border()
        },
        shadow: Shadow::default(),
        snap: true,
    };

    match status {
        button::Status::Hovered => {
            style.border = Border {
                width: 1.0,
                color: if selected { ACCENT } else { TEXT_SUBTLE },
                ..Border::default()
            };
        }
        button::Status::Pressed => {
            style.background = Some(Background::Color(PANEL_BACKGROUND_SELECTED));
        }
        button::Status::Disabled => {
            style.text_color = TEXT_SUBTLE;
        }
        button::Status::Active => {}
    }

    style
}

fn diff_scrollable_style(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    style.container = container::Style::default();
    style.vertical_rail.background = Some(Background::Color(Color::from_rgb(0.11, 0.13, 0.18)));
    style.horizontal_rail.background = Some(Background::Color(Color::from_rgb(0.11, 0.13, 0.18)));
    style.vertical_rail.scroller.background = Background::Color(Color::from_rgb(0.24, 0.32, 0.44));
    style.horizontal_rail.scroller.background =
        Background::Color(Color::from_rgb(0.24, 0.32, 0.44));
    style
}

fn panel_border() -> Border {
    Border {
        width: 1.0,
        color: PANEL_BORDER,
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

        let (program, args) = backend_command(&repository);

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

        let (_program, args) = backend_command(&repository);

        assert_eq!(args.last(), Some(&OsString::from("src")));
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
            "diff --git a/src/main.rs b/src/main.rs\nindex 123..456 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,2 +10,3 @@ fn demo()\n old\n-old value\n+new value\n+second line\n",
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

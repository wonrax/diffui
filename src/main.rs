use std::{path::PathBuf, time::Duration};

mod backend;
mod config;
mod diff_panel;
mod diff_view;
mod git;
mod graph;
mod graph_view;
mod jj;
mod repository;
mod resize_handle;
mod revision_list;
mod scrollbar;
mod sidebar;
mod theme;

use backend::{
    BackendOutput, CommitSummary, DiffDocument, RevisionDetails, RevisionSelection, load_backend,
    load_repository_snapshot,
};
use clap::Parser;
use config::AppConfig;
use iced::theme as iced_theme;
use iced::{
    Element, Length, Subscription, Task, Theme,
    event::{self, Event},
    keyboard, system, time,
    widget::{container, row, stack},
    window,
};
use repository::{Repository, RepositorySnapshot, prepare_repository};
use resize_handle::ResizeHandle;
use theme::{ResolvedTheme, ThemePreference, app_shell_style, vertical_divider};

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
pub(crate) struct Diffui {
    pub(crate) repository: Option<Repository>,
    pub(crate) status: LoadStatus,
    pub(crate) document: DiffDocument,
    pub(crate) commits: Vec<CommitSummary>,
    pub(crate) selected_revision: RevisionSelection,
    /// Sticky inline-file-list preference. Always reflects whether the
    /// *selected* revision's file list is shown; the user toggles it by
    /// re-clicking the selected row, and the value persists across
    /// revision switches so collapsing once stays collapsed for whatever
    /// revision the user moves to next.
    pub(crate) file_list_expanded: bool,
    pub(crate) pending_revision: Option<RevisionSelection>,
    pub(crate) repository_snapshot: Option<RepositorySnapshot>,
    pub(crate) snapshot_pending: bool,
    pub(crate) app_focused: bool,
    pub(crate) selected_theme: ThemePreference,
    pub(crate) system_theme: iced_theme::Mode,
    pub(crate) selected_file: usize,
    pub(crate) sidebar_width: f32,
    pub(crate) config: AppConfig,
    pub(crate) revision_details: Option<RevisionDetails>,
}

#[derive(Debug, Clone)]
pub(crate) enum LoadStatus {
    Loading,
    Loaded,
    Failed(String),
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    BackendLoaded(RevisionSelection, Box<Result<BackendOutput, String>>),
    RepositorySnapshotLoaded(Result<RepositorySnapshot, String>),
    SelectFile(usize),
    SelectRowKey(revision_list::RowSelectionKey),
    SelectTheme(ThemePreference),
    SystemThemeChanged(iced_theme::Mode),
    WindowFocusChanged(bool),
    RefreshRepository,
    SelectNextFile,
    SelectPreviousFile,
    CopyToClipboard(String),
    SidebarWidthChanged(f32),
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
                        file_list_expanded: true,
                        pending_revision: Some(RevisionSelection::WorkingCopy),
                        repository_snapshot: None,
                        snapshot_pending: false,
                        app_focused: true,
                        selected_theme: ThemePreference::System,
                        system_theme: iced_theme::Mode::None,
                        selected_file: 0,
                        sidebar_width: sidebar::DEFAULT_WIDTH.max(sidebar::min_width(config)),
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
                    file_list_expanded: true,
                    pending_revision: None,
                    repository_snapshot: None,
                    snapshot_pending: false,
                    app_focused: true,
                    selected_theme: ThemePreference::System,
                    system_theme: iced_theme::Mode::None,
                    selected_file: 0,
                    sidebar_width: sidebar::DEFAULT_WIDTH.max(sidebar::min_width(config)),
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

                    let revision_changed = self.selected_revision != revision;
                    self.selected_revision = revision;
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
                // Re-clicking the already-selected revision toggles its file
                // list without re-running the backend or changing the diff.
                // The toggled value persists across revision switches, so
                // collapsing once stays collapsed wherever the user moves
                // next.
                if self.selected_revision == selection {
                    self.file_list_expanded = !self.file_list_expanded;
                } else if self.pending_revision.as_ref() != Some(&selection)
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
                self.sidebar_width = width.max(sidebar::min_width(self.config));
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
            sidebar::build_sidebar(self, theme),
            vertical_divider(theme),
            diff_panel::build_diff_panel(self, theme),
        ]
        .spacing(0)
        .height(Length::Fill);
        let resize_overlay = ResizeHandle::new(
            self.sidebar_width,
            sidebar::min_width(self.config),
            sidebar::RESIZE_HIT_PADDING,
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

fn scroll_sidebar_to_file(_file_index: usize, _ui: &Diffui) -> Task<Message> {
    // TODO: re-implement scroll-to-reveal against `RevisionList`'s internal
    // scroll state once the widget exposes a scrollable operation.
    Task::none()
}

//! Where diffui sends a prompt and a diff: the AI provider.
//!
//! Providers are plugins with `[provides.ai]` (diffui bundles Claude Code,
//! Codex, Anthropic and an OpenAI-compatible one), plus config.toml's
//! `[ai] command`, the "Custom command" provider. `[ai] provider` (Settings →
//! AI, or config.toml) names the one "Generate description" and `agent`
//! command rows use, or leaves it automatic: the custom command when there
//! is one, else the first bundled provider that is set up, in
//! [`AUTO_ORDER`] (see [`pick`]).
//!
//! The when-clause key `ai` holds while a provider resolves and is set up;
//! the shipped AI rows name it, so without one they are left out of menus
//! and greyed in the palette with the reason.
//!
//! Settings → AI lists them with what each runs as, a Test that sends a tiny
//! prompt, and the pencil to the provider's own page.

use std::collections::{HashMap, HashSet};

use diffui_core::repo::CancelFlag;
use diffui_plugins::protocol::AiRequest;
use futures::StreamExt as _;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, MouseButton, MouseDownEvent, div, px};

use crate::commands::CommandArg;
use crate::geometry::Rectangle;
use crate::plugins::{AiProvider, Ended, PageAction, PluginAction, Streamed};
use crate::theme::{self, text_size};
use crate::ui::{self, Look};
use crate::{Action, Cx, Diffui, icons, menu, typography};

/// The bundled providers the automatic pick tries, in order.
pub(crate) const AUTO_ORDER: [&str; 4] = ["claude-code", "codex", "anthropic", "openai"];

/// `[ai] provider` for the automatic pick, and for `[ai] command`.
pub(crate) const AUTO: &str = "auto";
pub(crate) const CUSTOM: &str = "custom";

/// What the Test action sends.
const TEST_PROMPT: &str = "Reply with the single word OK.";

/// Where a prompt goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Provider {
    /// `[ai] command`: the prompt is added last.
    Custom(Vec<String>),
    Plugin {
        id: String,
        name: String,
    },
}

impl Provider {
    pub(crate) fn name(&self) -> &str {
        match self {
            Provider::Custom(_) => "Custom command",
            Provider::Plugin { name, .. } => name,
        }
    }
}

/// The provider `choice` (`auto`, `custom` or a plugin id) comes to.
///
/// `auto`: `[ai] command` when config.toml sets one; else the first of
/// [`AUTO_ORDER`] that is bundled (or replaces a bundled one), turned on and
/// set up: `claude-code` and `codex` when their program is found, `anthropic`
/// when a key is saved or `ANTHROPIC_API_KEY` is set, `openai` when it has a
/// model. A plugin from the folder is used only when named. A named provider
/// must be on and set up too; the error says what's missing.
pub(crate) fn pick(
    choice: &str,
    custom: Option<&[String]>,
    providers: &[AiProvider],
) -> Result<Provider, String> {
    let plugin = |provider: &AiProvider| Provider::Plugin {
        id: provider.id.clone(),
        name: provider.name.clone(),
    };
    match choice {
        AUTO => {
            if let Some(argv) = custom {
                return Ok(Provider::Custom(argv.to_vec()));
            }
            AUTO_ORDER
                .iter()
                .find_map(|id| {
                    providers.iter().find(|provider| {
                        provider.id == *id && provider.bundled && provider.usable && provider.ready
                    })
                })
                .map(plugin)
                .ok_or_else(|| {
                    "No AI provider: install claude or codex, or add an API key in Settings → AI"
                        .to_owned()
                })
        }
        CUSTOM => custom
            .map(|argv| Provider::Custom(argv.to_vec()))
            .ok_or_else(|| {
                "The custom command isn't set: add [ai] command to config.toml".to_owned()
            }),
        id => match providers.iter().find(|provider| provider.id == id) {
            Some(provider) if provider.usable && provider.ready => Ok(plugin(provider)),
            Some(provider) if provider.usable => Err(format!(
                "{} isn't set up: {}. See Settings → AI → {}",
                provider.name,
                provider.detail.to_lowercase(),
                provider.name
            )),
            Some(provider) => Err(format!(
                "{} can't run: {}. See Settings → Plugins",
                provider.name,
                provider.detail.to_lowercase()
            )),
            None => Err(format!(
                "There is no AI provider {id:?}: pick one in Settings → AI"
            )),
        },
    }
}

/// What Settings → AI keeps between frames.
#[derive(Debug, Default)]
pub(crate) struct AiState {
    /// Each Test's result, by provider (`custom` for the custom command).
    pub(crate) tests: HashMap<String, Test>,
    /// The secret store's accounts, read when Settings opens and after a
    /// secret changes: listing is cheap, but not for every frame.
    pub(crate) stored: HashSet<String>,
    /// The explain/review panel shows the reply as written, not rendered.
    pub(crate) panel_raw: bool,
    /// The pull request dialog shows its description rendered.
    pub(crate) pr_preview: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Test {
    Running,
    Replied(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AiAction {
    /// Use this provider: `auto`, `custom` or a plugin id.
    SetProvider(String),
    /// Send the provider a tiny prompt and show what came back.
    Test(String),
    /// The explain/review panel: the reply as written (`true`) or rendered.
    PanelRaw(bool),
    /// The pull request dialog's description: rendered (`true`) or the field.
    PrPreview(bool),
}

impl Diffui {
    /// `[ai] provider` and whether config.toml sets it (Settings can't).
    pub(crate) fn ai_choice(&self) -> (&str, bool) {
        match &self.user_config.ai_provider {
            Some(choice) => (choice, true),
            None => (
                self.config_files.ai_provider.as_deref().unwrap_or(AUTO),
                false,
            ),
        }
    }

    /// The provider a prompt goes to now, or why there is none.
    pub(crate) fn ai_provider(&self) -> Result<Provider, String> {
        let stored = self.plugins.secrets.accounts().unwrap_or_default();
        self.ai_provider_with(&stored.into_iter().collect())
    }

    /// [`Self::ai_provider`] with the store's accounts as last read: for
    /// what a frame shows.
    pub(crate) fn ai_provider_shown(&self) -> Result<Provider, String> {
        self.ai_provider_with(&self.ai.stored)
    }

    fn ai_provider_with(&self, stored: &HashSet<String>) -> Result<Provider, String> {
        pick(
            self.ai_choice().0,
            self.user_config.ai.as_deref(),
            &self.ai_providers(stored),
        )
    }

    /// Read the secret store's accounts again, for Settings → AI.
    pub(crate) fn refresh_ai_keys(&mut self) {
        self.ai.stored = self
            .plugins
            .secrets
            .accounts()
            .unwrap_or_default()
            .into_iter()
            .collect();
    }

    pub(crate) fn update_ai(&mut self, action: AiAction, cx: &mut Cx) {
        match action {
            AiAction::SetProvider(choice) => {
                if self.ai_choice().1 {
                    return self.push_error_toast(
                        crate::prose::Prose::from("The AI provider is set in ").path("config.toml"),
                        "Change it there",
                    );
                }
                let stored = (choice != AUTO).then_some(choice);
                if let Err(error) = self.config_files.write_ai_provider(stored) {
                    self.push_error_toast("Couldn't save settings", &error);
                }
            }
            AiAction::Test(key) => self.test_ai(key, cx),
            AiAction::PanelRaw(raw) => self.ai.panel_raw = raw,
            AiAction::PrPreview(preview) => self.ai.pr_preview = preview,
        }
    }

    /// What to tell the user when provider `id` failed with `kind`.
    pub(crate) fn ai_failure(&self, id: &str, kind: Option<&str>, message: &str) -> String {
        let providers = self.ai_providers(&self.ai.stored);
        let Some(provider) = providers.iter().find(|provider| provider.id == id) else {
            return message.to_owned();
        };
        let name = &provider.name;
        let what = match (kind, &provider.program) {
            (Some("missing-cli"), _) => {
                format!("Install it, or set where it is in Settings → AI → {name}.")
            }
            (Some("auth"), Some(program)) => {
                let program = program.rsplit('/').next().unwrap_or(program);
                format!("Sign in with {program} in a terminal, then try again.")
            }
            (Some("auth"), None) => format!("Add an API key in Settings → AI → {name}."),
            (Some("rate-limited"), _) => "Wait a moment, then try again.".to_owned(),
            (Some("unreachable"), None) => {
                format!("Check the connection, or the API address in Settings → AI → {name}.")
            }
            (Some("unreachable"), Some(_)) => "Check the connection.".to_owned(),
            _ => String::new(),
        };
        let message = message.trim();
        let message = if message.is_empty() || message.ends_with(['.', '!', '?']) {
            message.to_owned()
        } else {
            format!("{message}.")
        };
        [message, what]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn test_ai(&mut self, key: String, cx: &mut Cx) {
        if self.ai.tests.get(&key) == Some(&Test::Running) {
            return;
        }
        let work: futures::future::BoxFuture<'static, Result<String, String>> = if key == CUSTOM {
            let Some(argv) = self.user_config.ai.clone() else {
                return;
            };
            Box::pin(run_custom(argv))
        } else {
            let request = AiRequest {
                prompt: TEST_PROMPT.to_owned(),
                ..Default::default()
            };
            match self.ai_complete(&key, request, CancelFlag::default()) {
                Ok(stream) => Box::pin(collect(stream)),
                Err(error) => {
                    self.ai.tests.insert(key, Test::Failed(error));
                    return;
                }
            }
        };
        self.ai.tests.insert(key.clone(), Test::Running);
        Self::run_async(work, cx, move |this, result, _| {
            let test = match result {
                Ok(reply) => Test::Replied(reply),
                Err(failure) => Test::Failed(match failure.split_once('\u{0}') {
                    Some((kind, message)) => this.ai_failure(&key, Some(kind), message),
                    None => failure,
                }),
            };
            this.ai.tests.insert(key, test);
        });
    }

    /// The pop-up of providers, under the button at `anchor`.
    fn open_ai_menu(&mut self, anchor: Rectangle, cx: &mut Cx) {
        let current = self.ai_choice().0.to_owned();
        let item = |label: String, choice: &str| menu::MenuEntry::Item {
            label,
            detail: None,
            emphasized: current == choice,
            command: "settings.ai-provider".into(),
            arg: CommandArg::Text(choice.to_owned()),
            icon: None,
        };
        let mut tree = vec![
            item("Automatic".to_owned(), AUTO),
            menu::MenuEntry::Separator,
        ];
        if self.user_config.ai.is_some() {
            tree.push(item("Custom command".to_owned(), CUSTOM));
        }
        for provider in self.ai_providers(&self.ai.stored) {
            tree.push(item(provider.name.clone(), &provider.id));
        }
        #[cfg(target_os = "macos")]
        {
            let _ = anchor;
            Self::popup_tree(&tree, None, cx);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cx;
            self.push_mode(crate::modes::Mode::Menu(menu::OverlayMenu::new(
                tree,
                menu::AnchorSpec::Below(anchor),
                Some(MouseButton::Left),
            )));
        }
    }
}

/// A completion's text, or `kind\0message` when the provider failed.
async fn collect(
    mut stream: futures::channel::mpsc::UnboundedReceiver<Streamed>,
) -> Result<String, String> {
    let mut text = String::new();
    while let Some(streamed) = stream.next().await {
        match streamed {
            Streamed::Text(piece) => text.push_str(&piece),
            Streamed::Ended(Ended::Done) => return Ok(text.trim().to_owned()),
            Streamed::Ended(Ended::Cancelled) => return Err("Stopped".to_owned()),
            Streamed::Ended(Ended::Failed { kind, message }) => {
                return Err(match kind {
                    Some(kind) => format!("{kind}\u{0}{message}"),
                    None => message,
                });
            }
        }
    }
    Err("The provider stopped answering".to_owned())
}

/// `[ai] command` with the test prompt, its stdout.
async fn run_custom(argv: Vec<String>) -> Result<String, String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("[ai] command is empty".to_owned());
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .arg(TEST_PROMPT)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let run = tokio::time::timeout(std::time::Duration::from_secs(300), command.output());
    match run.await {
        Err(_) => Err(format!("{program} didn't answer in five minutes")),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("{program} isn't installed"))
        }
        Ok(Err(error)) => Err(error.to_string()),
        Ok(Ok(output)) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map_or_else(
                    || format!("{program} failed"),
                    |line| line.trim().to_owned(),
                ))
        }
    }
}

const ROW_HEIGHT: f32 = crate::settings::HINT_ROW_HEIGHT;
const ROW_INSET: f32 = crate::settings::ROW_INSET;
/// A pop-up button's label is cut with an ellipsis past this.
const SELECT_TEXT_MAX: f32 = 200.0;

fn send(
    action: Action,
    cx: &mut Cx,
) -> impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static {
    Diffui::on_action(action, cx)
}

/// Where an AI button sits: its id, the command it runs and on what, its
/// label, and the size of the buttons beside it.
pub(crate) struct AiButton<'a> {
    pub(crate) element: &'static str,
    pub(crate) command: &'a str,
    pub(crate) arg: CommandArg,
    pub(crate) label: &'static str,
    /// It is writing: the button is Stop.
    pub(crate) generating: bool,
    /// Held off for a reason of the caller's (the editor is saving).
    pub(crate) held: bool,
    pub(crate) size: ui::Size,
}

/// The button that runs an AI command from a field's dialog or editor,
/// sparkles before its label; "Stop" while it writes. Its tip says where the
/// diff goes, or, greyed, why it can't run and where to fix that. Nothing
/// when the command isn't there (a user removed it).
pub(crate) fn ai_button(
    ui: &Diffui,
    look: Look,
    spec: AiButton,
    cx: &mut Cx,
) -> Option<AnyElement> {
    let Look { theme, .. } = look;
    let command = ui.registry.get(spec.command)?;
    let (label, action, tip) = if spec.generating {
        ("Stop", Some(Action::StopGenerating), None)
    } else {
        match command.enabled(ui, &spec.arg) {
            Ok(()) => (
                spec.label,
                command.build(ui, spec.arg),
                ui.agent_destination(spec.command),
            ),
            Err(why) => (spec.label, None, Some(why.into_owned())),
        }
    };
    let action = action.filter(|_| !spec.held);
    let mut button = look
        .button(spec.element, label)
        .step(spec.size)
        .disabled(action.is_none());
    if !spec.generating {
        button = button.icon(icons::SPARKLES, theme.muted_text);
    }
    if let Some(tip) = tip {
        button = button.tip(tip);
    }
    Some(match action {
        Some(action) => button.on_click(send(action, cx)).into_any_element(),
        None => button.into_any_element(),
    })
}

/// Two views of one text, as a segmented control: `labels[1]` is on when
/// `second` is, and a press sends `pick(second)`.
pub(crate) fn view_toggle(
    look: Look,
    id: &'static str,
    labels: [&'static str; 2],
    second: bool,
    pick: impl Fn(bool) -> Action,
    cx: &mut Cx,
) -> AnyElement {
    let Look { theme, .. } = look;
    ui::segmented(theme)
        .children(labels.into_iter().enumerate().map(|(index, label)| {
            let this = index == 1;
            ui::segment(
                (id, index),
                ui::Segment {
                    icon: None,
                    label: label.into(),
                    count: None,
                    active: second == this,
                    enabled: true,
                },
                ui::Segmented::Compact,
                look,
            )
            .on_click(send(pick(this), cx))
        }))
        .into_any_element()
}

/// A surface's last parse: the text's hash, and its blocks.
type Parsed = (u64, std::rc::Rc<Vec<crate::markdown::Block>>);

thread_local! {
    /// The last parse of each surface's text, so a frame that shows the same
    /// text doesn't parse it again.
    static PARSED: std::cell::RefCell<HashMap<&'static str, Parsed>> = Default::default();
}

/// `text` rendered as Markdown for `surface` (an id of its own), its links
/// opening in the browser. Parsed again only when the text changed.
pub(crate) fn rendered_markdown(surface: &'static str, text: &str, look: Look) -> gpui_kit::Div {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    let key = hasher.finish();
    let blocks = PARSED.with(|parsed| {
        let mut parsed = parsed.borrow_mut();
        match parsed.get(surface) {
            Some((known, blocks)) if *known == key => blocks.clone(),
            _ => {
                let blocks = std::rc::Rc::new(crate::markdown::parse(text));
                parsed.insert(surface, (key, blocks.clone()));
                blocks
            }
        }
    });
    let Look { theme, font, mono } = look;
    crate::markdown::render(
        surface,
        &blocks,
        crate::markdown::MarkdownLook {
            theme,
            font,
            mono,
            size: crate::markdown::BODY_SIZE,
        },
        std::rc::Rc::new(
            |url: &str, _: &mut gpui_kit::Window, _: &mut gpui_kit::App| {
                if url.starts_with("http://") || url.starts_with("https://") {
                    crate::open_url(url);
                }
            },
        ),
    )
}

/// What a Test said, as a caption: the reply in green, a failure in red.
pub(crate) fn test_caption(look: Look, test: &Test) -> AnyElement {
    let Look { theme, font, .. } = look;
    let (text, ink) = match test {
        Test::Running => ("Testing\u{2026}".to_owned(), theme.muted_text),
        Test::Replied(reply) => (
            format!("Replied: {}", reply.lines().next().unwrap_or("")),
            theme.added_text,
        ),
        Test::Failed(why) => (why.clone(), theme.removed_text),
    };
    typography::line(text, font, text_size::CAPTION, ink)
        .overflow_hidden()
        .text_ellipsis()
        .into_any_element()
}

/// Settings' AI group: the provider in use, then a row per provider. Each
/// row is a hinted row's height.
pub(crate) fn settings_group(ui: &Diffui, look: Look, cx: &mut Cx) -> Vec<AnyElement> {
    let Look {
        theme, font, mono, ..
    } = look;
    let providers = ui.ai_providers(&ui.ai.stored);
    let (choice, locked) = ui.ai_choice();
    let custom = ui.user_config.ai.as_deref();
    let in_use = pick(choice, custom, &providers);
    let face_text = match (choice, &in_use) {
        (AUTO, Ok(provider)) => format!("Automatic \u{b7} {}", provider.name()),
        (AUTO, Err(_)) => "Automatic \u{b7} none found".to_owned(),
        (CUSTOM, _) => "Custom command".to_owned(),
        (id, _) => providers
            .iter()
            .find(|provider| provider.id == id)
            .map_or_else(|| id.to_owned(), |provider| provider.name.clone()),
    };

    let (anchor, tracker) = menu::anchor_tracker();
    let open = Diffui::listen(cx, move |this, _: &MouseDownEvent, _, cx| {
        this.open_ai_menu(anchor.get(), cx);
        cx.stop_propagation();
    });
    let ink = if locked {
        theme.subtle_text
    } else {
        theme.text
    };
    let face = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::SM))
        .child(
            typography::line(face_text, font, text_size::BODY, ink)
                .max_w(px(SELECT_TEXT_MAX))
                .overflow_hidden()
                .text_ellipsis(),
        )
        .child(icons::icon(
            icons::CHEVRON_DOWN,
            theme::icon::SM,
            theme.muted_text,
        ));
    let button = look
        .bare_button("settings-ai-provider")
        .raised()
        .disabled(locked)
        .child(face);
    let picker = div()
        .relative()
        .child(tracker)
        .when(!locked, |wrap| wrap.on_mouse_down(MouseButton::Left, open))
        .child(button);
    let mut rows = vec![crate::settings::hinted_row(
        look,
        "Provider",
        Some("Generate description and agent commands send the diff and their prompt here."),
        locked,
        picker,
    )];

    let in_use_key = match &in_use {
        Ok(crate::ai::Provider::Custom(_)) => Some(CUSTOM.to_owned()),
        Ok(crate::ai::Provider::Plugin { id, .. }) => Some(id.clone()),
        Err(_) => None,
    };
    let mut entries: Vec<(String, String, String, bool, bool)> = Vec::new();
    if let Some(argv) = custom {
        entries.push((
            CUSTOM.to_owned(),
            "Custom command".to_owned(),
            argv.join(" "),
            true,
            false,
        ));
    }
    for provider in &providers {
        entries.push((
            provider.id.clone(),
            provider.name.clone(),
            provider.detail.clone(),
            provider.usable,
            true,
        ));
    }
    for (index, (key, name, detail, usable, has_page)) in entries.into_iter().enumerate() {
        let mut title = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .gap(px(theme::space::MD))
            .child(typography::line(name, font, text_size::BODY, theme.text).text_ellipsis());
        if in_use_key.as_deref() == Some(key.as_str()) {
            title = title.child(
                div()
                    .flex_none()
                    .child(ui::tag("In use", theme.added_text, ui::TagSize::Small, font).view()),
            );
        }
        let caption = match ui.ai.tests.get(&key) {
            Some(test) => test_caption(look, test),
            None => typography::line(
                detail,
                if key == CUSTOM { mono } else { font },
                text_size::CAPTION,
                theme.muted_text,
            )
            .overflow_hidden()
            .text_ellipsis()
            .into_any_element(),
        };
        let label = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .gap(px(theme::space::XXS))
            .child(title)
            .child(caption);
        let testing = ui.ai.tests.get(&key) == Some(&Test::Running);
        let test = look
            .button(("settings-ai-test", index), "Test")
            .compact()
            .disabled(!usable || testing)
            .tip("Send a short prompt and show the reply")
            .on_click(send(Action::Ai(AiAction::Test(key.clone())), cx));
        let mut controls = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(theme::space::MD))
            .child(test);
        if has_page {
            let face = theme::radius::inner(theme::radius::CARD, theme::space::XXS);
            let side = theme::control::COMPACT - 2.0 * theme::space::XXS;
            controls = controls.child(
                look.icon_button(("settings-ai-page", index), icons::PENCIL, theme.muted_text)
                    .size(px(side))
                    .corner_radius(face)
                    .tip("Settings")
                    .on_click(send(
                        Action::Plugin(PluginAction::Page(PageAction::Open {
                            id: key.clone(),
                            host: None,
                        })),
                        cx,
                    )),
            );
        }
        rows.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .h(px(ROW_HEIGHT))
                .px(px(ROW_INSET))
                .gap(px(theme::space::XL))
                .child(label)
                .child(div().flex().flex_none().child(controls))
                .into_any_element(),
        );
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, usable: bool, ready: bool) -> AiProvider {
        AiProvider {
            id: id.to_owned(),
            name: id.to_uppercase(),
            bundled: true,
            usable,
            ready,
            program: None,
            detail: "Turned off in Plugins".to_owned(),
        }
    }

    fn picked(choice: &str, custom: Option<&[String]>, providers: &[AiProvider]) -> String {
        match pick(choice, custom, providers) {
            Ok(Provider::Custom(argv)) => format!("custom {}", argv.join(" ")),
            Ok(Provider::Plugin { id, .. }) => id,
            Err(why) => format!("error: {why}"),
        }
    }

    #[test]
    fn automatic_takes_the_custom_command_then_the_clis_then_the_apis() {
        let all = |ready: [bool; 4]| -> Vec<AiProvider> {
            // Listed in another order than the pick's, to show order is the pick's own.
            let mut providers: Vec<AiProvider> = AUTO_ORDER
                .iter()
                .zip(ready)
                .map(|(id, ready)| provider(id, true, ready))
                .collect();
            providers.reverse();
            providers
        };
        let llm = vec!["llm".to_owned(), "-m".to_owned(), "x".to_owned()];
        assert_eq!(picked(AUTO, Some(&llm), &all([true; 4])), "custom llm -m x");
        assert_eq!(picked(AUTO, None, &all([true; 4])), "claude-code");
        assert_eq!(picked(AUTO, None, &all([false, true, true, true])), "codex");
        assert_eq!(
            picked(AUTO, None, &all([false, false, true, true])),
            "anthropic"
        );
        assert_eq!(
            picked(AUTO, None, &all([false, false, false, true])),
            "openai"
        );
        assert!(picked(AUTO, None, &all([false; 4])).starts_with("error: No AI provider"));

        // One that is off, or a folder plugin, isn't picked by itself.
        let mut off = all([true; 4]);
        off.iter_mut()
            .find(|p| p.id == "claude-code")
            .unwrap()
            .usable = false;
        assert_eq!(picked(AUTO, None, &off), "codex");
        let mut folder = vec![provider("my-llm", true, true)];
        folder[0].bundled = false;
        assert!(picked(AUTO, None, &folder).starts_with("error"));
        assert_eq!(picked("my-llm", None, &folder), "my-llm");
    }

    #[test]
    fn a_named_provider_is_used_as_it_is_or_says_why_not() {
        let mut providers = [
            provider("anthropic", true, false),
            provider("codex", false, true),
        ];
        providers[0].detail = "No API key".to_owned();
        // Not set up is its own error, not a reason to pick another.
        assert_eq!(
            picked("anthropic", None, &providers),
            "error: ANTHROPIC isn't set up: no api key. See Settings → AI → ANTHROPIC"
        );
        providers[0].ready = true;
        assert_eq!(picked("anthropic", None, &providers), "anthropic");
        assert_eq!(
            picked("codex", None, &providers),
            "error: CODEX can't run: turned off in plugins. See Settings → Plugins"
        );
        assert!(picked("gone", None, &providers).contains("no AI provider \"gone\""));
        assert!(picked(CUSTOM, None, &providers).contains("[ai] command"));
        let llm = vec!["llm".to_owned()];
        assert_eq!(picked(CUSTOM, Some(&llm), &providers), "custom llm");
    }
}

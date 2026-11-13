//! The credential prompt: what git or ssh asks while a push or fetch runs
//! (a username, a password, a key's passphrase, whether to trust a new host),
//! shown as a dialog over the window. The relay that gets the question here
//! and the answer back is [`diffui_core::askpass`].
//!
//! Prompts that arrive while one is open queue behind it. A prompt whose
//! asker went away (git gave up, the op finished) is withdrawn without the
//! user doing anything.

use std::collections::VecDeque;

use iced::{
    Element, Length, Padding, Subscription, Task,
    widget::{Space, column, text_input},
};

use diffui_core::askpass::{self, AskpassEvent, PromptKind, PromptRequest};

use crate::modes::{Mode, ModeKind};
use crate::prose::Prose;
use crate::theme::{self, ThemeSpec, input_style, text_size};
use crate::{Action, Diffui, Message, ui};

const INPUT_ID: &str = "credential-input";

/// A user intent of the credential prompt — see [`Diffui::update_credential`].
#[derive(Debug, Clone)]
pub(crate) enum CredentialAction {
    Input(String),
    /// Send the answer: the field's text, or yes for a question.
    Submit,
    /// Decline. The operation that asked fails, and is reported as cancelled.
    Cancel,
}

/// The open prompt and the ones waiting behind it.
#[derive(Debug)]
pub(crate) struct CredentialDialog {
    current: PromptRequest,
    value: String,
    queue: VecDeque<PromptRequest>,
}

/// The relay's prompts, as messages. Empty when the relay isn't running.
pub(crate) fn subscription() -> Subscription<Message> {
    Subscription::run(|| {
        use futures::StreamExt;
        match askpass::take_events() {
            Some(events) => events.map(Message::Askpass).boxed(),
            None => futures::stream::pending().boxed(),
        }
    })
}

impl Diffui {
    pub(crate) fn on_askpass_event(&mut self, event: AskpassEvent) -> Task<Message> {
        match event {
            AskpassEvent::Prompt(request) => {
                if let Some(dialog) = self.credential_dialog_mut() {
                    dialog.queue.push_back(request);
                    return Task::none();
                }
                self.push_mode(Mode::Credential(CredentialDialog {
                    current: request,
                    value: String::new(),
                    queue: VecDeque::new(),
                }));
                Task::batch([
                    iced::widget::operation::focus(INPUT_ID),
                    self.raise_window(),
                ])
            }
            AskpassEvent::Withdrawn(id) => {
                let Some(dialog) = self.credential_dialog_mut() else {
                    return Task::none();
                };
                if dialog.current.id == id {
                    self.next_credential()
                } else {
                    dialog.queue.retain(|request| request.id != id);
                    Task::none()
                }
            }
        }
    }

    pub(crate) fn update_credential(&mut self, action: CredentialAction) -> Task<Message> {
        let Some(dialog) = self.credential_dialog_mut() else {
            return Task::none();
        };
        match action {
            CredentialAction::Input(value) => {
                dialog.value = value;
                Task::none()
            }
            CredentialAction::Submit => {
                let reply = match dialog.current.prompt.kind {
                    PromptKind::Secret | PromptKind::Text => std::mem::take(&mut dialog.value),
                    PromptKind::YesNo => "yes".to_owned(),
                    PromptKind::Confirm | PromptKind::Notice => String::new(),
                };
                askpass::answer(dialog.current.id, Some(reply));
                self.next_credential()
            }
            CredentialAction::Cancel => {
                // A notice has nothing to decline; dismissing it must not
                // fail the operation it is about.
                let reply = (dialog.current.prompt.kind == PromptKind::Notice).then(String::new);
                askpass::answer(dialog.current.id, reply);
                self.next_credential()
            }
        }
    }

    /// Show the next queued prompt, or close the dialog.
    fn next_credential(&mut self) -> Task<Message> {
        let Some(dialog) = self.credential_dialog_mut() else {
            return Task::none();
        };
        match dialog.queue.pop_front() {
            Some(next) => {
                dialog.current = next;
                dialog.value.clear();
                iced::widget::operation::focus(INPUT_ID)
            }
            None => {
                self.pop_mode(ModeKind::Credential);
                Task::none()
            }
        }
    }

    /// Cancel every prompt, for when the dialog can no longer be shown.
    pub(crate) fn cancel_credentials(&mut self) {
        if let Some(Mode::Credential(dialog)) = self.pop_mode(ModeKind::Credential) {
            for request in std::iter::once(dialog.current).chain(dialog.queue) {
                askpass::answer(request.id, None);
            }
        }
    }

    /// A fetch started before switching away is waiting on the user.
    fn raise_window(&self) -> Task<Message> {
        if self.app_focused {
            return Task::none();
        }
        iced::window::latest().and_then(|id| {
            iced::window::request_user_attention(
                id,
                Some(iced::window::UserAttention::Informational),
            )
        })
    }

    pub(crate) fn credential_dialog(&self) -> Option<&CredentialDialog> {
        match self.mode(ModeKind::Credential) {
            Some(Mode::Credential(dialog)) => Some(dialog),
            _ => None,
        }
    }

    fn credential_dialog_mut(&mut self) -> Option<&mut CredentialDialog> {
        match self.mode_mut(ModeKind::Credential) {
            Some(Mode::Credential(dialog)) => Some(dialog),
            _ => None,
        }
    }
}

/// Whether the open prompt is a yes/no question rather than a field.
pub(crate) fn is_question(ui: &Diffui) -> bool {
    ui.credential_dialog().is_some_and(|dialog| {
        matches!(
            dialog.current.prompt.kind,
            PromptKind::YesNo | PromptKind::Confirm
        )
    })
}

/// The dialog's title: the question, naming what it is about in mono.
fn title(request: &PromptRequest) -> Prose {
    let prompt = &request.prompt;
    let subject = |prose: Prose| match &prompt.subject {
        Some(subject) if subject.starts_with('/') || subject.starts_with('~') => {
            prose.path(subject.clone())
        }
        Some(subject) => prose.remote(subject.clone()),
        None => prose,
    };
    match prompt.kind {
        PromptKind::YesNo => subject(Prose::from("Trust host ")).text("?"),
        PromptKind::Secret | PromptKind::Text if prompt.subject.is_some() => {
            // `Passphrase for key` already says what it is for.
            let label = prompt.label();
            let joint = if label.contains(" for ") {
                " "
            } else {
                " for "
            };
            subject(Prose::from(format!("{label}{joint}")))
        }
        _ => Prose::from(prompt.label()),
    }
}

/// The modal card. Empty when no prompt is open.
pub(crate) fn build_overlay(ui: &Diffui, theme: ThemeSpec) -> Element<'_, Message> {
    let Some(dialog) = ui.credential_dialog() else {
        return Space::new().into();
    };
    let (font, mono) = (ui.config.ui_font, ui.config.mono_font);
    let send = |action| Message::Action(Action::Credential(action));
    let prompt = &dialog.current.prompt;

    let mut body = column![ui::dialog_title(&title(&dialog.current), font, mono, theme)]
        .spacing(ui::DIALOG_SPACING);

    let accept_label = match prompt.kind {
        PromptKind::Secret | PromptKind::Text => {
            // The title already says what goes here.
            let input = text_input("", &dialog.value)
                .id(INPUT_ID)
                .secure(prompt.kind == PromptKind::Secret)
                .padding(crate::typography::trimmed_padding(
                    Padding::from([theme::space::SM, theme::space::MD]),
                    mono,
                    text_size::BODY,
                ))
                .size(text_size::BODY)
                .font(mono)
                .on_input(move |value| send(CredentialAction::Input(value)))
                .on_submit(send(CredentialAction::Submit))
                .style(move |_, _| input_style(theme));
            body = body.push(crate::input::single_line(input, &dialog.value, mono));
            Some("Continue")
        }
        PromptKind::YesNo => {
            let mut text = Prose::from("First connection to this host. Check the fingerprint.");
            if let Some(detail) = &prompt.detail {
                text = text.text("\n").code(detail.clone());
            }
            body = body.push(ui::dialog_text(&text, font, mono, theme));
            Some("Yes")
        }
        PromptKind::Confirm => {
            body = body.push(ui::dialog_text(
                &Prose::from(prompt.text.clone()),
                font,
                mono,
                theme,
            ));
            Some("Yes")
        }
        PromptKind::Notice => {
            body = body.push(ui::dialog_text(
                &Prose::from(prompt.text.clone()),
                font,
                mono,
                theme,
            ));
            None
        }
    };

    let queued = dialog.queue.len();
    let leading = (queued > 0).then(|| {
        iced::widget::text(format!("{queued} more waiting"))
            .size(text_size::UI)
            .font(font)
            .color(theme.muted_text)
            .into()
    });
    let decline = match prompt.kind {
        PromptKind::YesNo | PromptKind::Confirm => ui::text_button(
            None,
            "No",
            ui::Variant::Raised,
            ui::Size::Standard,
            font,
            theme,
        ),
        PromptKind::Notice => ui::text_button(
            None,
            "Dismiss",
            ui::Variant::Raised,
            ui::Size::Standard,
            font,
            theme,
        ),
        PromptKind::Secret | PromptKind::Text => ui::cancel_button(ui::Size::Standard, font, theme),
    }
    .on_press(send(CredentialAction::Cancel));
    let mut buttons: Vec<Element<'_, Message>> = vec![decline.into()];
    if let Some(label) = accept_label {
        buttons.push(
            ui::text_button(
                None,
                label,
                ui::Variant::Primary,
                ui::Size::Standard,
                font,
                theme,
            )
            .on_press(send(CredentialAction::Submit))
            .into(),
        );
    }
    body = body.push(ui::button_row(leading, buttons));

    ui::modal(
        ui::dialog_card(body.width(Length::Fill), ui::DIALOG_WIDTH, theme),
        crate::toolbar::modal_top(),
        // A stray click on the scrim must not cancel a push.
        Message::Ui(crate::UiEvent::ConfirmNoOp),
        Message::Ui(crate::UiEvent::ConfirmNoOp),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use askpass::Prompt;

    fn request(text: &str) -> PromptRequest {
        PromptRequest {
            id: 1,
            prompt: Prompt::parse(text, ""),
        }
    }

    #[test]
    fn titles_name_the_subject() {
        assert_eq!(
            title(&request("Username for 'https://example.com': ")),
            Prose::from("Username for ").remote("https://example.com")
        );
        assert_eq!(
            title(&request("Enter passphrase for key '/k/id_ed25519': ")),
            Prose::from("Passphrase for key ").path("/k/id_ed25519")
        );
        assert_eq!(
            title(&request(
                "The authenticity of host 'h (1.2.3.4)' can't be established.\n\
                 Are you sure you want to continue connecting (yes/no)? "
            )),
            Prose::from("Trust host ").remote("h").text("?")
        );
    }
}

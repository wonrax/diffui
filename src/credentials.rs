//! The credential prompt: what git or ssh asks while a push or fetch runs
//! (a username, a password, a key's passphrase, whether to trust a new host),
//! shown as a dialog over the window. The relay that gets the question here
//! and the answer back is [`diffui_core::askpass`].
//!
//! Prompts that arrive while one is open queue behind it. A prompt whose
//! asker went away (git gave up, the op finished) is withdrawn without the
//! user doing anything.

use crate::Cx;
use gpui_kit::{AnyElement, Window};
use std::collections::VecDeque;

use diffui_core::askpass::{self, AskpassEvent, PromptKind, PromptRequest};

use crate::Diffui;
use crate::modes::{Mode, ModeKind};
use crate::prose::Prose;
use crate::theme::ThemeSpec;

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

/// Starts the loop that brings git's and ssh's credential prompts to the app.
pub(crate) fn start(cx: &mut Cx) {
    use futures::StreamExt as _;
    let Some(mut events) = askpass::take_events() else {
        return;
    };
    cx.spawn(async move |this, cx| {
        while let Some(event) = events.next().await {
            let updated = this.update(cx, |this, cx| {
                this.act(cx, |this, cx| this.on_askpass_event(event, cx));
            });
            if updated.is_err() {
                return;
            }
        }
    })
    .detach();
}

impl Diffui {
    pub(crate) fn on_askpass_event(&mut self, event: AskpassEvent, cx: &mut Cx) {
        match event {
            AskpassEvent::Prompt(request) => {
                if let Some(dialog) = self.credential_dialog_mut() {
                    dialog.queue.push_back(request);
                    return;
                }
                self.push_mode(Mode::Credential(CredentialDialog {
                    current: request,
                    value: String::new(),
                    queue: VecDeque::new(),
                }));
                crate::input::focus(INPUT_ID, cx);
                self.raise_window(cx);
            }
            AskpassEvent::Withdrawn(id) => {
                let Some(dialog) = self.credential_dialog_mut() else {
                    return;
                };
                if dialog.current.id == id {
                    self.next_credential(cx)
                } else {
                    dialog.queue.retain(|request| request.id != id);
                }
            }
        }
    }

    pub(crate) fn update_credential(&mut self, action: CredentialAction, cx: &mut Cx) {
        let Some(dialog) = self.credential_dialog_mut() else {
            return;
        };
        match action {
            CredentialAction::Input(value) => {
                dialog.value = value;
            }
            CredentialAction::Submit => {
                let reply = match dialog.current.prompt.kind {
                    PromptKind::Secret | PromptKind::Text => std::mem::take(&mut dialog.value),
                    PromptKind::YesNo => "yes".to_owned(),
                    PromptKind::Confirm | PromptKind::Notice => String::new(),
                };
                askpass::answer(dialog.current.id, Some(reply));
                self.next_credential(cx)
            }
            CredentialAction::Cancel => {
                // A notice has nothing to decline; dismissing it must not
                // fail the operation it is about.
                let reply = (dialog.current.prompt.kind == PromptKind::Notice).then(String::new);
                askpass::answer(dialog.current.id, reply);
                self.next_credential(cx)
            }
        }
    }

    /// Show the next queued prompt, or close the dialog.
    fn next_credential(&mut self, cx: &mut Cx) {
        let Some(dialog) = self.credential_dialog_mut() else {
            return;
        };
        match dialog.queue.pop_front() {
            Some(next) => {
                dialog.current = next;
                dialog.value.clear();
                crate::input::focus(INPUT_ID, cx)
            }
            None => {
                self.pop_mode(ModeKind::Credential);
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
    fn raise_window(&self, cx: &mut Cx) {
        if self.app_focused {
            return;
        }
        // GPUI has no bounce-the-dock call; activating is the closest it offers.
        self.with_window(cx, |window, _| window.activate_window());
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

/// Stub: the credentials view is the lane's to port (see the iced source).
#[allow(unused_variables)]
pub(crate) fn build_overlay(
    ui: &Diffui,
    theme: ThemeSpec,
    window: &mut Window,
    cx: &mut Cx,
) -> AnyElement {
    crate::stub::overlay(
        "credentials::build_overlay",
        ui.mode(ModeKind::Credential).is_some(),
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

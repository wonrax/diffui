//! The app's text fields: kit's inputs, driven like controlled components.
//!
//! kit's inputs own their text in an entity created against a `Window`, which
//! is awkward for update code that has neither and thinks of a field as a
//! `String` in app state. So the wrappers here keep that view: app state holds
//! the text, a panel passes it in each render along with what to do on an edit,
//! and when that value moves to something the input doesn't show (update code set
//! it: a restored revset, a picked preset, a chosen folder) the input is rewritten
//! to match. The entity lives
//! in the window's element state under the field's id, so it exists exactly
//! while the field is on screen, as the iced widget state did.
//!
//! Focus works the same way: update code asks with [`focus`] and no window, and
//! the field applies it the next time it renders.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::prelude::*;
use gpui_kit::{App, Context, Entity, Global, Subscription, Window, px};

use crate::theme::Font;

/// What a field does with an edit.
pub type OnInput = Rc<dyn Fn(String, &mut Window, &mut App)>;
/// What a field does when its text or caret moves: the text, and the caret's
/// byte offset.
pub type OnEdit = Rc<dyn Fn(String, usize, &mut Window, &mut App)>;
/// What a field does on Enter (⌘⏎ in a multi-line one).
pub type OnSubmit = Rc<dyn Fn(&mut Window, &mut App)>;

/// What to do to a field the next time it renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusRequest {
    /// Take focus, leaving the caret where it is.
    Focus,
    /// Take focus and select all the text.
    SelectAll,
    /// Put the caret at this byte offset.
    Caret(usize),
}

/// Focus asked for by update code, waiting for the field to render.
#[derive(Default)]
struct PendingFocus(Vec<(&'static str, FocusRequest)>);

impl Global for PendingFocus {}

/// Asks the field named `id` to take focus when it next renders. Call from
/// update code; the `act` that wraps it notifies, so the render happens.
pub fn focus(id: &'static str, cx: &mut App) {
    request(id, FocusRequest::Focus, cx);
}

/// Puts the caret of the field named `id` at byte `at` when it next renders.
pub fn set_caret(id: &'static str, at: usize, cx: &mut App) {
    request(id, FocusRequest::Caret(at), cx);
}

/// [`focus`], with the text selected.
pub fn focus_and_select_all(id: &'static str, cx: &mut App) {
    request(id, FocusRequest::SelectAll, cx);
}

fn request(id: &'static str, kind: FocusRequest, cx: &mut App) {
    cx.default_global::<PendingFocus>().0.push((id, kind));
}

fn take_requests(id: &'static str, cx: &mut App) -> Vec<FocusRequest> {
    if !cx.has_global::<PendingFocus>() {
        return Vec::new();
    }
    let pending = cx.global_mut::<PendingFocus>();
    let mut taken = Vec::new();
    pending.0.retain(|(other, request)| {
        let mine = *other == id;
        if mine {
            taken.push(*request);
        }
        !mine
    });
    taken
}

/// Whether a kit text input has the keyboard. Replaces iced's `consumed` flag:
/// such an input takes the plain keys it types with, so the keymap leaves those
/// alone. `context_stack` is the focused element's path; kit's inputs all put
/// `Input` on it.
pub fn input_has_focus(window: &Window) -> bool {
    window
        .context_stack()
        .iter()
        .any(|context| context.contains("Input"))
}

/// What varies between single-line and multi-line fields.
pub struct FieldSpec<'a> {
    /// Names the field: its element id, and what [`focus`] takes.
    pub id: &'static str,
    pub value: &'a str,
    pub placeholder: &'a str,
    /// Echo the text as dots (credential prompts).
    pub masked: bool,
    pub on_input: Option<OnInput>,
    pub on_edit: Option<OnEdit>,
    pub on_submit: Option<OnSubmit>,
}

impl<'a> FieldSpec<'a> {
    pub fn new(id: &'static str, value: &'a str) -> Self {
        Self {
            id,
            value,
            placeholder: "",
            masked: false,
            on_input: None,
            on_edit: None,
            on_submit: None,
        }
    }

    pub fn placeholder(mut self, placeholder: &'a str) -> Self {
        self.placeholder = placeholder;
        self
    }

    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    pub fn on_input(mut self, on_input: impl Fn(String, &mut Window, &mut App) + 'static) -> Self {
        self.on_input = Some(Rc::new(on_input));
        self
    }

    /// For a field that also cares where the caret is (the command line, the
    /// revset box): runs when the text or the caret moves, with both.
    pub fn on_edit(
        mut self,
        on_edit: impl Fn(String, usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_edit = Some(Rc::new(on_edit));
        self
    }

    pub fn on_submit(mut self, on_submit: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_submit = Some(Rc::new(on_submit));
        self
    }
}

/// The callbacks of the latest render, read by the one subscription the field
/// holds for its whole life.
#[derive(Default)]
struct Callbacks {
    on_input: Option<OnInput>,
    on_edit: Option<OnEdit>,
    on_submit: Option<OnSubmit>,
    placeholder: String,
    masked: bool,
    /// The text and caret last reported to `on_edit`.
    reported: (String, usize),
    /// The value last passed in. The input is only rewritten when this moves:
    /// an edit reaches app state a moment after the input has it, and a frame in
    /// between would otherwise wipe the keystroke.
    passed: Option<String>,
}

struct Controlled<S: 'static> {
    state: Entity<S>,
    callbacks: Rc<RefCell<Callbacks>>,
    _events: Subscription,
    _caret: Subscription,
}

/// Reports caret moves: the input notifies on them but raises no event.
fn observe_caret<S: 'static>(
    state: &Entity<S>,
    window: &mut Window,
    cx: &mut Context<Controlled<S>>,
    callbacks: Rc<RefCell<Callbacks>>,
    read: impl Fn(&S) -> (String, usize) + 'static,
) -> Subscription {
    cx.observe_in(state, window, move |_, state, window, cx| {
        let on_edit = callbacks.borrow().on_edit.clone();
        let Some(on_edit) = on_edit else {
            return;
        };
        let now = read(state.read(cx));
        if callbacks.borrow().reported == now {
            return;
        }
        callbacks.borrow_mut().reported = now.clone();
        on_edit(now.0, now.1, window, cx);
    })
}

fn subscribe<S>(
    state: &Entity<S>,
    window: &mut Window,
    cx: &mut Context<Controlled<S>>,
    callbacks: Rc<RefCell<Callbacks>>,
    value: impl Fn(&S) -> String + 'static,
) -> Subscription
where
    S: gpui_kit::EventEmitter<InputEvent> + 'static,
{
    cx.subscribe_in(state, window, move |_, state, event, window, cx| {
        let (on_input, on_submit) = {
            let callbacks = callbacks.borrow();
            (callbacks.on_input.clone(), callbacks.on_submit.clone())
        };
        match event {
            InputEvent::Change => {
                if let Some(on_input) = on_input {
                    let text = value(state.read(cx));
                    on_input(text, window, cx);
                }
            }
            InputEvent::PressEnter { .. } => {
                if let Some(on_submit) = on_submit {
                    on_submit(window, cx);
                }
            }
            InputEvent::Focus | InputEvent::Blur => {}
        }
    })
}

/// A single-line field. The chrome (height, border, leading icon) is the
/// caller's: `.appearance(false)` on the result leaves a bare input to set in a
/// well, as [`field`](crate) does in the app's filter fields.
pub fn single_line(spec: FieldSpec<'_>, font: Font, window: &mut Window, cx: &mut App) -> Input {
    let id = spec.id;
    let controlled = window.use_keyed_state(id, cx, |window, cx| {
        let state = cx.new(|cx| InputState::new(window, cx));
        let callbacks = Rc::new(RefCell::new(Callbacks::default()));
        let events = subscribe(
            &state,
            window,
            cx,
            callbacks.clone(),
            |state: &InputState| state.value().to_string(),
        );
        let caret = observe_caret(
            &state,
            window,
            cx,
            callbacks.clone(),
            |state: &InputState| (state.value().to_string(), state.cursor()),
        );
        Controlled {
            state,
            callbacks,
            _events: events,
            _caret: caret,
        }
    });
    let (state, callbacks) = {
        let controlled = controlled.read(cx);
        (controlled.state.clone(), controlled.callbacks.clone())
    };
    let (replace_placeholder, mask) = {
        let mut callbacks = callbacks.borrow_mut();
        callbacks.on_input = spec.on_input;
        callbacks.on_edit = spec.on_edit;
        callbacks.on_submit = spec.on_submit;
        let replace = callbacks.placeholder != spec.placeholder;
        let mask = callbacks.masked != spec.masked;
        if replace {
            callbacks.placeholder = spec.placeholder.to_owned();
        }
        callbacks.masked = spec.masked;
        (replace.then(|| spec.placeholder.to_owned()), mask)
    };
    let value = {
        let mut callbacks = callbacks.borrow_mut();
        let moved = callbacks.passed.as_deref() != Some(spec.value);
        if moved {
            callbacks.passed = Some(spec.value.to_owned());
        }
        moved.then(|| spec.value.to_owned())
    };
    state.update(cx, |state, cx| {
        if let Some(value) = value
            && state.value().as_ref() != value
        {
            state.set_value(value, window, cx);
        }
        if let Some(placeholder) = replace_placeholder {
            state.set_placeholder(placeholder, window, cx);
        }
        if mask {
            state.toggle_masked(window, cx);
        }
    });
    for request in take_requests(id, cx) {
        let state = state.clone();
        window.defer(cx, move |window, cx| {
            state.update(cx, |state, cx| match request {
                FocusRequest::Focus => state.focus(window, cx),
                FocusRequest::SelectAll => {
                    state.focus(window, cx);
                    state.select_all(window, cx);
                }
                FocusRequest::Caret(at) => state.set_selected_range(at..at, cx),
            });
        });
    }
    Input::new(&state)
        .id(id)
        .font(font.gpui())
        .text_size(px(crate::theme::text_size::UI))
}

/// A multi-line field that wraps its text. With no `on_input` it is read-only,
/// which is how the description editor behaves while it saves and how captured
/// output stays selectable.
pub fn multi_line(spec: FieldSpec<'_>, font: Font, window: &mut Window, cx: &mut App) -> Textarea {
    let id = spec.id;
    let controlled = window.use_keyed_state(id, cx, |window, cx| {
        let state = cx.new(|cx| TextareaState::new(window, cx));
        let callbacks = Rc::new(RefCell::new(Callbacks::default()));
        let events = subscribe(
            &state,
            window,
            cx,
            callbacks.clone(),
            |state: &TextareaState| state.value().to_string(),
        );
        let caret = observe_caret(
            &state,
            window,
            cx,
            callbacks.clone(),
            |state: &TextareaState| (state.value().to_string(), state.cursor()),
        );
        Controlled {
            state,
            callbacks,
            _events: events,
            _caret: caret,
        }
    });
    let (state, callbacks) = {
        let controlled = controlled.read(cx);
        (controlled.state.clone(), controlled.callbacks.clone())
    };
    let readonly = spec.on_input.is_none() && spec.on_edit.is_none();
    let replace_placeholder = {
        let mut callbacks = callbacks.borrow_mut();
        callbacks.on_input = spec.on_input;
        callbacks.on_edit = spec.on_edit;
        callbacks.on_submit = spec.on_submit;
        let replace = callbacks.placeholder != spec.placeholder;
        if replace {
            callbacks.placeholder = spec.placeholder.to_owned();
        }
        replace.then(|| spec.placeholder.to_owned())
    };
    let value = {
        let mut callbacks = callbacks.borrow_mut();
        let moved = callbacks.passed.as_deref() != Some(spec.value);
        if moved {
            callbacks.passed = Some(spec.value.to_owned());
        }
        moved.then(|| spec.value.to_owned())
    };
    state.update(cx, |state, cx| {
        if let Some(value) = value
            && state.value().as_ref() != value
        {
            state.set_value(value, window, cx);
        }
        if let Some(placeholder) = replace_placeholder {
            state.set_placeholder(placeholder, window, cx);
        }
    });
    for request in take_requests(id, cx) {
        let state = state.clone();
        window.defer(cx, move |window, cx| {
            state.update(cx, |state, cx| match request {
                FocusRequest::Focus => state.focus(window, cx),
                FocusRequest::SelectAll => {
                    state.focus(window, cx);
                    state.select_all(window, cx);
                }
                FocusRequest::Caret(at) => state.set_selected_range(at..at, cx),
            });
        });
    }
    Textarea::new(&state)
        .readonly(readonly)
        .font(font.gpui())
        .text_size(px(crate::theme::text_size::UI))
}

/// Inset of a [`multi_line`] editor's text from its border.
pub const EDITOR_PADDING: crate::theme::Padding = crate::theme::Padding {
    top: 10.0,
    right: 12.0,
    bottom: 10.0,
    left: 12.0,
};

/// The height that shows all of `content` wrapped at `width` (the editor's
/// outer width), between `min` and `max`; past `max` the editor scrolls.
pub fn fit_height(content: &str, font: Font, size: f32, width: f32, min: f32, max: f32) -> f32 {
    let line_height = (size * crate::measure::LINE_HEIGHT_MULTIPLIER).max(1.0);
    let text_width = (width - EDITOR_PADDING.left - EDITOR_PADDING.right).max(1.0);
    let lines = crate::measure::wrapped_line_count(content, size, font, line_height, text_width);
    let natural = (lines as f32 * line_height).ceil() + EDITOR_PADDING.top + EDITOR_PADDING.bottom;
    natural.clamp(min, max.max(min))
}

/// The tallest a [`multi_line`] editor grows before it scrolls: room for a
/// commit message's body, never most of the window.
pub fn editor_max_height(window_height: f32) -> f32 {
    (window_height * 0.45).clamp(160.0, 420.0)
}

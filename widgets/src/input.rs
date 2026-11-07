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
//! while the field is on screen.
//!
//! Focus works the same way: update code asks with [`focus`] and no window, and
//! the field applies it the next time it renders.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{
    AnyInputState, Editor, EditorState, GutterDecoration, GutterDecorationCollection, Input,
    InputEvent, InputState, TextDecoration, TextDecorationCollection, Textarea, TextareaState,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Context, Entity, Global, HighlightStyle, Pixels, Point, Subscription, Window, point, px,
};

use crate::keys::{Key, Modifiers, Named};
use crate::theme::{Color, Font};

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

/// Whether a kit text input has the keyboard:
/// such an input takes the plain keys it types with, so the keymap leaves those
/// alone. `context_stack` is the focused element's path; kit's inputs all put
/// `Input` on it.
pub fn input_has_focus(window: &Window) -> bool {
    window
        .context_stack()
        .iter()
        .any(|context| context.contains("Input"))
}

/// A left press outside the focused field takes the keyboard from it. GPUI leaves
/// focus alone on a press elsewhere and kit's buttons deliberately keep it where
/// it is, so without this the field goes on swallowing the keymap's plain keys.
pub fn blur_on_press_outside(position: Point<Pixels>, window: &mut Window, cx: &mut App) {
    let bounds = match window.focused_input(cx) {
        Some(AnyInputState::Input(state)) => state.read(cx).input_bounds(),
        Some(AnyInputState::Textarea(state)) => state.read(cx).input_bounds(),
        Some(AnyInputState::Editor(state)) => state.read(cx).input_bounds(),
        _ => return,
    };
    if !bounds.contains(&position) {
        window.blur(cx);
    }
}

/// Whether a focused text field keeps `key` for itself, which is what the
/// keymap's `consumed` means. Reading only "a field has focus" isn't enough:
/// a one-line field has no use for ↑/↓, so they drive whatever list it filters.
pub fn field_takes_key(window: &mut Window, key: &Key, modifiers: Modifiers, cx: &mut App) -> bool {
    if !input_has_focus(window) {
        return false;
    }
    let focused = window.focused_input(cx);
    if is_copy(key, modifiers) {
        // Nothing selected: nothing to copy, so the key is free.
        return focused.is_some_and(|field| has_selection(&field, cx));
    }
    let multi_line = matches!(focused, Some(AnyInputState::Textarea(_)));
    takes_key(key, modifiers, multi_line)
}

/// Whether this is the copy shortcut.
pub fn is_copy(key: &Key, modifiers: Modifiers) -> bool {
    modifiers.command() && matches!(key.as_ref(), Key::Character("c" | "C"))
}

fn has_selection(field: &AnyInputState, cx: &App) -> bool {
    let range = match field {
        AnyInputState::Input(state) => state.read(cx).selected_range(),
        AnyInputState::Textarea(state) => state.read(cx).selected_range(),
        AnyInputState::Editor(state) => state.read(cx).selected_range(),
        AnyInputState::Otp(_) => return false,
    };
    !range.is_empty()
}

/// The keys a field types, moves or edits with. The command line's editor counts
/// as one line here.
fn takes_key(key: &Key, modifiers: Modifiers, multi_line: bool) -> bool {
    match key.as_ref() {
        Key::Named(Named::Escape | Named::Tab) => false,
        Key::Named(Named::ArrowUp | Named::ArrowDown) => multi_line || modifiers.shift(),
        // ⌘⏎ submits an editor from the keymap.
        Key::Named(Named::Enter) => !(multi_line && modifiers.command()),
        Key::Named(
            Named::Space
            | Named::Backspace
            | Named::Delete
            | Named::ArrowLeft
            | Named::ArrowRight
            | Named::Home
            | Named::End
            | Named::PageUp
            | Named::PageDown,
        ) => true,
        Key::Named(_) | Key::Unidentified => false,
        Key::Character(typed) => {
            let latin = typed.chars().next().unwrap_or('\0').to_ascii_lowercase();
            if modifiers.command() {
                matches!(latin, 'x' | 'a' | 'z' | 'y') || (latin == 'v' && !modifiers.alt())
            } else if modifiers.control() {
                crate::keys::cocoa_text_key(latin, modifiers, multi_line).is_some()
            } else {
                true
            }
        }
    }
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

/// Coloured byte ranges of a text.
pub type Spans = Vec<(Range<usize>, Color)>;

/// How a [`styled_text`] field lays its text out and what it paints over it.
#[derive(Default)]
pub struct StyledLook {
    /// Long lines wrap instead of panning.
    pub wrap: bool,
    /// Enter raises `on_submit` instead of breaking the line.
    pub submit_on_enter: bool,
    /// Stays at the end as text arrives, unless scrolled away from it.
    pub follow: bool,
    /// Width of the strip left of the text that [`Self::marks`] draw in.
    pub mark_width: Option<f32>,
    /// Coloured byte ranges of the text.
    pub colors: Spans,
    /// Bars in the strip, beside these byte ranges.
    pub marks: Spans,
}

/// What a [`styled_text`] field last painted.
struct StyledState {
    colors: TextDecorationCollection,
    marks: Option<GutterDecorationCollection>,
    shown: (Spans, Spans),
    /// The offset following left the view at, once it has been laid out.
    followed: Option<f32>,
    /// A jump to the end is on its way to the next layout.
    jumping: bool,
}

/// A field whose words carry their own colours, on kit's editor (the only kit
/// input with decorations): the text, caret and selection share one layout, so
/// they can't drift apart. No `on_input` or `on_edit` makes it read-only, with
/// selection still working.
pub fn styled_text(
    spec: FieldSpec<'_>,
    look: StyledLook,
    font: Font,
    size: f32,
    window: &mut Window,
    cx: &mut App,
) -> Editor {
    let id = spec.id;
    let (wrap, submit) = (look.wrap, look.submit_on_enter);
    let controlled = window.use_keyed_state(id, cx, |window, cx| {
        let state = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(false)
                .folding(false)
                .indent_guides(false)
                .searchable(false)
                .auto_close(false)
                .smart_indent(false)
                .soft_wrap(wrap)
                .submit_on_enter(submit)
                .scroll_beyond_last_line(Some(0))
        });
        let callbacks = Rc::new(RefCell::new(Callbacks::default()));
        let events = subscribe(
            &state,
            window,
            cx,
            callbacks.clone(),
            |state: &EditorState| state.value().to_string(),
        );
        let caret = observe_caret(
            &state,
            window,
            cx,
            callbacks.clone(),
            |state: &EditorState| (state.value().to_string(), state.cursor()),
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
    let extras = window.use_keyed_state(
        gpui_kit::ElementId::Name(format!("{id}-styled").into()),
        cx,
        |_, cx| {
            let mark_width = look.mark_width;
            state.update(cx, |state, cx| {
                let mut marks = None;
                if let Some(width) = mark_width {
                    state.set_gutter_decorations_width(px(width), cx);
                    marks = Some(state.create_gutter_decorations_collection(Vec::new(), cx));
                }
                StyledState {
                    colors: state.create_decorations_collection(Vec::new(), cx),
                    marks,
                    shown: Default::default(),
                    followed: None,
                    jumping: false,
                }
            })
        },
    );
    let readonly = spec.on_input.is_none() && spec.on_edit.is_none();
    {
        let mut callbacks = callbacks.borrow_mut();
        callbacks.on_input = spec.on_input;
        callbacks.on_edit = spec.on_edit;
        callbacks.on_submit = spec.on_submit;
    }
    let value = {
        let mut callbacks = callbacks.borrow_mut();
        let moved = callbacks.passed.as_deref() != Some(spec.value);
        if moved {
            callbacks.passed = Some(spec.value.to_owned());
        }
        moved.then(|| spec.value.to_owned())
    };
    let follow = look.follow;
    let moved = value.is_some();
    state.update(cx, |state, cx| {
        if let Some(value) = value
            && state.value().as_ref() != value
        {
            state.set_value(value, window, cx);
            // The editor leaves the caret at the start. A request of the caller's,
            // below, still has the last word.
            let end = state.value().len();
            state.set_selected_range(end..end, cx);
        }
    });
    if follow {
        let at = state.read(cx).scroll_offset().y.into();
        extras.update(cx, |extras, _| {
            if std::mem::take(&mut extras.jumping) {
                extras.followed = Some(at);
            }
        });
        let stuck = extras
            .read(cx)
            .followed
            .is_none_or(|followed: f32| (followed - at).abs() < 1.0);
        if moved && stuck {
            extras.update(cx, |extras, _| extras.jumping = true);
            state.update(cx, |state, cx| {
                state.set_scroll_offset(point(px(0.0), px(-1e7)), cx);
            });
        }
    }
    let shown = (look.colors, look.marks);
    if extras.read(cx).shown != shown {
        let decorations = shown
            .0
            .iter()
            .map(|(range, color)| {
                TextDecoration::new(
                    range.clone(),
                    HighlightStyle {
                        color: Some((*color).into()),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let marks: Vec<GutterDecoration> = shown
            .1
            .iter()
            .map(|(range, color)| GutterDecoration::new(range.clone()).with_color((*color).into()))
            .collect();
        extras.update(cx, |extras, cx| {
            extras.colors.set(decorations, cx);
            if let Some(collection) = &extras.marks {
                collection.set(marks, cx);
            }
            extras.shown = shown;
        });
    }
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
    Editor::new(&state)
        .readonly(readonly)
        .appearance(false)
        .font(font.gpui())
        .text_size(px(size))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(text: &str) -> Key {
        Key::Character(text.to_owned())
    }

    #[test]
    fn a_one_line_field_leaves_what_it_has_no_use_for() {
        let none = Modifiers::empty();
        for key in [Named::ArrowUp, Named::ArrowDown, Named::Tab, Named::Escape] {
            assert!(!takes_key(&Key::Named(key), none, false), "{key:?}");
        }
        assert!(takes_key(
            &Key::Named(Named::ArrowUp),
            Modifiers::SHIFT,
            false
        ));
        assert!(!takes_key(&ch("n"), Modifiers::CTRL, false), "drives lists");
        assert!(!takes_key(&ch("k"), Modifiers::LOGO, false), "the palette");
    }

    #[test]
    fn a_one_line_field_keeps_what_it_edits_with() {
        let none = Modifiers::empty();
        assert!(takes_key(&ch("j"), none, false));
        assert!(takes_key(&Key::Named(Named::Enter), none, false));
        assert!(takes_key(&Key::Named(Named::ArrowLeft), none, false));
        assert!(takes_key(&ch("z"), Modifiers::LOGO, false));
        if cfg!(target_os = "macos") {
            assert!(takes_key(&ch("a"), Modifiers::CTRL, false));
        }
    }

    #[test]
    fn a_multi_line_field_keeps_its_lines() {
        let none = Modifiers::empty();
        assert!(takes_key(&Key::Named(Named::ArrowDown), none, true));
        assert!(takes_key(&Key::Named(Named::Enter), none, true));
        assert!(!takes_key(&Key::Named(Named::Enter), Modifiers::LOGO, true));
        assert!(!takes_key(&Key::Named(Named::Escape), none, true));
    }
}

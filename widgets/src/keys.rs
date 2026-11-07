//! Keys, as the keymap and the command line think of them.
//!
//! The keymap resolves named keys and characters under modifiers, and a chord
//! has to mean the same whatever the windowing layer calls it. GPUI hands us a
//! [`Keystroke`] of strings; [`Key::from_keystroke`] reads it into this form
//! once, at the edge, so nothing past the root listener parses key names.

use gpui_kit::Keystroke;

/// A logical key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key<C = String> {
    Named(Named),
    Character(C),
    Unidentified,
}

impl Key {
    /// The borrowed form, for matching on string patterns.
    pub fn as_ref(&self) -> Key<&str> {
        match self {
            Self::Named(named) => Key::Named(*named),
            Self::Character(c) => Key::Character(c.as_str()),
            Self::Unidentified => Key::Unidentified,
        }
    }

    /// What a [`Keystroke`] names. The character is what the key types with
    /// ⇧ applied (`?`, `A`), unless ⌘ or ⌃ is held, where it is the bare key.
    pub fn from_keystroke(keystroke: &Keystroke) -> Self {
        let key = keystroke.key.as_str();
        let named = match key {
            "enter" => Named::Enter,
            "escape" => Named::Escape,
            "tab" => Named::Tab,
            "space" => Named::Space,
            "backspace" => Named::Backspace,
            "delete" => Named::Delete,
            "up" => Named::ArrowUp,
            "down" => Named::ArrowDown,
            "left" => Named::ArrowLeft,
            "right" => Named::ArrowRight,
            "home" => Named::Home,
            "end" => Named::End,
            "pageup" => Named::PageUp,
            "pagedown" => Named::PageDown,
            "insert" => Named::Insert,
            "f1" => Named::F1,
            "f2" => Named::F2,
            "f3" => Named::F3,
            "f4" => Named::F4,
            "f5" => Named::F5,
            "f6" => Named::F6,
            "f7" => Named::F7,
            "f8" => Named::F8,
            "f9" => Named::F9,
            "f10" => Named::F10,
            "f11" => Named::F11,
            "f12" => Named::F12,
            "" => return Self::Unidentified,
            _ => {
                let mods = &keystroke.modifiers;
                if !mods.platform && !mods.control {
                    if let Some(typed) = &keystroke.key_char {
                        return Self::Character(typed.clone());
                    }
                }
                if mods.shift && key.len() == 1 {
                    return Self::Character(key.to_uppercase());
                }
                return Self::Character(key.to_owned());
            }
        };
        Self::Named(named)
    }
}

/// A key with a name rather than a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Named {
    Enter,
    Escape,
    Tab,
    Space,
    Backspace,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

/// Which modifiers are held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const SHIFT: Self = Self(1);
    pub const CTRL: Self = Self(2);
    pub const ALT: Self = Self(4);
    pub const LOGO: Self = Self(8);
    /// The platform's command key: ⌘ on macOS, ⌃ elsewhere.
    pub const COMMAND: Self = if cfg!(target_os = "macos") {
        Self::LOGO
    } else {
        Self::CTRL
    };

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    const fn has(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn shift(self) -> bool {
        self.has(Self::SHIFT)
    }

    pub const fn control(self) -> bool {
        self.has(Self::CTRL)
    }

    pub const fn alt(self) -> bool {
        self.has(Self::ALT)
    }

    pub const fn logo(self) -> bool {
        self.has(Self::LOGO)
    }

    /// Just the ⇧ of these.
    pub const fn only_shift(self) -> Self {
        Self(self.0 & Self::SHIFT.0)
    }

    pub const fn command(self) -> bool {
        self.has(Self::COMMAND)
    }
}

impl From<gpui_kit::Modifiers> for Modifiers {
    fn from(held: gpui_kit::Modifiers) -> Self {
        let mut out = Self::empty();
        for (on, bit) in [
            (held.shift, Self::SHIFT),
            (held.control, Self::CTRL),
            (held.alt, Self::ALT),
            (held.platform, Self::LOGO),
        ] {
            if on {
                out |= bit;
            }
        }
        out
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// The named key, and modifiers, a macOS text field reads a ⌃ chord as: ⌃A/⌃E
/// line start/end, ⌃B/⌃F back/forward a character, ⌃N/⌃P down/up a line,
/// ⌃H/⌃D delete back/forward, ⌃K delete to the end of the line. ⇧ extends a
/// motion's selection. A one-line field (`multi_line` false) has no line to
/// move to and leaves ⌃N/⌃P to whatever list it drives. `None` for any other
/// press. `latin` is the letter the key carries without modifiers.
pub fn cocoa_text_key(
    latin: char,
    modifiers: Modifiers,
    multi_line: bool,
) -> Option<(Named, Modifiers)> {
    if !modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    let motion = |named| Some((named, modifiers.only_shift()));
    match latin.to_ascii_lowercase() {
        'a' => motion(Named::Home),
        'e' => motion(Named::End),
        'b' => motion(Named::ArrowLeft),
        'f' => motion(Named::ArrowRight),
        'n' if multi_line => motion(Named::ArrowDown),
        'p' if multi_line => motion(Named::ArrowUp),
        'h' => Some((Named::Backspace, Modifiers::empty())),
        'd' => Some((Named::Delete, Modifiers::empty())),
        // ⌘⌦ is iced's "delete to the end of the line".
        'k' => Some((Named::Delete, Modifiers::LOGO)),
        _ => None,
    }
}

/// On macOS, a press of one of the [`cocoa_text_key`] chords as the named
/// key it stands for; `None` for any other press, and anywhere else (⌃ is the
/// command key there).
pub fn cocoa_key(key: &Key, modifiers: Modifiers, multi_line: bool) -> Option<(Key, Modifiers)> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let Key::Character(latin) = key else {
        return None;
    };
    let latin = latin.chars().next()?.to_ascii_lowercase();
    let (named, modifiers) = cocoa_text_key(latin, modifiers, multi_line)?;
    Some((Key::Named(named), modifiers))
}

/// On macOS, ⌃N/⌃P as the ↓/↑ they stand for in a list a one-line field
/// drives (a completion dropdown), the way Spotlight and Raycast read them.
pub fn cocoa_list_key(key: &Key, modifiers: Modifiers) -> Option<Key> {
    if !cfg!(target_os = "macos") || modifiers != Modifiers::CTRL {
        return None;
    }
    let named = match key.as_ref() {
        Key::Character("n") => Named::ArrowDown,
        Key::Character("p") => Named::ArrowUp,
        _ => return None,
    };
    Some(Key::Named(named))
}

#[cfg(test)]
mod tests {
    use super::Named::*;
    use super::*;

    #[test]
    fn cocoa_chords_name_their_keys() {
        let ctrl = Modifiers::CTRL;
        let shift = Modifiers::CTRL | Modifiers::SHIFT;
        let none = Modifiers::empty();
        for (latin, multi_line, modifiers, expected) in [
            ('a', false, ctrl, Some((Home, none))),
            ('e', false, ctrl, Some((End, none))),
            ('b', false, ctrl, Some((ArrowLeft, none))),
            ('f', false, ctrl, Some((ArrowRight, none))),
            ('a', false, shift, Some((Home, Modifiers::SHIFT))),
            ('A', false, shift, Some((Home, Modifiers::SHIFT))),
            ('h', false, ctrl, Some((Backspace, none))),
            ('d', false, shift, Some((Delete, none))),
            ('k', false, ctrl, Some((Delete, Modifiers::LOGO))),
            ('n', true, ctrl, Some((ArrowDown, none))),
            ('p', true, shift, Some((ArrowUp, Modifiers::SHIFT))),
            // A one-line field leaves ⌃N/⌃P to its list.
            ('n', false, ctrl, None),
            ('p', false, ctrl, None),
            // Not a text-system chord.
            ('x', true, ctrl, None),
            ('a', true, none, None),
            ('a', true, Modifiers::LOGO, None),
            ('a', true, Modifiers::CTRL | Modifiers::LOGO, None),
            ('a', true, Modifiers::CTRL | Modifiers::ALT, None),
        ] {
            assert_eq!(
                cocoa_text_key(latin, modifiers, multi_line),
                expected,
                "{latin} {modifiers:?} multi_line={multi_line}"
            );
        }
    }

    #[test]
    fn keystrokes_read_as_keys() {
        let key =
            |text: &'static str| Key::from_keystroke(&Keystroke::parse(text).expect("keystroke"));
        assert_eq!(key("enter"), Key::Named(Enter));
        assert_eq!(key("cmd-up"), Key::Named(ArrowUp));
        assert_eq!(key("a"), Key::Character("a".to_owned()));
        assert_eq!(key("cmd-a"), Key::Character("a".to_owned()));
    }
}

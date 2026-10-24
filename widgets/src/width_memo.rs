//! A bounded memo over [`crate::measure::line_width`].
//!
//! Measuring a string builds a fresh cosmic-text buffer and shapes it. The
//! sidebar asks for eight to fifteen of those per visible row — the change-id
//! prefix and suffix, the commit id, the author, the ellipsis, every bookmark
//! chip — and it asks again on every frame, for strings that have not changed
//! since the row was built. Shaping is by far the most expensive thing a row
//! does, and none of it is new work.
//!
//! The answer depends on exactly `(content, size, font)`, so that is the key.
//! The memo lives in the widget's own state, which means it is dropped with the
//! widget and never shared across two that shape differently.

use std::cell::RefCell;
use std::collections::HashMap;

use iced::Font;

use crate::measure;

/// Entries kept before the memo is cleared. The working set is one screenful of
/// rows; the cap is there so a long scroll through a million commits cannot
/// grow it without bound. Clearing wholesale rather than evicting one at a time
/// costs a single frame of re-shaping and keeps the lookup a plain hash.
const CAPACITY: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    content: String,
    /// `f32` is neither `Eq` nor `Hash`; its bit pattern is both. Sizes here
    /// come from the type scale and the config, so equal sizes are bit-equal.
    size: u32,
    font: Font,
}

/// Shaped widths seen so far. Interior mutability because `draw` gets the
/// widget tree by shared reference — measuring is a read as far as the widget
/// is concerned, and the memo is an implementation detail of that read.
#[derive(Debug, Default)]
pub struct WidthMemo {
    widths: RefCell<HashMap<Key, f32>>,
}

impl WidthMemo {
    /// The rendered width of `content`, shaping it only the first time.
    pub fn width(&self, content: &str, size: f32, font: Font) -> f32 {
        if content.is_empty() {
            return 0.0;
        }
        let key = Key {
            content: content.to_owned(),
            size: size.to_bits(),
            font,
        };
        if let Some(width) = self.widths.borrow().get(&key) {
            return *width;
        }
        let width = measure::line_width(content, size, font);
        let mut widths = self.widths.borrow_mut();
        if widths.len() >= CAPACITY {
            widths.clear();
        }
        widths.insert(key, width);
        width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The memo has to answer exactly what a direct measure would, or every
    /// rail it lays out drifts. Sizes and fonts are part of the key, so two
    /// renderings of the same string never share an answer.
    #[test]
    fn a_memoized_width_matches_the_measurement_it_replaces() {
        let memo = WidthMemo::default();
        let font = Font::MONOSPACE;
        for (content, size) in [("zqxlrpwv", 13.0f32), ("zqxlrpwv", 11.0), ("+12", 13.0)] {
            let direct = measure::line_width(content, size, font);
            assert_eq!(memo.width(content, size, font), direct);
            // …and again, from the memo this time.
            assert_eq!(memo.width(content, size, font), direct);
        }
        assert_eq!(memo.width("", 13.0, font), 0.0);
    }

    #[test]
    fn the_memo_stays_bounded() {
        let memo = WidthMemo::default();
        for index in 0..CAPACITY + 10 {
            memo.width(&format!("row {index}"), 13.0, Font::MONOSPACE);
        }
        assert!(memo.widths.borrow().len() <= CAPACITY);
    }
}

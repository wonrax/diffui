//! Sentences that name things. A dialog, toast or banner that mentions a
//! revision, bookmark, path or operation builds a [`Prose`] instead of a
//! `String`, so every surface renders those names the same way the graph
//! does: monospaced, with change ids in the accent and bookmarks in the lane
//! colour.

use std::fmt;

use iced::widget::text::{Rich, Span};
use iced::{Color, Font};

use crate::theme::ThemeSpec;
use crate::typography::Aligned;

/// What a run of text names, which decides how it renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    /// A change or commit id.
    Revision,
    Bookmark,
    Path,
    Operation,
    Remote,
    /// Anything else quoted as code, e.g. a backticked name in jj's output.
    Code,
}

/// Text with the names in it marked, built left to right:
/// `Prose::text("Move ").bookmark("main").text(" backwards?")`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Prose {
    parts: Vec<(Kind, String)>,
}

impl Prose {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn push(mut self, kind: Kind, text: impl Into<String>) -> Self {
        let text = text.into();
        if text.is_empty() {
            return self;
        }
        match self.parts.last_mut() {
            Some((last, run)) if *last == kind && kind == Kind::Text => run.push_str(&text),
            _ => self.parts.push((kind, text)),
        }
        self
    }

    pub(crate) fn text(self, text: impl Into<String>) -> Self {
        self.push(Kind::Text, text)
    }

    pub(crate) fn rev(self, id: impl Into<String>) -> Self {
        self.push(Kind::Revision, id)
    }

    pub(crate) fn bookmark(self, name: impl Into<String>) -> Self {
        self.push(Kind::Bookmark, name)
    }

    pub(crate) fn path(self, path: impl Into<String>) -> Self {
        self.push(Kind::Path, path)
    }

    pub(crate) fn op(self, id: impl Into<String>) -> Self {
        self.push(Kind::Operation, id)
    }

    pub(crate) fn remote(self, name: impl Into<String>) -> Self {
        self.push(Kind::Remote, name)
    }

    /// Code that isn't one of the named kinds: a revset, a config key.
    pub(crate) fn code(self, code: impl Into<String>) -> Self {
        self.push(Kind::Code, code)
    }

    /// `other` appended after this.
    pub(crate) fn then(mut self, other: Prose) -> Self {
        for (kind, text) in other.parts {
            self = self.push(kind, text);
        }
        self
    }

    /// Plain text where `` `quoted` `` runs are code, the way jj quotes names
    /// in its messages. An unpaired backtick stays as typed.
    pub(crate) fn from_backticks(source: &str) -> Self {
        let mut prose = Self::new();
        let mut rest = source;
        while let Some(open) = rest.find('`') {
            let Some(len) = rest[open + 1..].find('`') else {
                break;
            };
            let close = open + 1 + len;
            prose = prose.text(&rest[..open]);
            if len == 0 {
                prose = prose.text("``");
            } else {
                prose = prose.push(Kind::Code, &rest[open + 1..close]);
            }
            rest = &rest[close + 1..];
        }
        prose.text(rest)
    }

    /// The revision ids named, in order.
    #[cfg(test)]
    pub(crate) fn revisions(&self) -> impl Iterator<Item = &str> {
        self.parts
            .iter()
            .filter(|(kind, _)| *kind == Kind::Revision)
            .map(|(_, text)| text.as_str())
    }

    /// The bookmark names named, in order.
    #[cfg(test)]
    pub(crate) fn bookmarks(&self) -> impl Iterator<Item = &str> {
        self.parts
            .iter()
            .filter(|(kind, _)| *kind == Kind::Bookmark)
            .map(|(_, text)| text.as_str())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Rich text in `font` and `color`, with the names in `mono` and their
    /// own colours. Size, wrapping and width are the caller's.
    pub(crate) fn view<'a, Message: 'a>(
        &self,
        font: Font,
        mono: Font,
        color: Color,
        theme: ThemeSpec,
    ) -> Rich<'a, (), Message> {
        let spans: Vec<Span<'a, ()>> = self
            .parts
            .iter()
            .map(|(kind, text)| {
                let span = Span::new(text.clone());
                match kind {
                    Kind::Text => span,
                    Kind::Revision => span.font(mono).color(theme.accent),
                    Kind::Bookmark => span.font(mono).color(theme.lane_base),
                    Kind::Path | Kind::Operation | Kind::Remote | Kind::Code => span.font(mono),
                }
            })
            .collect();
        Rich::with_spans(spans).font(font).color(color)
    }

    /// The same text as one [`Aligned`] run per name, for a line that has
    /// to share a baseline with chips and glyphs (a rich text's runs aren't
    /// placed by [`crate::typography`], and drift off it). One line, no
    /// wrapping; the caller clips it.
    pub(crate) fn runs<'a, Message: 'a>(
        &self,
        font: Font,
        mono: Font,
        color: Color,
        theme: ThemeSpec,
        size: f32,
    ) -> Vec<Aligned<'a, Message>> {
        self.parts
            .iter()
            .map(|(kind, part)| {
                let (font, color) = match kind {
                    Kind::Text => (font, color),
                    Kind::Revision => (mono, theme.accent),
                    Kind::Bookmark => (mono, theme.lane_base),
                    Kind::Path | Kind::Operation | Kind::Remote | Kind::Code => (mono, color),
                };
                Aligned::text(
                    iced::widget::text(part.clone())
                        .size(size)
                        .font(font)
                        .color(color)
                        .wrapping(iced::widget::text::Wrapping::None),
                    font,
                    size,
                )
            })
            .collect()
    }
}

impl From<&str> for Prose {
    fn from(text: &str) -> Self {
        Self::new().text(text)
    }
}

impl From<String> for Prose {
    fn from(text: String) -> Self {
        Self::new().text(text)
    }
}

/// The plain sentence, names unquoted: for logs, tests and anywhere the
/// text can't be styled.
impl fmt::Display for Prose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (_, text) in &self.parts {
            f.write_str(text)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_separate_runs_and_plain_text_reads_as_written() {
        let prose = Prose::from("Move ")
            .bookmark("main")
            .text(" moves to ")
            .rev("svxkqqsv")
            .text(".");
        assert_eq!(prose.to_string(), "Move main moves to svxkqqsv.");
        let kinds: Vec<Kind> = prose.parts.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::Text,
                Kind::Bookmark,
                Kind::Text,
                Kind::Revision,
                Kind::Text
            ]
        );
    }

    #[test]
    fn adjacent_text_merges_and_empty_runs_vanish() {
        let prose = Prose::new().text("a").text("b").path("").text("c");
        assert_eq!(prose.parts, [(Kind::Text, "abc".to_owned())]);
        assert!(Prose::new().text("").is_empty());
    }

    #[test]
    fn backticked_names_become_code() {
        let prose = Prose::from_backticks("Revision `nope` doesn't exist");
        assert_eq!(
            prose.parts,
            [
                (Kind::Text, "Revision ".to_owned()),
                (Kind::Code, "nope".to_owned()),
                (Kind::Text, " doesn't exist".to_owned()),
            ]
        );
        assert_eq!(prose.to_string(), "Revision nope doesn't exist");
        // Unpaired and empty quotes are left as typed.
        assert_eq!(
            Prose::from_backticks("a ` b").parts,
            [(Kind::Text, "a ` b".to_owned())]
        );
        assert_eq!(Prose::from_backticks("x `` y").to_string(), "x `` y");
    }
}

//! Markdown read into [`Block`]s, and the blocks drawn as elements: what an
//! AI reply, a pull request's description, or a plugin's text reads like
//! rendered.
//!
//! Parsing is markdown-rs (`markdown`, CommonMark with GitHub's extensions),
//! which GPUI's component layer already builds; [`parse`] keeps only what
//! [`render`] draws. Headings, paragraphs, emphasis, strong, strikethrough,
//! inline code, fenced code (monospace on a well, no highlighting), lists
//! (ordered from their start, nested), block quotes, links and rules are
//! drawn; a table, raw HTML and the rest are shown as written, monospace.
//!
//! Re-parsing on every change is the intended use: a 2 000-line reply parses
//! in a few milliseconds (see the `measure` test), so streamed text is parsed
//! whole as it grows.

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::prelude::*;
use gpui_kit::{
    App, Div, ElementId, FontStyle, FontWeight, HighlightStyle, InteractiveText, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle, Window, div, px,
};
use markdown::mdast::Node;

use crate::theme::{self, Color, Font, ThemeSpec, text_size};

/// One block of a document.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// `level` 1 to 6.
    Heading {
        level: u8,
        text: Inlines,
    },
    Paragraph(Inlines),
    /// A fenced or indented block, its info string's first word as `lang`.
    Code {
        lang: Option<String>,
        text: String,
    },
    /// Each item its blocks; `start` numbers an ordered list.
    List {
        ordered: bool,
        start: u32,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Rule,
    /// What isn't drawn (a table, raw HTML), shown as it was written.
    Raw(String),
}

/// A run of text with its spans.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Inlines {
    pub text: String,
    /// Byte ranges of `text`; they nest (a link's text may be strong).
    pub spans: Vec<(Range<usize>, Span)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Span {
    Emphasis,
    Strong,
    Strike,
    Code,
    /// A link to this address.
    Link(String),
}

/// `source`'s blocks. Never fails: what markdown-rs refuses (it refuses
/// only MDX, which isn't turned on) comes back as one [`Block::Raw`].
pub fn parse(source: &str) -> Vec<Block> {
    match markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        Ok(root) => blocks(root.children().map_or(&[][..], Vec::as_slice), source),
        Err(_) => vec![Block::Raw(source.to_owned())],
    }
}

fn blocks(nodes: &[Node], source: &str) -> Vec<Block> {
    nodes
        .iter()
        .filter_map(|node| block(node, source))
        .collect()
}

fn block(node: &Node, source: &str) -> Option<Block> {
    Some(match node {
        Node::Heading(heading) => Block::Heading {
            level: heading.depth,
            text: inlines(&heading.children),
        },
        Node::Paragraph(paragraph) => Block::Paragraph(inlines(&paragraph.children)),
        Node::Code(code) => Block::Code {
            lang: code
                .lang
                .as_ref()
                .and_then(|lang| lang.split_whitespace().next())
                .map(str::to_owned),
            text: code.value.clone(),
        },
        Node::List(list) => Block::List {
            ordered: list.ordered,
            start: list.start.unwrap_or(1),
            items: list
                .children
                .iter()
                .map(|item| blocks(item.children().map_or(&[][..], Vec::as_slice), source))
                .collect(),
        },
        Node::Blockquote(quote) => Block::Quote(blocks(&quote.children, source)),
        Node::ThematicBreak(_) => Block::Rule,
        // Link and footnote definitions say nothing on their own.
        Node::Definition(_) | Node::FootnoteDefinition(_) => return None,
        other => {
            let position = other.position()?;
            let written = source.get(position.start.offset..position.end.offset)?;
            Block::Raw(written.trim_end().to_owned())
        }
    })
}

fn inlines(nodes: &[Node]) -> Inlines {
    let mut out = Inlines::default();
    for node in nodes {
        inline(node, &mut out);
    }
    out
}

fn inline(node: &Node, out: &mut Inlines) {
    let start = out.text.len();
    let span = |out: &mut Inlines, children: &[Node], kind: Span| {
        for child in children {
            inline(child, out);
        }
        if out.text.len() > start {
            out.spans.push((start..out.text.len(), kind));
        }
    };
    match node {
        Node::Text(text) => out.text.push_str(&text.value),
        Node::Html(html) => out.text.push_str(&html.value),
        Node::Break(_) => out.text.push('\n'),
        Node::InlineCode(code) => {
            out.text.push_str(&code.value);
            out.spans.push((start..out.text.len(), Span::Code));
        }
        Node::Emphasis(node) => span(out, &node.children, Span::Emphasis),
        Node::Strong(node) => span(out, &node.children, Span::Strong),
        Node::Delete(node) => span(out, &node.children, Span::Strike),
        Node::Link(link) => span(out, &link.children, Span::Link(link.url.clone())),
        Node::Image(image) => out.text.push_str(&image.alt),
        other => {
            for child in other.children().map_or(&[][..], Vec::as_slice) {
                inline(child, out);
            }
        }
    }
}

/// How a document is drawn.
#[derive(Clone, Copy)]
pub struct MarkdownLook {
    pub theme: ThemeSpec,
    pub font: Font,
    pub mono: Font,
    /// The body's text size; headings and code size from it.
    pub size: f32,
}

/// What a link click does: open the address.
pub type OnLink = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// `blocks` as a column, `id` prefixing every interactive part's id.
pub fn render(id: &str, blocks: &[Block], look: MarkdownLook, on_link: OnLink) -> Div {
    let mut column = div().flex().flex_col().w_full().gap(px(theme::space::MD));
    for (index, block) in blocks.iter().enumerate() {
        column = column.child(render_block(
            &format!("{id}-{index}"),
            block,
            look,
            &on_link,
        ));
    }
    column
}

fn render_block(id: &str, block: &Block, look: MarkdownLook, on_link: &OnLink) -> Div {
    let MarkdownLook {
        theme,
        font,
        mono,
        size,
    } = look;
    match block {
        Block::Heading { level, text } => {
            let scale = match level {
                1 => 1.45,
                2 => 1.25,
                3 => 1.1,
                _ => 1.0,
            };
            let heading = MarkdownLook {
                size: size * scale,
                ..look
            };
            div()
                .when(*level <= 2, |block| block.pt(px(theme::space::SM)))
                .font_weight(FontWeight::SEMIBOLD)
                .child(paragraph(id, text, heading, on_link))
        }
        Block::Paragraph(text) => div().child(paragraph(id, text, look, on_link)),
        Block::Code { text, .. } | Block::Raw(text) => code_block(text, theme, mono, size),
        Block::List {
            ordered,
            start,
            items,
        } => {
            let mut list = div().flex().flex_col().gap(px(theme::space::XS));
            for (index, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}.", *start as usize + index)
                } else {
                    "\u{2022}".to_owned()
                };
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(theme::space::SM))
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(size * 1.2))
                                .font(font.gpui())
                                .text_size(px(size))
                                .line_height(px(line_height(size)))
                                .text_color(theme.muted_text)
                                .child(marker),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                render(&format!("{id}-{index}"), item, look, on_link.clone())
                                    .gap(px(theme::space::XS)),
                            ),
                        ),
                );
            }
            list
        }
        Block::Quote(blocks) => div()
            .pl(px(theme::space::LG))
            .border_l_2()
            .border_color(theme.border)
            .child(render(
                id,
                blocks,
                MarkdownLook {
                    theme: ThemeSpec {
                        text: theme.muted_text,
                        ..theme
                    },
                    ..look
                },
                on_link.clone(),
            )),
        Block::Rule => div()
            .w_full()
            .my(px(theme::space::XS))
            .h(px(theme::HAIRLINE))
            .bg(theme.border),
    }
}

fn line_height(size: f32) -> f32 {
    (size * 1.5).round()
}

/// Wrapping text with its spans styled, its links clickable.
fn paragraph(id: &str, text: &Inlines, look: MarkdownLook, on_link: &OnLink) -> Div {
    let MarkdownLook {
        theme,
        font,
        mono,
        size,
    } = look;
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut monospace: Vec<(Range<usize>, SharedString)> = Vec::new();
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    for (range, span) in &text.spans {
        let style = match span {
            Span::Emphasis => HighlightStyle {
                font_style: Some(FontStyle::Italic),
                ..Default::default()
            },
            Span::Strong => HighlightStyle {
                font_weight: Some(FontWeight::SEMIBOLD),
                ..Default::default()
            },
            Span::Strike => HighlightStyle {
                strikethrough: Some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: None,
                }),
                ..Default::default()
            },
            Span::Code => {
                monospace.push((range.clone(), mono.resolved_family().into()));
                HighlightStyle {
                    background_color: Some(code_fill(theme).into()),
                    ..Default::default()
                }
            }
            Span::Link(url) => {
                links.push((range.clone(), url.clone()));
                HighlightStyle {
                    color: Some(theme.accent.into()),
                    underline: Some(UnderlineStyle {
                        thickness: px(1.0),
                        color: None,
                        wavy: false,
                    }),
                    ..Default::default()
                }
            }
        };
        highlights.push((range.clone(), style));
    }
    let styled = StyledText::new(text.text.clone())
        .with_highlights(flatten(highlights))
        .with_font_family_overrides(monospace);
    let (ranges, urls): (Vec<_>, Vec<_>) = links.into_iter().unzip();
    let on_link = on_link.clone();
    let interactive = InteractiveText::new(
        ElementId::Name(SharedString::from(id.to_owned())),
        styled,
    )
    .on_click(ranges, move |index, window, cx| {
        if let Some(url) = urls.get(index) {
            on_link(url, window, cx);
        }
    });
    div()
        .w_full()
        .font(font.gpui())
        .text_size(px(size))
        .line_height(px(line_height(size)))
        .text_color(theme.text)
        .child(interactive)
}

/// `highlights` cut where they overlap, each piece carrying every style
/// over it: GPUI wants them sorted and apart.
fn flatten(highlights: Vec<(Range<usize>, HighlightStyle)>) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut cuts: Vec<usize> = highlights
        .iter()
        .flat_map(|(range, _)| [range.start, range.end])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter_map(|pair| {
            let piece = pair[0]..pair[1];
            let mut style: Option<HighlightStyle> = None;
            for (range, over) in &highlights {
                if range.start <= piece.start && piece.end <= range.end {
                    style = Some(match style {
                        Some(under) => under.highlight(*over),
                        None => *over,
                    });
                }
            }
            style.map(|style| (piece, style))
        })
        .collect()
}

fn code_fill(theme: ThemeSpec) -> Color {
    theme::mix(theme.panel_background, theme.text, 0.06)
}

/// Monospace on a well, wrapping long lines.
fn code_block(text: &str, theme: ThemeSpec, mono: Font, size: f32) -> Div {
    let size = size * 0.92;
    div()
        .w_full()
        .px(px(theme::space::MD))
        .py(px(theme::space::SM))
        .rounded(px(theme::radius::CONTROL))
        .bg(code_fill(theme))
        .font(mono.gpui())
        .text_size(px(size))
        .line_height(px(line_height(size)))
        .text_color(theme.text)
        .child(StyledText::new(SharedString::from(text.to_owned())))
}

/// The text size a document reads at when nothing says otherwise.
pub const BODY_SIZE: f32 = text_size::BODY;

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraph_of(source: &str) -> Inlines {
        match parse(source).into_iter().next() {
            Some(Block::Paragraph(inlines)) => inlines,
            other => panic!("not a paragraph: {other:?}"),
        }
    }

    #[test]
    fn headings_paragraphs_and_rules() {
        assert_eq!(
            parse("# Title\n\nSome text\non two lines.\n\n---\n\n### Small"),
            vec![
                Block::Heading {
                    level: 1,
                    text: Inlines {
                        text: "Title".into(),
                        spans: vec![]
                    }
                },
                Block::Paragraph(Inlines {
                    text: "Some text\non two lines.".into(),
                    spans: vec![]
                }),
                Block::Rule,
                Block::Heading {
                    level: 3,
                    text: Inlines {
                        text: "Small".into(),
                        spans: vec![]
                    }
                },
            ]
        );
    }

    #[test]
    fn emphasis_strong_code_strike_and_links_are_spans() {
        let text =
            paragraph_of("An *em*, a **strong `code`**, ~~gone~~ and [a link](https://x.test).");
        assert_eq!(text.text, "An em, a strong code, gone and a link.");
        let span = |kind: &Span| {
            text.spans
                .iter()
                .find(|(_, span)| span == kind)
                .map(|(range, _)| &text.text[range.clone()])
        };
        assert_eq!(span(&Span::Emphasis), Some("em"));
        assert_eq!(span(&Span::Strong), Some("strong code"));
        assert_eq!(span(&Span::Code), Some("code"));
        assert_eq!(span(&Span::Strike), Some("gone"));
        assert_eq!(span(&Span::Link("https://x.test".into())), Some("a link"));
    }

    #[test]
    fn overlapping_spans_flatten_into_pieces() {
        let bold = HighlightStyle {
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        };
        let italic = HighlightStyle {
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        };
        let pieces = flatten(vec![(0..10, bold), (4..6, italic)]);
        let ranges: Vec<Range<usize>> = pieces.iter().map(|(range, _)| range.clone()).collect();
        assert_eq!(ranges, [0..4, 4..6, 6..10]);
        assert_eq!(pieces[1].1.font_style, Some(FontStyle::Italic));
        assert_eq!(pieces[1].1.font_weight, Some(FontWeight::SEMIBOLD));
    }

    #[test]
    fn code_blocks_keep_their_text_and_language() {
        assert_eq!(
            parse("```rust title\nfn main() {}\n```\n\n    indented\n"),
            vec![
                Block::Code {
                    lang: Some("rust".into()),
                    text: "fn main() {}".into()
                },
                Block::Code {
                    lang: None,
                    text: "indented".into()
                },
            ]
        );
    }

    #[test]
    fn lists_number_from_their_start_and_nest() {
        let blocks = parse("3. three\n4. four\n   - inner\n");
        let Block::List {
            ordered,
            start,
            items,
        } = &blocks[0]
        else {
            panic!("{blocks:?}")
        };
        assert!(*ordered);
        assert_eq!(*start, 3);
        assert_eq!(items.len(), 2);
        assert!(
            matches!(&items[1][1], Block::List { ordered: false, items, .. } if items.len() == 1)
        );
    }

    #[test]
    fn quotes_hold_blocks_and_tables_stay_as_written() {
        let blocks = parse("> quoted **text**\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(
            matches!(&blocks[0], Block::Quote(inner) if matches!(&inner[0], Block::Paragraph(text) if text.text == "quoted text"))
        );
        assert_eq!(
            blocks[1],
            Block::Raw("| a | b |\n|---|---|\n| 1 | 2 |".into())
        );
    }

    #[test]
    fn a_half_written_document_still_parses() {
        // What a stream looks like mid-way: an open fence, an open emphasis.
        let blocks = parse("Intro *still go\n\n```rust\nfn half(");
        assert!(matches!(&blocks[1], Block::Code { text, .. } if text == "fn half("));
    }

    /// Parse time of a 2 000-line reply, for the doc comment above.
    #[test]
    #[ignore = "a measurement: run on demand"]
    fn measure_a_long_reply() {
        let chunk = "## Section\n\nSome **bold** text with `code` and a [link](https://x.test).\n\n- one\n- two\n  - nested\n\n```rust\nfn main() {}\n```\n\n";
        let source = chunk.repeat(2000 / chunk.lines().count());
        let started = std::time::Instant::now();
        let blocks = parse(&source);
        println!(
            "{} lines, {} blocks, parsed in {:?}",
            source.lines().count(),
            blocks.len(),
            started.elapsed()
        );
    }
}

//! Image previews inside the diff: a block per image or SVG file, drawn in place of
//! its rows. Before and after sit side by side (one column for an added or deleted
//! file) on a checkerboard so transparency reads, fitted to the pane without scaling
//! past 1:1, each captioned with its dimensions and size.

use std::sync::Arc;

use super::layout::View;
use super::paint::Plan;
use crate::geometry::Rectangle;
use crate::scrollbar;
use crate::theme::Color;

/// Inset of the previews from the block's edges.
const PADDING: f32 = 16.0;
/// Gap between the before and after columns.
const COLUMN_GAP: f32 = 24.0;
/// Space between the tallest preview and the captions under it.
const CAPTION_GAP: f32 = 8.0;
/// A preview never stands taller than this: a long screenshot would otherwise be a
/// scroll of its own.
const MAX_HEIGHT: f32 = 560.0;
/// The frame of a side whose pixels can't be read.
const PLACEHOLDER: (f32, f32) = (160.0, 96.0);
/// Side of one checkerboard square.
const CHECKER_CELL: f32 = 8.0;

/// A file's preview, drawn in place of its rows (see [`super::DiffProps::media`]).
#[derive(Debug, Clone)]
pub struct Media {
    /// `None` for an added file.
    pub before: Option<MediaSide>,
    /// `None` for a deleted file.
    pub after: Option<MediaSide>,
}

#[derive(Debug, Clone)]
pub struct MediaSide {
    pub size: u64,
    /// Natural pixel size; `None` when the content doesn't parse.
    pub dimensions: Option<(u32, u32)>,
    pub image: MediaImage,
}

#[derive(Debug, Clone)]
pub enum MediaImage {
    /// Decoding off the UI thread; the frame already has its final size.
    Loading,
    Ready(Arc<gpui_kit::RenderImage>),
    /// No pixels to show: unreadable, too large, or the decode failed.
    Unavailable,
}

/// Where one side lands inside its block, relative to the block's top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SideFrame {
    /// 0 for before, 1 for after.
    pub side: usize,
    /// The column the side and its caption are confined to.
    pub column: Rectangle,
    /// The fitted preview itself.
    pub frame: Rectangle,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct MediaLayout {
    pub height: f32,
    pub frames: Vec<SideFrame>,
    /// Top of the caption row.
    pub caption_y: f32,
}

/// Lay `media` out across a pane `viewport_width` wide, with captions one
/// `caption_height` row tall.
pub(super) fn layout(media: &Media, viewport_width: f32, caption_height: f32) -> MediaLayout {
    let available = (viewport_width - 2.0 * PADDING - scrollbar::GUTTER).max(32.0);
    let sides: Vec<(usize, &MediaSide)> = [&media.before, &media.after]
        .into_iter()
        .enumerate()
        .filter_map(|(index, side)| Some((index, side.as_ref()?)))
        .collect();
    let columns = sides.len().max(1) as f32;
    let column_width = ((available - COLUMN_GAP * (columns - 1.0)) / columns).floor();
    let frames: Vec<SideFrame> = sides
        .iter()
        .enumerate()
        .map(|(slot, (side, media_side))| {
            let (width, height) = media_side
                .dimensions
                .map_or(PLACEHOLDER, |(w, h)| (w as f32, h as f32));
            let scale = 1.0_f32.min(column_width / width).min(MAX_HEIGHT / height);
            let x = PADDING + slot as f32 * (column_width + COLUMN_GAP);
            SideFrame {
                side: *side,
                column: Rectangle {
                    x,
                    y: PADDING,
                    width: column_width,
                    height: 0.0,
                },
                frame: Rectangle {
                    x,
                    y: PADDING,
                    width: (width * scale).round().max(1.0),
                    height: (height * scale).round().max(1.0),
                },
            }
        })
        .collect();
    let tallest = frames
        .iter()
        .map(|frame| frame.frame.height)
        .fold(0.0, f32::max);
    let caption_y = PADDING + tallest + CAPTION_GAP;
    MediaLayout {
        height: caption_y + caption_height + PADDING,
        frames,
        caption_y,
    }
}

/// The caption under one side: its dimensions and size, and for the after side of a
/// change, how the size moved.
fn caption(side: &MediaSide, other: Option<&MediaSide>) -> (String, Option<(String, bool)>) {
    let size = diffui_core::media::format_size(side.size);
    let text = match side.dimensions {
        Some((width, height)) => format!("{width} \u{d7} {height}  {size}"),
        None => size,
    };
    let delta = other.and_then(|other| {
        diffui_core::media::format_size_delta(other.size, side.size)
            .map(|delta| (delta, side.size > other.size))
    });
    (text, delta)
}

fn intersect(a: Rectangle, b: Rectangle) -> Option<Rectangle> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then_some(Rectangle {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

/// Draw `media`'s block with its top at screen-y `top`.
pub(super) fn draw(view: &View<'_>, plan: &mut Plan, media: &Media, top: f32, bounds: Rectangle) {
    let palette = &view.props.palette;
    let row_height = view.metrics.row_height;
    let laid = layout(media, bounds.width, row_height);
    // Opaque over the gutter chrome: a preview has no line numbers.
    plan.rect(bounds.x, top, bounds.width, laid.height, palette.panel);
    let both = media.before.is_some() && media.after.is_some();
    for placed in &laid.frames {
        let (side, other) = if placed.side == 0 {
            (media.before.as_ref(), None)
        } else {
            (media.after.as_ref(), media.before.as_ref())
        };
        let Some(side) = side else { continue };
        let frame = Rectangle {
            x: bounds.x + placed.frame.x,
            y: top + placed.frame.y,
            ..placed.frame
        };
        let column = Rectangle {
            x: bounds.x + placed.column.x,
            y: top,
            width: placed.column.width,
            height: laid.height,
        };
        draw_frame(view, plan, side, frame, bounds);

        let text_y = top + laid.caption_y + view.metrics.text_y_pad;
        let clip = intersect(column, bounds).unwrap_or(column);
        let mut x = column.x;
        let mut piece = |plan: &mut Plan, content: &str, color: Color| {
            view.code_line(plan, content, x, text_y, color, clip);
            x += (content.chars().count() as f32 + 2.0) * view.metrics.char_width;
        };
        if both {
            let label = if placed.side == 0 { "Before" } else { "After" };
            piece(plan, label, palette.text);
        }
        let (text, delta) = caption(side, other);
        piece(plan, &text, palette.text_muted);
        if let Some((delta, grew)) = delta {
            let color = if grew {
                palette.addition_text
            } else {
                palette.deletion_text
            };
            piece(plan, &delta, color);
        }
    }
}

/// One side's preview: a hairline frame, the checkerboard, and the pixels over it
/// once they're decoded.
fn draw_frame(
    view: &View<'_>,
    plan: &mut Plan,
    side: &MediaSide,
    frame: Rectangle,
    bounds: Rectangle,
) {
    let Some(visible) = intersect(frame, bounds) else {
        return;
    };
    let palette = &view.props.palette;
    // The hairline sits just outside the pixels, so a 1:1 preview keeps every one of
    // them.
    plan.outline(
        Rectangle {
            x: frame.x - 1.0,
            y: frame.y - 1.0,
            width: frame.width + 2.0,
            height: frame.height + 2.0,
        },
        palette.border,
    );
    draw_checkerboard(view, plan, frame, visible);
    match &side.image {
        MediaImage::Ready(image) => plan.image(frame, Arc::clone(image), bounds),
        MediaImage::Loading => {}
        MediaImage::Unavailable => {
            let label = "No preview";
            let width = view.text_width(label);
            view.code_line(
                plan,
                label,
                frame.x + ((frame.width - width) / 2.0).max(0.0) + 8.0,
                frame.y + (frame.height - view.metrics.row_height) / 2.0 + view.metrics.text_y_pad,
                palette.text_muted,
                visible,
            );
        }
    }
}

/// Two-tone squares behind the pixels, painted only where `visible`.
fn draw_checkerboard(view: &View<'_>, plan: &mut Plan, frame: Rectangle, visible: Rectangle) {
    let palette = &view.props.palette;
    let light = palette.panel;
    let dark = mix(palette.panel, palette.text, 0.08);
    plan.rect(visible.x, visible.y, visible.width, visible.height, light);
    let first_row = ((visible.y - frame.y) / CHECKER_CELL).floor().max(0.0) as usize;
    let last_row = ((visible.y + visible.height - frame.y) / CHECKER_CELL).ceil() as usize;
    let columns = (frame.width / CHECKER_CELL).ceil() as usize;
    for row in first_row..last_row {
        let y = frame.y + row as f32 * CHECKER_CELL;
        for column in (row % 2..columns).step_by(2) {
            let x = frame.x + column as f32 * CHECKER_CELL;
            let cell = Rectangle {
                x,
                y,
                width: CHECKER_CELL.min(frame.x + frame.width - x),
                height: CHECKER_CELL.min(frame.y + frame.height - y),
            };
            if let Some(cell) = intersect(cell, visible) {
                plan.rect(cell.x, cell.y, cell.width, cell.height, dark);
            }
        }
    }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(dimensions: Option<(u32, u32)>, size: u64) -> MediaSide {
        MediaSide {
            size,
            dimensions,
            image: MediaImage::Loading,
        }
    }

    const WIDE: f32 = 2000.0;

    #[test]
    fn small_images_stay_at_their_natural_size() {
        let media = Media {
            before: None,
            after: Some(side(Some((64, 32)), 10)),
        };
        let laid = layout(&media, WIDE, 20.0);
        assert_eq!(laid.frames.len(), 1);
        assert_eq!(
            (laid.frames[0].frame.width, laid.frames[0].frame.height),
            (64.0, 32.0)
        );
        assert_eq!(laid.caption_y, PADDING + 32.0 + CAPTION_GAP);
        assert_eq!(laid.height, laid.caption_y + 20.0 + PADDING);
    }

    #[test]
    fn two_sides_split_the_pane_and_fit_their_column() {
        let media = Media {
            before: Some(side(Some((4000, 1000)), 10)),
            after: Some(side(Some((100, 100)), 10)),
        };
        let width = 1000.0;
        let laid = layout(&media, width, 20.0);
        let column = laid.frames[0].column.width;
        assert_eq!(
            column,
            ((width - 2.0 * PADDING - scrollbar::GUTTER - COLUMN_GAP) / 2.0).floor()
        );
        let before = laid.frames[0].frame;
        assert_eq!(before.width, column);
        assert_eq!(before.height, (1000.0 * column / 4000.0).round());
        // The after side starts its own column and keeps 1:1.
        let after = laid.frames[1];
        assert_eq!(after.side, 1);
        assert_eq!(after.frame.x, PADDING + column + COLUMN_GAP);
        assert_eq!((after.frame.width, after.frame.height), (100.0, 100.0));
        // Captions line up under the taller side.
        assert_eq!(
            laid.caption_y,
            PADDING + before.height.max(100.0) + CAPTION_GAP
        );
    }

    #[test]
    fn tall_images_cap_their_height_and_unreadable_ones_get_a_placeholder() {
        let media = Media {
            before: Some(side(Some((200, 4000)), 10)),
            after: Some(side(None, 10)),
        };
        let laid = layout(&media, WIDE, 20.0);
        assert_eq!(laid.frames[0].frame.height, MAX_HEIGHT);
        assert_eq!(
            laid.frames[0].frame.width,
            (200.0 * MAX_HEIGHT / 4000.0).round()
        );
        assert_eq!(
            (laid.frames[1].frame.width, laid.frames[1].frame.height),
            PLACEHOLDER
        );
    }

    #[test]
    fn captions_carry_dimensions_size_and_the_delta() {
        let before = side(Some((320, 200)), 2048);
        let after = side(Some((360, 200)), 1024);
        assert_eq!(
            caption(&before, None),
            ("320 \u{d7} 200  2.0 KB".to_owned(), None)
        );
        assert_eq!(
            caption(&after, Some(&before)),
            (
                "360 \u{d7} 200  1.0 KB".to_owned(),
                Some(("-1.0 KB".to_owned(), false))
            )
        );
        assert_eq!(caption(&side(None, 5), None).0, "5 B");
    }
}

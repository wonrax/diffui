//! The one chip component: small rounded-rect label used for bookmark /
//! workspace / status indicators in the sidebar's commit rows, the file
//! list's A/M/D badges, and the diff view's revision-header bookmarks and
//! file-header status. Every custom-drawn chip goes through [`draw`] so they
//! share geometry, text treatment, and border styles by construction;
//! widget-built chips (the activity bar's counters) pick up the same look
//! via [`container_style`].

use iced::advanced::graphics::geometry::{self, Frame, LineCap, LineJoin, Path, Stroke};
use iced::advanced::text;
use iced::advanced::{renderer, text::Text};
use iced::{Background, Border, Color, Font, Pixels, Point, Rectangle, Shadow, Size, alignment};

use crate::theme::chip_background;
use crate::{icons, measure};

pub const TEXT_SIZE: f32 = crate::theme::text_size::BODY;
pub const PAD_X: f32 = 5.0;
pub const RADIUS: f32 = 5.0;
/// Chip icon glyph size — a step under the label size so the glyph's optical
/// weight matches the text next to it.
const ICON_SIZE: f32 = 11.0;
/// Gap between a chip's icon and its label.
const ICON_GAP: f32 = 3.0;

/// Tight box: just enough vertical room for the cap-height plus a hair of
/// breathing room. Anything more makes the chip dwarf the text around it.
pub fn height() -> f32 {
    (TEXT_SIZE + 3.0).round()
}

/// Container style for chips built from iced widgets rather than drawn via
/// [`draw`] (the activity bar's `+N` / `N queued`), so element-based chips
/// share the drawn chips' corner radius and translucent fill.
pub fn container_style(color: Color) -> iced::widget::container::Style {
    iced::widget::container::Style {
        background: Some(Background::Color(chip_background(color))),
        border: Border {
            width: 0.0,
            color: Color::TRANSPARENT,
            radius: RADIUS.into(),
        },
        ..iced::widget::container::Style::default()
    }
}

#[derive(Debug, Clone)]
pub struct Chip {
    pub label: String,
    /// Typeface for the label. Name-like chips (bookmarks) read best in the
    /// UI's sans-serif; tag-like chips (file status letters, `empty`,
    /// `conflict`, …) use the mono font.
    pub font: Font,
    pub background: Color,
    pub text_color: Color,
    /// Optional 1px chip border. When `border_dashed` is `true`, the
    /// stroke is dashed (used by the `empty` chip in the design
    /// system). When `false`, it's a solid 1px line (used by remote
    /// bookmark chips — outlined, lane-colored).
    pub border_color: Option<Color>,
    pub border_dashed: bool,
    /// Optional Lucide glyph drawn ahead of the label, tinted like it.
    /// Workspace chips carry one so a `name@` working-copy marker reads as
    /// a different kind of thing than the bookmark pills around it —
    /// color alone doesn't separate two chips in the same rail.
    pub icon: Option<&'static str>,
}

/// Full chip width (icon + label + horizontal padding) for the given label,
/// rendered with `font` at [`TEXT_SIZE`].
pub fn width(label: &str, icon: Option<&str>, font: Font) -> f32 {
    let label_w = measure::line_width(label, TEXT_SIZE, font);
    let icon_w = icon.map_or(0.0, |glyph| {
        measure::line_width(glyph, ICON_SIZE, icons::ICON_FONT) + ICON_GAP
    });
    label_w + icon_w + PAD_X * 2.0
}

/// Draw `chip` with its left edge at `x`, vertically centered on `center_y`.
/// Returns the chip's width so rails can advance to the next chip.
pub fn draw<R>(renderer: &mut R, chip: &Chip, x: f32, center_y: f32, clip: Rectangle) -> f32
where
    R: text::Renderer<Font = Font> + geometry::Renderer,
{
    let label_w = measure::line_width(&chip.label, TEXT_SIZE, chip.font);
    let icon_block = chip.icon.map(|glyph| {
        (
            glyph,
            measure::line_width(glyph, ICON_SIZE, icons::ICON_FONT),
        )
    });
    let icon_indent = icon_block.map_or(0.0, |(_, icon_w)| icon_w + ICON_GAP);
    let chip_h = height();
    let chip_w = icon_indent + label_w + PAD_X * 2.0;
    let rect = Rectangle {
        x,
        y: (center_y - chip_h / 2.0).round(),
        width: chip_w,
        height: chip_h,
    };
    // Skip the fill when the chip is meant to read as outlined-only —
    // a translucent fill with `a == 0` would still emit a quad but
    // avoiding it keeps the layer count down and makes intent clear.
    if chip.background.a > f32::EPSILON {
        renderer.fill_quad(
            renderer::Quad {
                bounds: rect,
                border: Border {
                    radius: iced::border::Radius::from(RADIUS),
                    ..Border::default()
                },
                shadow: Shadow::default(),
                snap: true,
            },
            Background::Color(chip.background),
        );
    }
    if let Some(border_color) = chip.border_color {
        if chip.border_dashed {
            stroke_dashed_rounded_rect(renderer, rect, border_color, RADIUS);
        } else {
            stroke_solid_rounded_rect(renderer, rect, border_color, RADIUS);
        }
    }
    // The icon can't share the label's centering path: Lucide's em box
    // sits entirely above the baseline (ascent = em, descent = 0), so any
    // line-height leading beyond the glyph size lands asymmetrically and
    // shoves the ink off-center. Collapsing the line box to exactly the
    // glyph size makes centering it equal centering the ink — the same
    // trick `icons::icon` documents for the widget path.
    // Anchor the label (and icon) to the *rounded* rect's own mid-line, not
    // the raw `center_y` — the rect's y snaps to the pixel grid above, and
    // anchoring the text elsewhere reads as the label floating off-center.
    let rect_mid_y = rect.y + chip_h / 2.0;
    if let Some((glyph, icon_w)) = icon_block {
        renderer.fill_text(
            Text {
                content: glyph.to_owned(),
                bounds: Size::new(icon_w.max(1.0), ICON_SIZE),
                size: Pixels(ICON_SIZE),
                line_height: text::LineHeight::Absolute(Pixels(ICON_SIZE)),
                font: icons::ICON_FONT,
                align_x: text::Alignment::Left,
                align_y: alignment::Vertical::Center,
                shaping: text::Shaping::Advanced,
                wrapping: text::Wrapping::None,
                ellipsis: text::Ellipsis::None,
                hint_factor: None,
            },
            Point::new(x + PAD_X, rect_mid_y),
            chip.text_color,
            clip,
        );
    }
    // Line box pinned to the chip's own height: a taller box (the usual
    // 1.4× multiplier) distributes its extra leading around the glyphs,
    // and any asymmetry in that distribution shows up as the label riding
    // high or low inside a box this tight.
    renderer.fill_text(
        Text {
            content: chip.label.clone(),
            bounds: Size::new((chip_w - icon_indent).max(1.0), chip_h),
            size: Pixels(TEXT_SIZE),
            line_height: text::LineHeight::Absolute(Pixels(chip_h)),
            font: chip.font,
            align_x: text::Alignment::Center,
            align_y: alignment::Vertical::Center,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
            ellipsis: text::Ellipsis::None,
            hint_factor: None,
        },
        Point::new(x + icon_indent + (chip_w - icon_indent) / 2.0, rect_mid_y),
        chip.text_color,
        clip,
    );
    chip_w
}

/// Solid 1px rounded-rect outline. Cheaper than the dashed variant —
/// goes through iced's quad border instead of a geometry path. Used by
/// remote-bookmark chips that want a crisp outlined look.
fn stroke_solid_rounded_rect<R: renderer::Renderer>(
    renderer: &mut R,
    rect: Rectangle,
    color: Color,
    radius: f32,
) {
    renderer.fill_quad(
        renderer::Quad {
            bounds: rect,
            border: Border {
                color,
                width: 1.0,
                radius: iced::border::Radius::from(radius),
            },
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(Color::TRANSPARENT),
    );
}

/// Dashed 1px rounded-rect outline. Used by outlined chips (e.g. the
/// `empty` indicator). Iced's quad border can't be dashed, so we stroke
/// a canvas path with a `LineDash` pattern.
fn stroke_dashed_rounded_rect<R: geometry::Renderer>(
    renderer: &mut R,
    rect: Rectangle,
    color: Color,
    radius: f32,
) {
    // Inset by half a pixel so the 1px stroke sits on the rect's
    // edge rather than straddling it (avoids fuzzy aliasing).
    let inset_rect = Rectangle {
        x: rect.x + 0.5,
        y: rect.y + 0.5,
        width: rect.width - 1.0,
        height: rect.height - 1.0,
    };
    let path = Path::rounded_rectangle(
        Point::new(inset_rect.x, inset_rect.y),
        Size::new(inset_rect.width, inset_rect.height),
        iced::border::Radius::from(radius),
    );
    let segments: [f32; 2] = [4.0, 2.2];
    let mut stroke = Stroke::default()
        .with_color(color)
        .with_width(1.0)
        .with_line_cap(LineCap::Butt)
        .with_line_join(LineJoin::Miter);
    stroke.line_dash = geometry::LineDash {
        segments: &segments,
        offset: 0,
    };
    let mut frame = Frame::new(
        renderer,
        Size::new(rect.x + rect.width + 1.0, rect.y + rect.height + 1.0),
    );
    frame.stroke(&path, stroke);
    renderer.draw_geometry(frame.into_geometry());
}

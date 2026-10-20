//! Rich previews of the files a diff can't show as text: which paths are
//! images or SVG, each side's size and pixel dimensions, and decoding a side
//! to RGBA off the UI thread.
//!
//! The diff loader classifies and measures (cheap: an image's header, an
//! SVG's parse); the frontend decodes ([`decode`]) only what it displays.

use std::io::Cursor;
use std::sync::{Arc, OnceLock};

use resvg::usvg;

use crate::model::{BlobKind, BlobSide};

/// Longest side, in pixels, of a decoded preview. Larger images are scaled
/// down to it: the pane never shows them bigger, and an 8000px photo would
/// otherwise cost a quarter gigabyte of texture.
pub const MAX_DECODED_SIDE: u32 = 2048;

/// Pixel count past which a raster image is not decoded at all.
const MAX_DECODE_PIXELS: u64 = 64 * 1024 * 1024;

/// SVGs rasterize at this multiple of their natural size, so they stay sharp
/// on a 2x display.
const SVG_RASTER_SCALE: f32 = 2.0;

/// What `path` holds, judged by its extension and its bytes: images are
/// binary, SVG is text. `None` for a plain text file.
pub fn kind_of(path: &str, binary: bool, sample: &[u8]) -> Option<BlobKind> {
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    if extension == "svg" && !binary {
        return Some(BlobKind::Svg);
    }
    let image_extension = matches!(
        extension.as_str(),
        "png" | "apng" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "cur" | "bmp"
    );
    if binary && (image_extension || image::guess_format(sample).is_ok()) {
        return Some(BlobKind::Image);
    }
    binary.then_some(BlobKind::Binary)
}

/// Pixel size of one side, from its header (raster) or its root element
/// (SVG). `None` when the bytes don't parse as `kind`.
pub fn dimensions(kind: BlobKind, bytes: &[u8]) -> Option<(u32, u32)> {
    match kind {
        BlobKind::Binary => None,
        BlobKind::Image => image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()
            .filter(|&(width, height)| width > 0 && height > 0),
        BlobKind::Svg => {
            let tree = usvg::Tree::from_data(bytes, &svg_options()).ok()?;
            let size = tree.size();
            Some((
                size.width().ceil().max(1.0) as u32,
                size.height().ceil().max(1.0) as u32,
            ))
        }
    }
}

/// A side's descriptor, keeping `bytes` only when `keep` (the caps allowed a
/// preview) and the format parsed. `hash` is the content id the diff carries;
/// a conflict or a dummy id gives no stable identity and so no preview.
pub fn side(kind: BlobKind, hash: &str, bytes: &[u8], keep: bool) -> BlobSide {
    let dimensions = dimensions(kind, bytes);
    let previewable = keep && dimensions.is_some() && hash.bytes().any(|b| b != b'0');
    BlobSide {
        size: bytes.len() as u64,
        id: format!("{hash}:{}", bytes.len()),
        dimensions,
        bytes: previewable.then(|| Arc::from(bytes)),
    }
}

/// One side decoded for display: straight (not premultiplied) RGBA8, at most
/// [`MAX_DECODED_SIDE`] on its longer edge.
#[derive(Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for DecodedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Decode `bytes` for display. Seconds of CPU on a large photo, so callers
/// run it on a blocking thread.
pub fn decode(kind: BlobKind, bytes: &[u8]) -> Result<DecodedImage, String> {
    match kind {
        BlobKind::Binary => Err("not an image".to_owned()),
        BlobKind::Image => decode_raster(bytes),
        BlobKind::Svg => std::panic::catch_unwind(|| rasterize_svg(bytes))
            .unwrap_or_else(|_| Err("the SVG renderer crashed".to_owned())),
    }
}

fn decode_raster(bytes: &[u8]) -> Result<DecodedImage, String> {
    let (width, height) =
        dimensions(BlobKind::Image, bytes).ok_or_else(|| "unreadable image".to_owned())?;
    if u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS {
        return Err(format!("{width} × {height} is too large to preview"));
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    reader.no_limits();
    let image = reader.decode().map_err(|error| error.to_string())?;
    let (fit_width, fit_height) = fit_within(width, height, MAX_DECODED_SIDE);
    let image = if (fit_width, fit_height) == (width, height) {
        image
    } else {
        image.resize_exact(fit_width, fit_height, image::imageops::FilterType::Triangle)
    };
    let rgba = image.into_rgba8();
    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

fn rasterize_svg(bytes: &[u8]) -> Result<DecodedImage, String> {
    let tree = usvg::Tree::from_data(bytes, &svg_options()).map_err(|error| error.to_string())?;
    let size = tree.size();
    let longest = size.width().max(size.height()).max(1.0);
    let scale = SVG_RASTER_SCALE.min(MAX_DECODED_SIDE as f32 / longest);
    let width = (size.width() * scale).ceil().max(1.0) as u32;
    let height = (size.height() * scale).ceil().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| "the SVG has no area".to_owned())?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect();
    Ok(DecodedImage {
        width,
        height,
        rgba,
    })
}

/// Parse options with the system's fonts, so text in an SVG resolves its
/// families (and the parse doesn't warn about every one it can't).
fn svg_options() -> usvg::Options<'static> {
    usvg::Options {
        fontdb: Arc::clone(system_fonts()),
        ..usvg::Options::default()
    }
}

/// The system's fonts, loaded once: SVG text renders through them.
fn system_fonts() -> &'static Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut database = usvg::fontdb::Database::new();
        database.load_system_fonts();
        Arc::new(database)
    })
}

/// `(width, height)` scaled down, keeping the aspect ratio, so neither side
/// exceeds `max`. Never scales up.
pub fn fit_within(width: u32, height: u32, max: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= max {
        return (width, height);
    }
    let scale = f64::from(max) / f64::from(longest);
    let scaled = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    (scaled(width), scaled(height))
}

/// A byte count for people: `512 B`, `12.4 KB`, `3.1 MB` (powers of 1024,
/// as the source browser shows them).
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KB * KB * KB {
        format!("{:.1} GB", value / (KB * KB * KB))
    } else if value >= KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// The signed difference `after - before`, sized: `+1.2 KB`, `-300 B`.
/// `None` when the two are equal.
pub fn format_size_delta(before: u64, after: u64) -> Option<String> {
    match after.cmp(&before) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(format!("+{}", format_size(after - before))),
        std::cmp::Ordering::Less => Some(format!("-{}", format_size(before - after))),
    }
}

/// A file's sizes across the change: `12.4 KB → 13.0 KB`, or the one size
/// an added or deleted file has.
pub fn size_span(before: Option<u64>, after: Option<u64>) -> String {
    match (before, after) {
        (Some(before), Some(after)) if before == after => {
            format!("{}, size unchanged", format_size(after))
        }
        (Some(before), Some(after)) => {
            format!("{} → {}", format_size(before), format_size(after))
        }
        (Some(size), None) | (None, Some(size)) => format_size(size),
        (None, None) => String::new(),
    }
}

/// The one line a binary file's diff shows in place of rows:
/// `Binary file · 12.4 KB → 13.0 KB`.
pub fn summary_line(label: &str, before: Option<u64>, after: Option<u64>) -> String {
    match size_span(before, after) {
        span if span.is_empty() => label.to_owned(),
        span => format!("{label} · {span}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, alpha]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode png");
        bytes
    }

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="20" height="20" fill="#00f"/></svg>"##;

    #[test]
    fn kinds_follow_extension_then_bytes() {
        assert_eq!(kind_of("a/logo.SVG", false, b"<svg"), Some(BlobKind::Svg));
        assert_eq!(
            kind_of("icon.png", true, &png(1, 1, 255)),
            Some(BlobKind::Image)
        );
        // A misnamed image still previews; an unknown binary doesn't.
        assert_eq!(
            kind_of("blob.dat", true, &png(1, 1, 255)),
            Some(BlobKind::Image)
        );
        assert_eq!(
            kind_of("blob.dat", true, b"\0\x01\x02"),
            Some(BlobKind::Binary)
        );
        assert_eq!(kind_of("main.rs", false, b"fn main() {}"), None);
        // A gzipped SVG is binary, not markup.
        assert_eq!(
            kind_of("a.svg", true, b"\x1f\x8b\0"),
            Some(BlobKind::Binary)
        );
    }

    #[test]
    fn dimensions_read_headers_and_svg_roots() {
        assert_eq!(dimensions(BlobKind::Image, &png(7, 3, 255)), Some((7, 3)));
        assert_eq!(dimensions(BlobKind::Svg, SVG.as_bytes()), Some((40, 20)));
        assert_eq!(dimensions(BlobKind::Image, b"not an image"), None);
    }

    #[test]
    fn sides_keep_bytes_only_when_previewable() {
        let bytes = png(2, 2, 255);
        let kept = side(BlobKind::Image, "abc123", &bytes, true);
        assert_eq!(kept.size, bytes.len() as u64);
        assert_eq!(kept.dimensions, Some((2, 2)));
        assert!(kept.bytes.is_some());
        assert!(
            side(BlobKind::Image, "abc123", &bytes, false)
                .bytes
                .is_none()
        );
        // A conflict's dummy id names no content to cache a decode under.
        assert!(
            side(BlobKind::Image, "0000000000", &bytes, true)
                .bytes
                .is_none()
        );
        assert!(
            side(BlobKind::Image, "abc123", b"junk", true)
                .bytes
                .is_none()
        );
    }

    #[test]
    fn decode_keeps_transparency_straight() {
        let decoded = decode(BlobKind::Image, &png(3, 2, 128)).expect("decode");
        assert_eq!((decoded.width, decoded.height), (3, 2));
        assert_eq!(&decoded.rgba[..4], &[255, 0, 0, 128]);
    }

    #[test]
    fn svg_rasterizes_at_twice_its_size_demultiplied() {
        let decoded = decode(BlobKind::Svg, SVG.as_bytes()).expect("rasterize");
        assert_eq!((decoded.width, decoded.height), (80, 40));
        // Left half is the opaque blue rect, right half untouched.
        assert_eq!(&decoded.rgba[..4], &[0, 0, 255, 255]);
        let right = (79 * 4) as usize;
        assert_eq!(decoded.rgba[right + 3], 0);
    }

    #[test]
    fn large_images_scale_down_to_the_cap() {
        assert_eq!(fit_within(4096, 1024, 2048), (2048, 512));
        assert_eq!(fit_within(100, 50, 2048), (100, 50));
        let decoded = decode(BlobKind::Image, &png(MAX_DECODED_SIDE * 2, 4, 255)).expect("decode");
        assert_eq!((decoded.width, decoded.height), (MAX_DECODED_SIDE, 2));
    }

    #[test]
    fn sizes_read_tersely() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(12_700), "12.4 KB");
        assert_eq!(format_size(3 * 1024 * 1024 + 200_000), "3.2 MB");
        assert_eq!(format_size_delta(1000, 1000), None);
        assert_eq!(format_size_delta(1024, 3072).as_deref(), Some("+2.0 KB"));
        assert_eq!(format_size_delta(400, 100).as_deref(), Some("-300 B"));
        assert_eq!(
            summary_line("Binary file", Some(1024), Some(2048)),
            "Binary file · 1.0 KB → 2.0 KB"
        );
        assert_eq!(
            summary_line("Binary file", None, Some(10)),
            "Binary file · 10 B"
        );
        assert_eq!(
            summary_line("Binary file", Some(10), Some(10)),
            "Binary file · 10 B, size unchanged"
        );
    }
}

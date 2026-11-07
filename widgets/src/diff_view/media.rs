//! Rendered previews of a file: the types the app fills in. Painting them is
//! the diff lane's.

use std::sync::Arc;

/// A rendered preview of a file, either side of a diff.
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

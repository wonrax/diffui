//! Image and SVG previews in the diff: the decoded pixels, shared by every
//! tab and diff source and keyed by blob id, and the per-file
//! [`diff_view::Media`] the diff widget draws from them.
//!
//! The core loads each previewable side's bytes with the diff; decoding is
//! seconds of CPU on a large photo, so it runs on a thread of its own, one
//! side at a time, for the documents the active tab shows.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use diffui_core::{BlobChange, BlobKind, BlobSide, DiffDocument, DiffFile};
use iced::Task;
use iced::widget::image::Handle;

use crate::diff_view::{Media, MediaImage, MediaSide};
use crate::{Diffui, Message, UiEvent};

/// One side's decode, by blob id.
#[derive(Debug, Clone)]
enum Entry {
    Pending,
    Ready(Handle),
    Failed,
}

/// A finished decode, carried back to the reducer.
#[derive(Debug, Clone)]
pub(crate) struct Decoded {
    id: String,
    result: Result<Handle, String>,
}

#[derive(Debug, Default)]
pub(crate) struct MediaCache {
    entries: HashMap<String, Entry>,
    /// The active tab's documents as of the last scan: a new id there is
    /// what starts decodes.
    scanned: Option<(Option<crate::TabId>, u64, Option<u64>)>,
}

impl MediaCache {
    fn image(&self, side: &BlobSide) -> MediaImage {
        if side.bytes.is_none() {
            return MediaImage::Unavailable;
        }
        match self.entries.get(&side.id) {
            Some(Entry::Ready(handle)) => MediaImage::Ready(handle.clone()),
            Some(Entry::Failed) => MediaImage::Unavailable,
            Some(Entry::Pending) | None => MediaImage::Loading,
        }
    }
}

/// The preview `file` shows in place of its rows, if any: an image with a
/// side to show, or an SVG while the rendered view is on.
pub(crate) fn file_media(ui: &Diffui, file: &DiffFile) -> Option<Media> {
    let blob = file.blob.as_deref()?;
    match blob.kind {
        BlobKind::Binary => return None,
        BlobKind::Svg if ui.svg_source => return None,
        BlobKind::Image | BlobKind::Svg => {}
    }
    if !blob.previewable() {
        return None;
    }
    let side = |side: &BlobSide| MediaSide {
        size: side.size,
        dimensions: side.dimensions,
        image: ui.media.image(side),
    };
    Some(Media {
        before: blob.before.as_ref().map(side),
        after: blob.after.as_ref().map(side),
    })
}

/// [`file_media`] for every file of `document`, in order, plus the key the
/// diff widget lays its previews out by.
pub(crate) fn document_media(
    ui: &Diffui,
    document: &DiffDocument,
) -> Option<(Vec<Option<Media>>, u64)> {
    let media: Vec<Option<Media>> = document
        .files
        .iter()
        .map(|file| file_media(ui, file))
        .collect();
    media
        .iter()
        .any(Option::is_some)
        .then_some((media, u64::from(ui.svg_source)))
}

/// Whether the diff on screen holds an SVG that can show rendered: what puts
/// the rendered/markup switch in the toolbar.
pub(crate) fn shows_svg_preview(ui: &Diffui) -> bool {
    let tab = ui.active();
    match tab.main_view {
        crate::MainView::Diff => has_svg_preview(&tab.session.document),
        crate::MainView::History => tab
            .history
            .diff_document()
            .is_some_and(|(document, _)| has_svg_preview(document)),
        crate::MainView::Source => false,
    }
}

fn has_svg_preview(document: &DiffDocument) -> bool {
    document.files.iter().any(|file| {
        file.blob
            .as_deref()
            .is_some_and(|blob| blob.kind == BlobKind::Svg && blob.previewable())
    })
}

fn previewable_sides(document: &DiffDocument) -> impl Iterator<Item = (BlobKind, &BlobSide)> {
    document
        .files
        .iter()
        .filter_map(|file| file.blob.as_deref())
        .flat_map(|blob: &BlobChange| {
            [&blob.before, &blob.after]
                .into_iter()
                .flatten()
                .filter(|side| side.bytes.is_some())
                .map(move |side| (blob.kind, side))
        })
}

impl Diffui {
    /// Start decoding what the active tab's diffs show and haven't decoded,
    /// and drop the decodes no open tab shows any more. Cheap when nothing
    /// changed: the active tab's document ids are compared first.
    pub(crate) fn sync_media(&mut self) -> Task<Message> {
        let tab = self.active();
        let history = tab.history.diff_document();
        let key = (
            self.active_tab_id(),
            tab.session.document_id,
            history.map(|(_, id)| id),
        );
        if self.media.scanned == Some(key) {
            return Task::none();
        }
        let mut jobs: Vec<(String, BlobKind, Arc<[u8]>)> = Vec::new();
        let shown = std::iter::once(&tab.session.document).chain(history.map(|(doc, _)| doc));
        for (kind, side) in shown.flat_map(previewable_sides) {
            let known = self.media.entries.contains_key(&side.id)
                || jobs.iter().any(|(id, ..)| id == &side.id);
            if let (false, Some(bytes)) = (known, &side.bytes) {
                jobs.push((side.id.clone(), kind, Arc::clone(bytes)));
            }
        }
        self.media.scanned = Some(key);

        // Keep what any open tab still shows; a reload or a closed tab
        // otherwise leaves its pixels behind for good.
        let live: HashSet<&str> = self
            .tabs
            .iter()
            .flat_map(|tab| {
                std::iter::once(&tab.state.session.document)
                    .chain(tab.state.history.diff_document().map(|(doc, _)| doc))
            })
            .flat_map(previewable_sides)
            .map(|(_, side)| side.id.as_str())
            .collect();
        self.media
            .entries
            .retain(|id, _| live.contains(id.as_str()));

        if jobs.is_empty() {
            return Task::none();
        }
        for (id, ..) in &jobs {
            self.media.entries.insert(id.clone(), Entry::Pending);
        }
        decode(jobs)
    }

    pub(crate) fn on_media_decoded(&mut self, decoded: Decoded) -> Task<Message> {
        // Dropped meanwhile (a reload replaced the document): don't revive it.
        if let Some(entry) = self.media.entries.get_mut(&decoded.id) {
            *entry = match decoded.result {
                Ok(handle) => Entry::Ready(handle),
                Err(error) => {
                    tracing::debug!(id = %decoded.id, %error, "preview decode failed");
                    Entry::Failed
                }
            };
        }
        Task::none()
    }
}

/// Decode `jobs` one after another on a thread of their own, reporting each
/// as it lands so the first previews paint while the rest decode.
fn decode(jobs: Vec<(String, BlobKind, Arc<[u8]>)>) -> Task<Message> {
    let (sender, receiver) = futures::channel::mpsc::unbounded();
    std::thread::Builder::new()
        .name("diffui-media".to_owned())
        .spawn(move || {
            for (id, kind, bytes) in jobs {
                let result = diffui_core::media::decode(kind, &bytes)
                    .map(|image| Handle::from_rgba(image.width, image.height, image.rgba));
                if sender.unbounded_send(Decoded { id, result }).is_err() {
                    break;
                }
            }
        })
        .map_or_else(
            |error| {
                tracing::warn!(%error, "could not start the preview decoder");
                Task::none()
            },
            |_| {
                Task::run(receiver, |decoded| {
                    Message::Ui(UiEvent::MediaDecoded(Box::new(decoded)))
                })
            },
        )
}

//! Labelled placeholders for views a lane hasn't ported yet, sized like the real
//! thing so the root layout already reads true. Delete a call when its view is
//! real; delete this module with the last one.

use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, div, px};

use crate::typography::Aligned;

const COLOR: u32 = 0x808080;

fn tag(label: &'static str) -> gpui_kit::Div {
    div()
        .text_xs()
        .text_color(gpui_kit::rgb(COLOR))
        .child(label)
}

/// Fills what it's given: a pane.
pub(crate) fn panel(label: &'static str) -> AnyElement {
    div()
        .id(label)
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .child(tag(label))
        .into_any_element()
}

/// A strip of a fixed height.
pub(crate) fn bar(label: &'static str, height: f32) -> AnyElement {
    div()
        .id(label)
        .w_full()
        .h(px(height))
        .flex_none()
        .flex()
        .items_center()
        .px(px(12.0))
        .child(tag(label))
        .into_any_element()
}

/// A floating layer, on screen only while `active`.
pub(crate) fn overlay(label: &'static str, active: bool) -> AnyElement {
    if !active {
        return div().into_any_element();
    }
    div()
        .id(label)
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .justify_center()
        .pt(px(96.0))
        .child(
            div()
                .w(px(460.0))
                .h(px(120.0))
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::rgb(0x202020))
                .child(tag(label)),
        )
        .into_any_element()
}

/// A run on a shared baseline.
pub(crate) fn aligned(label: &'static str) -> Aligned {
    Aligned::text(tag(label), crate::theme::Font::DEFAULT, 12.0)
}

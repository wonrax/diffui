//! The app's widget layer: the diff and revision-list widgets, the theme and
//! typography they draw with, and the controls the panels share. Nothing here
//! knows the app's state; widgets take plain callbacks, so the app can change
//! without rebuilding this crate.

pub mod chip;
pub mod chrome;
pub mod diff_view;
pub mod ellipsis;
pub mod frame_log;
pub mod geometry;
pub mod graph_view;
pub mod hunk_actions;
pub mod icons;
pub mod input;
pub mod keys;
pub mod measure;
pub mod prose;
pub mod resize_handle;
pub mod revision_list;
pub mod scroll_resample;
pub mod scrollbar;
pub mod status_badge;
pub mod theme;
pub mod typography;
pub mod ui;
pub mod wheel;
pub mod width_memo;

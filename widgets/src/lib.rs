//! The app's widget layer: the diff and revision-list widgets, the theme and
//! typography they draw with, and the controls the panels share. Nothing here
//! knows the app's state or message types; widgets that emit messages are
//! generic over them, so the app can change without rebuilding this crate.

pub mod chip;
pub mod chrome;
pub mod diff_view;
pub mod editor_drag;
pub mod ellipsis;
pub mod graph_view;
pub mod hunk_actions;
pub mod icons;
pub mod input;
pub mod measure;
pub mod pointer;
pub mod prose;
pub mod resize_handle;
pub mod revision_list;
pub mod scrollbar;
pub mod theme;
pub mod typography;
pub mod ui;
pub mod wheel;
pub mod width_memo;

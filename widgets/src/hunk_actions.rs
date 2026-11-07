use crate::theme::{Color, Font};

/// One run of a button's label.
#[derive(Debug, Clone)]
pub struct Run {
    pub text: String,
    pub font: Font,
    pub color: Color,
}

/// A button: its label runs. The first run is kept when the strip is too
/// narrow for the whole label.
#[derive(Debug, Clone)]
pub struct Button {
    pub runs: Vec<Run>,
}

/// The buttons of one hunk header.
#[derive(Debug, Clone)]
pub struct Row {
    pub file: usize,
    pub hunk: usize,
    /// The keyboard's region: a bar marks the strip.
    pub focused: bool,
    /// A word before the buttons ("Take").
    pub lead: Option<Run>,
    pub buttons: Vec<Button>,
}

/// A ghost button (no fill or border at rest) following a file strip's
/// text. The first run is kept when the strip is too narrow for the whole
/// label.
#[derive(Debug, Clone)]
pub struct Tool {
    pub runs: Vec<Run>,
    pub enabled: bool,
}

/// The strip of its own a file gets under its header, above its first
/// hunk: what a conflicted file's whole-file controls ride on.
#[derive(Debug, Clone)]
pub struct FileStrip {
    pub file: usize,
    /// Its text, at the left where a hunk strip has its header.
    pub label: String,
    pub tools: Vec<Tool>,
    /// The right-aligned buttons, as on a hunk strip (`hunk` unused).
    pub row: Row,
}

/// Colours and type, resolved by the caller from the theme.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub size: f32,
    pub fill: Color,
    pub hover_fill: Color,
    /// A hovered tool's fill.
    pub ghost_hover_fill: Color,
    pub border: Color,
    pub focus: Color,
    pub radius: f32,
}

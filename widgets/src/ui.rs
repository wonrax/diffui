//! The shared control kit: every chrome button, icon button and tooltip is
//! built here, so heights, insets, icon sizes and label sizes come from one
//! table instead of being re-derived (and drifting) per call site.
//!
//! The styles themselves stay in [`crate::theme`]; this module owns the
//! geometry. A control's height is always one of the fixed [`Size`] steps —
//! never a text line box plus padding, whose fractional height made
//! neighbouring controls disagree by a pixel and snap unevenly.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button as KitButton, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::scroll::{Scrollable, ScrollableElement as _};
use gpui_kit::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{
    App, Bounds, Context, Div, ElementId, Entity, MouseButton, Pixels, ScrollHandle, SharedString,
    Stateful, Window, anchored, canvas, div, point, px,
};

use crate::icons;
use crate::squircle;
use crate::theme::{self, Color, Font, ThemeSpec, Weight, text_size};
use crate::typography;

/// Control height step. Everything sharing a row should share a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// Controls inside a row, a banner or a card: close and remove marks,
    /// a toast's or an activity row's actions.
    Small,
    /// Toolbars, panel headers, inline editors, find bar.
    Compact,
    /// Dialog footers, empty-state actions.
    Standard,
}

impl Size {
    pub fn height(self) -> f32 {
        match self {
            Size::Small => theme::control::SMALL,
            Size::Compact => theme::control::COMPACT,
            Size::Standard => theme::control::STANDARD,
        }
    }

    /// Horizontal inset of a labelled button.
    pub fn pad_x(self) -> f32 {
        match self {
            Size::Small => theme::space::MD,
            Size::Compact => 10.0,
            Size::Standard => 14.0,
        }
    }

    pub fn text_size(self) -> f32 {
        match self {
            Size::Small => text_size::CAPTION,
            Size::Compact => text_size::BODY,
            Size::Standard => text_size::BODY,
        }
    }

    /// Glyph size of a leading icon or an icon button's mark.
    pub fn icon_size(self) -> f32 {
        match self {
            Size::Small => SMALL_ICON_SIZE,
            Size::Compact | Size::Standard => ICON_SIZE,
        }
    }

    /// Corner radius: a small control is rounded like a chip.
    pub fn radius(self) -> f32 {
        match self {
            Size::Small => theme::radius::XS,
            Size::Compact | Size::Standard => theme::radius::CONTROL,
        }
    }
}

/// Glyph size for an icon that leads a label or fills an icon button. A step
/// over the 12px label because Lucide's 24px grid carries ~2px of internal
/// padding, so at equal size the mark reads smaller than the text beside it.
pub const ICON_SIZE: f32 = 14.0;
/// [`ICON_SIZE`] for a [`Size::Small`] control, a step over its 11px label.
const SMALL_ICON_SIZE: f32 = 12.0;
/// How much of a disabled button's label and icon shows.
const DISABLED_OPACITY: f32 = 0.45;
/// Gap between a leading icon and its label.
const ICON_GAP: f32 = 6.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Invisible at rest: for controls that sit on a framed surface.
    Ghost,
    /// Visible at rest: standalone secondary actions, and every Cancel.
    Raised,
    /// The one call-to-action of a surface.
    Primary,
    /// A confirm that destroys or overrides something.
    Destructive,
}

fn style(variant: Variant, theme: ThemeSpec) -> theme::ButtonStyle {
    match variant {
        Variant::Ghost => theme::ghost_button_style(theme),
        Variant::Raised => theme::raised_button_style(theme),
        Variant::Primary => theme::primary_button_style(theme),
        Variant::Destructive => theme::destructive_button_style(theme),
    }
}

/// kit's button in one of diffui's variants at one of its height steps. Add the
/// content with `.child(...)` ([`label`] for the common case) and the action
/// with `.on_click(...)`.
pub fn button(
    id: impl Into<ElementId>,
    variant: Variant,
    size: Size,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    frame(id, style(variant, theme), size, cx)
        .h(px(size.height()))
        .px(px(size.pad_x()))
}

fn frame(id: impl Into<ElementId>, style: theme::ButtonStyle, size: Size, cx: &App) -> Button {
    let id = id.into();
    // kit paints nothing of its own: its fill would be a circular quad. The
    // squircle behind the label paints each state instead.
    let clear = gpui_kit::Hsla::from(theme::TRANSPARENT);
    let colors = ButtonCustomVariant::new(cx)
        .color(clear)
        .foreground(style.label.into())
        .hover(clear)
        .active(clear);
    let mut inner = KitButton::new(id.clone())
        .custom(colors)
        .bg(theme::TRANSPARENT)
        // The lift's shape: a shadow's blur hides its circular corner.
        .rounded(px(size.radius()));
    if let Some(lift) = style.lift {
        inner = inner.shadow(vec![lift]);
    }
    let fill = |color: Option<Color>| {
        let mut fill = squircle::Fill::new(squircle::Corners::all(size.radius()));
        if let Some(color) = color {
            fill = fill.background(color);
        }
        if let Some(ring) = style.ring {
            fill = fill.border(1.0, ring);
        }
        fill
    };
    Button {
        inner,
        id,
        states: squircle::States {
            rest: fill(style.rest),
            hover: fill(Some(style.hover)),
            active: fill(Some(style.active)),
        },
        disabled: false,
        tip: None,
        children: Vec::new(),
    }
}

/// A diffui button: kit's button for what it does (clicks, focus, tooltips,
/// accessibility), drawn as a squircle. kit lays its children out inside its
/// padding and clips them there, so the button's padding moves onto a row of
/// its own when it renders, and the squircle, its first child, covers the
/// whole box.
#[derive(IntoElement)]
pub struct Button {
    inner: KitButton,
    id: ElementId,
    states: squircle::States,
    disabled: bool,
    tip: Option<(SharedString, Font, ThemeSpec)>,
    children: Vec<gpui_kit::AnyElement>,
}

impl Button {
    pub fn on_click(
        mut self,
        handler: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.inner = self.inner.on_click(handler);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self.inner = self.inner.disabled(disabled);
        self
    }

    /// The app's hover label ([`Tip`]) on this button.
    pub fn tip(mut self, label: impl Into<SharedString>, font: Font, theme: ThemeSpec) -> Self {
        self.tip = Some((label.into(), font, theme));
        self
    }

    /// Rounds the button by `radius` in place of its size's: a segment's face,
    /// concentric with its track.
    pub fn corner_radius(mut self, radius: f32) -> Self {
        let corners = squircle::Corners::all(radius);
        self.states.rest.corners = corners;
        self.states.hover.corners = corners;
        self.states.active.corners = corners;
        self.inner = self.inner.rounded(px(radius));
        self
    }

    /// The fill and edge at rest, in place of the variant's: a toggle while
    /// it's on. Hover and press keep the variant's.
    pub fn resting(mut self, fill: Color, ring: Option<Color>) -> Self {
        let mut rest = squircle::Fill::new(self.states.rest.corners).background(fill);
        if let Some(ring) = ring {
            rest = rest.border(1.0, ring);
        }
        self.states.rest = rest;
        self
    }

    /// [`Button::resting`] raised out of its track, as the active segment of a
    /// segmented control is.
    pub fn raised(self, theme: ThemeSpec) -> Self {
        self.resting(
            theme.panel_background_elevated,
            Some(theme::firm_ring(theme)),
        )
        .shadow(vec![theme::raised_lift()])
    }
}

impl Styled for Button {
    fn style(&mut self) -> &mut gpui_kit::StyleRefinement {
        self.inner.style()
    }
}

impl ParentElement for Button {
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui_kit::AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let mut inner = self.inner;
        let padding = std::mem::take(&mut inner.style().padding);
        // An explicit zero: left unset, kit pads the button for its size.
        let inner = inner.p(px(0.0));
        let mut row = div()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_2()
            .children(self.children);
        row.style().padding = padding;
        // A disabled button doesn't answer the pointer, and its label and
        // icon fade.
        if self.disabled {
            row = row.opacity(DISABLED_OPACITY);
        }
        let states = if self.disabled {
            squircle::States {
                hover: self.states.rest,
                active: self.states.rest,
                ..self.states
            }
        } else {
            self.states
        };
        let id = std::sync::Arc::new(self.id);
        let button = inner
            .child(squircle::interactive_background(
                ElementId::NamedChild(id.clone(), "squircle".into()),
                states,
                0.0,
            ))
            .child(row);
        // kit's button takes only kit's tooltip, so the app's hangs off a
        // wrapper the size of the button.
        match self.tip {
            Some((label, font, theme)) => div()
                .id(ElementId::NamedChild(id, "tip".into()))
                .flex_none()
                .tip(label, font, theme)
                .child(button)
                .into_any_element(),
            None => button.into_any_element(),
        }
    }
}

/// An optional leading icon and a label, sized for `size`. The building block
/// of [`text_button`], exposed for buttons that dress the label themselves.
pub fn label(
    icon: Option<(&'static str, Color)>,
    label: impl Into<SharedString>,
    size: Size,
    font: Font,
) -> Div {
    // Labels sit a weight over body text, as the prototype's buttons do.
    let font = theme::emphasis_font(font, Weight::MEDIUM);
    let text = div()
        .font(font.gpui())
        .text_size(px(size.text_size()))
        .line_height(px(size.text_size() * typography::DEFAULT_LINE_HEIGHT))
        .whitespace_nowrap()
        .child(label.into());
    // Optically centred: the control centres the caps, not the line box.
    let text = typography::centered(text, font, size.text_size());
    let row = div().flex().flex_row().items_center().gap(px(ICON_GAP));
    match icon {
        Some((glyph, color)) => row
            .child(icons::icon(glyph, size.icon_size(), color))
            .child(text),
        None => row.child(text),
    }
}

/// The common button: an optional icon and a label.
#[allow(clippy::too_many_arguments)]
pub fn text_button(
    id: impl Into<ElementId>,
    icon: Option<(&'static str, Color)>,
    text: impl Into<SharedString>,
    variant: Variant,
    size: Size,
    font: Font,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    button(id, variant, size, theme, cx).child(label(icon, text, size, font))
}

/// [`text_button`] in the primary variant, greyed out and inert unless
/// `enabled`.
pub fn primary_button(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    enabled: bool,
    size: Size,
    font: Font,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    let colors = if enabled {
        theme::primary_button_style(theme)
    } else {
        theme::disabled_button_style(theme)
    };
    frame(id, colors, size, cx)
        .h(px(size.height()))
        .px(px(size.pad_x()))
        .disabled(!enabled)
        .child(label(None, text, size, font))
}

/// Gap between buttons sitting side by side, in dialogs, editors and bars.
pub const BUTTON_GAP: f32 = theme::space::SM;

/// The Cancel of a dialog or editor footer: always the raised secondary
/// button, beside the footer's one filled call-to-action.
pub fn cancel_button(
    id: impl Into<ElementId>,
    size: Size,
    font: Font,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    text_button(id, None, "Cancel", Variant::Raised, size, font, theme, cx)
}

/// A square ghost button holding one glyph.
pub fn icon_button(
    id: impl Into<ElementId>,
    glyph: &'static str,
    color: Color,
    size: Size,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    frame(id, theme::ghost_button_style(theme), size, cx)
        .size(px(size.height()))
        .p(px(0.0))
        .child(icons::icon(glyph, size.icon_size(), color))
}

/// The close (or clear) ✕ at the end of a banner, strip or row: a
/// [`Size::Small`] square wash, so it sits inside the banner with even
/// insets instead of filling its height.
pub fn close_button(id: impl Into<ElementId>, color: Color, theme: ThemeSpec, cx: &App) -> Button {
    icon_button(id, icons::CLOSE, color, Size::Small, theme, cx)
}

/// Side of [`close_button`]'s square.
pub const CLOSE_SIDE: f32 = theme::control::SMALL;

// ── Segmented control ───────────────────────────────────────────────────

/// A segmented control's height step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segmented {
    /// On a bar, beside compact controls: the toolbar's view switcher.
    Compact,
    /// Inside a card, under its input: the palette's scope tabs.
    Small,
}

impl Segmented {
    fn segment_height(self) -> f32 {
        self.well_height() - 2.0 * SEGMENT_INSET
    }

    fn well_height(self) -> f32 {
        match self {
            Self::Compact => theme::control::COMPACT,
            Self::Small => 24.0,
        }
    }

    fn text_size(self) -> f32 {
        match self {
            Self::Compact => text_size::BODY,
            Self::Small => text_size::CAPTION,
        }
    }

    fn icon_size(self) -> f32 {
        match self {
            Self::Compact => theme::icon::MD,
            Self::Small => theme::icon::SM,
        }
    }
}

/// The well's inset around its segments, and the gap between them.
const SEGMENT_INSET: f32 = theme::space::XXS;
/// A segment's text inset from its face.
const SEGMENT_PAD_X: f32 = theme::space::MD;

/// The well a run of segments sits in, a step into the surface under it. Add
/// [`segment`]s as its children.
pub fn segmented(theme: ThemeSpec) -> Div {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .p(px(SEGMENT_INSET))
        .gap(px(SEGMENT_INSET))
        .map(|well| {
            theme::squircle(
                well,
                theme::radius::CARD,
                Some(theme::well_wash(theme)),
                None,
            )
        })
}

/// One segment of a [`segmented`] control.
pub struct Segment {
    pub icon: Option<&'static str>,
    pub label: SharedString,
    /// A count after the label, and whether it shows: a hidden one keeps
    /// its room, so the segments don't move when it comes and goes.
    pub count: Option<(usize, bool)>,
    pub active: bool,
    pub enabled: bool,
}

/// A segment: a quiet label in the well, raised out of it as a ringed face
/// while `active`. Add the action with `.on_click`.
pub fn segment(
    id: impl Into<ElementId>,
    segment: Segment,
    size: Segmented,
    font: Font,
    theme: ThemeSpec,
    cx: &App,
) -> Button {
    let label_font = theme::emphasis_font(font, Weight::MEDIUM);
    let text = size.text_size();
    let ink = if segment.active {
        theme.text
    } else if segment.enabled {
        theme.muted_text
    } else {
        theme.subtle_text
    };
    let mut line = Vec::new();
    if let Some(icon) = segment.icon {
        line.push(typography::Aligned::glyph(
            icons::icon(icon, size.icon_size(), ink),
            size.icon_size(),
        ));
        line.push(typography::Aligned::gap(ICON_GAP));
    }
    line.push(typography::Aligned::text(
        typography::line(segment.label, label_font, text, ink),
        label_font,
        text,
    ));
    let mut pad_x = SEGMENT_PAD_X;
    if let Some((count, shown)) = segment.count {
        let count = count.to_string();
        if shown {
            line.push(typography::Aligned::gap(ICON_GAP));
            line.push(typography::Aligned::text(
                typography::line(count, font, text, theme.subtle_text),
                font,
                text,
            ));
        } else {
            // Half the room to each side, the label centred in it.
            pad_x += (ICON_GAP + crate::measure::line_width(&count, text, font)) / 2.0;
        }
    }
    let face = theme::radius::inner(theme::radius::CARD, SEGMENT_INSET);
    button(id, Variant::Ghost, Size::Compact, theme, cx)
        .h(px(size.segment_height()))
        .px(px(pad_x))
        .corner_radius(face)
        .disabled(!segment.enabled)
        .when(segment.active, |segment| segment.raised(theme))
        .child(typography::baseline_row(line, 0.0))
}

// ── Panes ───────────────────────────────────────────────────────────────

/// A pane's header row: [`theme::PANE_HEADER_HEIGHT`] tall, its content in
/// one centred row. The caller sets the insets, which line its content up
/// with the pane's own, and a ground where the list scrolls under it.
pub fn panel_header() -> Div {
    div()
        .w_full()
        .flex_none()
        .h(px(theme::PANE_HEADER_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
}

/// A pane header's title, on its baseline: semibold in the text's ink, and
/// the count after it, if any, in regular subtle text ("Changed files 12").
pub fn panel_title(
    title: impl Into<SharedString>,
    count: Option<String>,
    font: Font,
    theme: ThemeSpec,
) -> typography::Aligned {
    let title_font = theme::emphasis_font(font, Weight::SEMIBOLD);
    let size = text_size::BODY;
    let mut items = vec![
        typography::Aligned::text(
            typography::line(title, title_font, size, theme.text).text_ellipsis(),
            title_font,
            size,
        )
        .shrink(),
    ];
    if let Some(count) = count {
        items.push(typography::Aligned::text(
            typography::line(count, font, size, theme.subtle_text),
            font,
            size,
        ));
    }
    typography::group(items, theme::space::SM)
}

/// A section's label inside a pane, a list or a sheet: sentence case,
/// semibold, muted, with an optional count in regular subtle text.
pub fn section_label(
    label: impl Into<SharedString>,
    count: Option<String>,
    font: Font,
    theme: ThemeSpec,
) -> typography::Aligned {
    let label_font = theme::emphasis_font(font, Weight::SEMIBOLD);
    let size = text_size::BODY;
    let mut items = vec![typography::Aligned::text(
        typography::line(label, label_font, size, theme.muted_text),
        label_font,
        size,
    )];
    if let Some(count) = count {
        items.push(typography::Aligned::text(
            typography::line(count, font, size, theme.subtle_text),
            font,
            size,
        ));
    }
    typography::group(items, theme::space::SM)
}

// ── Lists ───────────────────────────────────────────────────────────────

/// A list row's shell: its height, its text inset from the wash, its corner,
/// and whether it washes under the cursor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListRow {
    /// `None`: the content's height, for rows that wrap.
    pub height: Option<f32>,
    pub pad_x: f32,
    pub radius: f32,
    /// Off where the pointer moves the selection itself (the palette), so a
    /// hovered row and the selected one never show at once.
    pub hover: bool,
}

impl ListRow {
    /// A row `height` tall, rounded for its height, washing under the cursor.
    pub const fn new(height: f32, pad_x: f32) -> Self {
        Self {
            height: Some(height),
            pad_x,
            radius: theme::list::radius(height),
            hover: true,
        }
    }

    /// A row as tall as its content, rounded by `radius`.
    pub const fn fit(pad_x: f32, radius: f32) -> Self {
        Self {
            height: None,
            pad_x,
            radius,
            hover: true,
        }
    }

    pub const fn radius(self, radius: f32) -> Self {
        Self { radius, ..self }
    }

    pub const fn no_hover(self) -> Self {
        Self {
            hover: false,
            ..self
        }
    }
}

/// A list row: a fixed-height flex row with its content centred and inset,
/// the selection's squircle when `selected`, and the hover's otherwise. Add
/// the content with `.child`, and an id and handlers as the list needs.
pub fn list_row(row: ListRow, selected: bool, theme: ThemeSpec) -> Div {
    let shell = div()
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .px(px(row.pad_x))
        .cursor_pointer();
    let shell = match row.height {
        Some(height) => shell.h(px(height)),
        None => shell,
    };
    if selected {
        theme::squircle(
            shell,
            row.radius,
            Some(theme::list::row_selected(theme)),
            None,
        )
    } else if row.hover {
        shell.child(squircle::hover_background(
            squircle::Fill::new(squircle::Corners::all(row.radius))
                .background(theme::list::row_hover(theme)),
            0.0,
        ))
    } else {
        shell
    }
}

// ── Empty states ────────────────────────────────────────────────────────

/// Where an [`empty_state`] sits in the box it stands in for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// In the middle of a pane.
    Centered,
    /// Centred across a list, near its top.
    Top,
    /// A line in a list, on its text edge.
    Inline,
}

/// What a pane or a list says when it has nothing to show: one quiet
/// message, wrapping in a narrow box rather than running out of it. Callers
/// add actions or detail with `.child`.
pub fn empty_state(
    message: impl Into<SharedString>,
    placement: Placement,
    font: Font,
    color: Color,
) -> Div {
    let text = div()
        .font(font.gpui())
        .text_size(px(text_size::BODY))
        .line_height(px(text_size::BODY * typography::DEFAULT_LINE_HEIGHT))
        .text_color(color)
        .child(message.into());
    match placement {
        Placement::Centered => div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(theme::space::SM))
            .p(px(theme::space::XXL))
            .child(text.text_center()),
        Placement::Top => div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(theme::space::SM))
            .px(px(theme::space::XL))
            .py(px(theme::space::XXL))
            .child(text.text_center()),
        Placement::Inline => div()
            .w_full()
            .flex_none()
            .px(px(theme::space::LG))
            .py(px(theme::space::SM))
            .child(text),
    }
}

// ── Banners ─────────────────────────────────────────────────────────────

/// What a [`Banner`] says about its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerTone {
    /// A mode or a scope the view is in: a comparison, a file's history.
    Info,
    /// Worth knowing before going on, but nothing stops it.
    Warning,
    /// Something failed.
    Danger,
}

impl BannerTone {
    /// The tone's ink (its glyph, a title) and its wash.
    pub fn colors(self, theme: ThemeSpec) -> (Color, Color) {
        match self {
            Self::Info => (
                theme.info,
                Color {
                    a: 0.10,
                    ..theme.info
                },
            ),
            Self::Warning => (theme.note_text, theme.note_background),
            Self::Danger => (
                theme.removed_text,
                Color {
                    a: 0.10,
                    ..theme.removed_text
                },
            ),
        }
    }
}

/// Where a [`Banner`] sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerShape {
    /// Edge to edge across a pane, atop its content.
    Strip,
    /// A rounded card inside a pane, a list or a dialog.
    Card,
}

/// A tinted message: an optional glyph on its first line, the message, and
/// controls at the end. Every banner and callout in the app is one, so they
/// stack with one inset and one strength.
pub struct Banner {
    tone: BannerTone,
    shape: BannerShape,
    icon: Option<&'static str>,
    body: Vec<gpui_kit::AnyElement>,
    end: Vec<gpui_kit::AnyElement>,
}

impl Banner {
    pub fn new(tone: BannerTone) -> Self {
        Self {
            tone,
            shape: BannerShape::Card,
            icon: None,
            body: Vec::new(),
            end: Vec::new(),
        }
    }

    pub fn shape(mut self, shape: BannerShape) -> Self {
        self.shape = shape;
        self
    }

    /// A glyph in the tone's ink, centred on the first line's caps.
    pub fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    /// A line or block of the message, stacked under the ones before it.
    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.body.push(child.into_any_element());
        self
    }

    /// [`Banner::child`] for each of `children`.
    pub fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.body
            .extend(children.into_iter().map(IntoElement::into_any_element));
        self
    }

    /// A control at the banner's end, centred on its height.
    pub fn end(mut self, control: impl IntoElement) -> Self {
        self.end.push(control.into_any_element());
        self
    }

    pub fn build(self, font: Font, theme: ThemeSpec) -> Div {
        let (ink, wash) = self.tone.colors(theme);
        let banner = div()
            .w_full()
            .flex_none()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(theme::space::MD))
            .py(px(theme::space::MD))
            .text_size(px(text_size::BODY));
        let banner = match self.shape {
            BannerShape::Strip => banner.px(px(theme::space::XL)).bg(wash),
            BannerShape::Card => theme::squircle(
                banner.px(px(theme::space::LG)),
                theme::radius::CARD,
                Some(wash),
                None,
            ),
        };
        let banner = match self.icon {
            Some(icon) => {
                let line = typography::TextBox::default_line(font, text_size::BODY);
                let top =
                    ((line.cap_top() + line.baseline()) / 2.0 - theme::icon::MD / 2.0).max(0.0);
                banner.child(div().flex_none().pt(px(top)).child(icons::icon(
                    icon,
                    theme::icon::MD,
                    ink,
                )))
            }
            None => banner,
        };
        let banner = banner.child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(theme::space::XXS))
                .children(self.body),
        );
        if self.end.is_empty() {
            banner
        } else {
            banner.child(
                div()
                    .flex_none()
                    .self_center()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(theme::space::XS))
                    // A button's wash reaches past its glyph: the glyph, not
                    // the wash, keeps to the banner's inset.
                    .mr(px(-theme::space::XS))
                    .children(self.end),
            )
        }
    }
}

// ── Tags ────────────────────────────────────────────────────────────────

/// What a status word or count says, by colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Accent,
    Muted,
    Ok,
    Warn,
    Danger,
}

impl Tone {
    pub fn ink(self, theme: ThemeSpec) -> Color {
        match self {
            Tone::Accent => theme.accent,
            Tone::Muted => theme.subtle_text,
            Tone::Ok => theme.added_text,
            Tone::Warn => theme.note_text,
            Tone::Danger => theme.removed_text,
        }
    }
}

/// A [`tag`]'s step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagSize {
    /// 10px: a kind badge (`jj`, `git`), a marker beside a title.
    Small,
    /// 11px: a count or a state ("1 running", "untracked").
    Regular,
}

impl TagSize {
    fn text_size(self) -> f32 {
        match self {
            TagSize::Small => text_size::BADGE,
            TagSize::Regular => text_size::CAPTION,
        }
    }

    /// `[vertical, horizontal]` around the text's line box.
    fn padding(self) -> [f32; 2] {
        match self {
            TagSize::Small => [1.0, 5.0],
            TagSize::Regular => [2.0, 6.0],
        }
    }
}

/// A short label in a tinted box, the chrome's chips and badges. Painted rows
/// draw theirs with [`crate::chip`].
pub struct Tag {
    label: SharedString,
    ink: Color,
    fill: Color,
    edge: Option<Color>,
    size: TagSize,
    font: Font,
    optical: bool,
}

/// `label` in `ink` on a wash of it, at `size` in `font`.
pub fn tag(label: impl Into<SharedString>, ink: Color, size: TagSize, font: Font) -> Tag {
    Tag {
        label: label.into(),
        ink,
        fill: theme::chip_background(ink),
        edge: None,
        size,
        font,
        optical: false,
    }
}

impl Tag {
    /// Set in `mono`, sized to match the UI font beside it.
    pub fn mono(mut self, mono: Font) -> Self {
        self.font = mono;
        self.optical = true;
        self
    }

    /// The box's fill in place of the ink's wash.
    pub fn fill(mut self, fill: Color) -> Self {
        self.fill = fill;
        self
    }

    /// No fill, a 1px `edge` inside the box instead.
    pub fn outlined(mut self, edge: Color) -> Self {
        self.fill = theme::TRANSPARENT;
        self.edge = Some(edge);
        self
    }

    fn parts(self) -> (Div, theme::Padding, Font, f32) {
        let size = if self.optical {
            typography::optical_size(self.font, self.size.text_size())
        } else {
            self.size.text_size()
        };
        let padding = typography::trimmed_padding(self.size.padding().into(), self.font, size);
        let mut chip = padding
            .apply(div())
            .flex_none()
            .bg(self.fill)
            .rounded(px(crate::chip::RADIUS));
        if let Some(edge) = self.edge {
            chip = chip.shadow(vec![theme::inset_ring(edge)]);
        }
        let chip = chip.child(typography::line(self.label, self.font, size, self.ink));
        (chip, padding, self.font, size)
    }

    /// The tag on its text's baseline, for a [`typography::baseline_row`].
    pub fn aligned(self) -> typography::Aligned {
        let (chip, padding, font, size) = self.parts();
        typography::Aligned::boxed(chip, padding, typography::TextBox::default_line(font, size))
    }

    /// The tag's box, for a parent that centres it.
    pub fn view(self) -> Div {
        self.parts().0
    }
}

// ── Keycaps ─────────────────────────────────────────────────────────────

/// Side of a keycap: the box a single key sits in.
pub const KEYCAP: f32 = 18.0;
/// Gap between the keys of one chord.
pub const KEYCAP_GAP: f32 = 3.0;
const KEYCAP_PAD_X: f32 = 5.0;

/// One key in its own small box, raised a step off the card with a firm
/// hairline. Set in the UI font, whatever surrounds it.
pub struct Keycap {
    key: SharedString,
    font: Font,
    ink: Color,
    fill: Color,
    edge: Color,
}

pub fn keycap(key: impl Into<SharedString>, font: Font, theme: ThemeSpec) -> Keycap {
    Keycap {
        key: key.into(),
        font: theme::emphasis_font(font, Weight::MEDIUM),
        ink: theme.subtle_text,
        fill: theme.panel_background_elevated,
        edge: theme::firm_ring(theme),
    }
}

impl Keycap {
    /// Its ink, fill and edge in place of the card's: a cap on an inverted bar.
    pub fn colors(mut self, ink: Color, fill: Color, edge: Color) -> Self {
        self.ink = ink;
        self.fill = fill;
        self.edge = edge;
        self
    }

    fn parts(self) -> (Div, theme::Padding, Font, f32) {
        let size = text_size::BADGE;
        let line = typography::TextBox::default_line(self.font, size).line_height();
        // The edge is a border, inside the cap's height.
        let pad_y = (KEYCAP - 2.0 - line) / 2.0;
        let padding =
            typography::trimmed_padding([pad_y, KEYCAP_PAD_X - 1.0].into(), self.font, size);
        let cap = padding
            .apply(div())
            .flex_none()
            .flex()
            .justify_center()
            .min_w(px(KEYCAP))
            .rounded(px(theme::radius::XS))
            .bg(self.fill)
            .border_1()
            .border_color(self.edge)
            .child(typography::line(self.key, self.font, size, self.ink));
        let padding = theme::Padding {
            top: padding.top + 1.0,
            bottom: padding.bottom + 1.0,
            ..padding
        };
        (cap, padding, self.font, size)
    }

    /// The cap on its text's baseline, for a [`typography::baseline_row`].
    pub fn aligned(self) -> typography::Aligned {
        let (cap, padding, font, size) = self.parts();
        typography::Aligned::boxed(cap, padding, typography::TextBox::default_line(font, size))
    }

    /// The cap's box, for a parent that centres it.
    pub fn view(self) -> Div {
        self.parts().0
    }
}

/// A chord as one keycap per key.
pub fn chord<S: Into<SharedString>>(
    keys: impl IntoIterator<Item = S>,
    font: Font,
    theme: ThemeSpec,
) -> Div {
    div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(px(KEYCAP_GAP))
        .children(keys.into_iter().map(|key| keycap(key, font, theme).view()))
}

/// [`chord`] on its text's baseline.
pub fn chord_aligned<S: Into<SharedString>>(
    keys: impl IntoIterator<Item = S>,
    font: Font,
    theme: ThemeSpec,
) -> typography::Aligned {
    typography::group(
        keys.into_iter()
            .map(|key| keycap(key, font, theme).aligned()),
        KEYCAP_GAP,
    )
}

// ── Modals ──────────────────────────────────────────────────────────────

/// A card over the scrim, hung `top` px below the window's top edge. A press
/// on the scrim runs `dismiss`; one on the card stays on the card. The layer
/// is occluding so wheel events and the cursor don't bleed through to the
/// shell below the modal. Place it as the last child of the root.
pub fn modal(
    id: impl Into<ElementId>,
    card: impl IntoElement,
    top: f32,
    dismiss: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(top))
        .bg(theme::SCRIM)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            dismiss(window, cx);
        })
        .child(
            div()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(card),
        )
}

/// [`modal`] for a dialog holding typed input: a stray press on the scrim
/// mustn't throw the typing away, so only Esc and Cancel close it.
pub fn input_modal(id: impl Into<ElementId>, card: impl IntoElement, top: f32) -> Stateful<Div> {
    modal(id, card, top, |_, _| {})
}

/// Every plain scroll view in the app: `content` scrolling vertically under
/// kit's bar, which [`theme::apply_to_kit`] colours like the hand-drawn bar of
/// the graph and the diff. Like there, the content runs under the bar: row
/// washes may reach the edge, but text keeps [`crate::scrollbar::GUTTER`] clear
/// on the right.
pub fn scrollable(
    id: impl Into<ElementId>,
    content: impl IntoElement,
) -> Scrollable<Stateful<Div>> {
    div()
        .id(id)
        .size_full()
        .child(content)
        .overflow_y_scrollbar()
}

/// [`scrollable`] on a handle the caller holds, for a list that scrolls itself
/// to a row. The handle usually lives in [`scroll_handle`].
pub fn scrollable_with(
    id: impl Into<ElementId>,
    handle: &ScrollHandle,
    content: impl IntoElement,
) -> Stateful<Div> {
    scroll_list(id, handle, ScrollList::default(), content)
}

/// How a [`scroll_list`] sits in its box.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScrollList {
    /// The rows' inset from the box, inside the scroll view: the rows scroll
    /// through it, and the bar stays on the box's edge.
    pub inset: theme::Padding,
    /// How far the bar's track stops short of the top and bottom: for a list
    /// filling a rounded card, so the bar keeps clear of the corners.
    pub bar_inset: f32,
    /// The ground a fade over the top takes once the rows have scrolled
    /// under it.
    pub fade: Option<Color>,
}

/// A list scrolling vertically in its box under kit's bar, on a handle the
/// caller holds. The rows run under the bar; their text keeps
/// [`crate::scrollbar::GUTTER`] clear on the right
/// ([`theme::list::PAD_END`]), which a modal list's inset and row inset make
/// up between them.
pub fn scroll_list(
    id: impl Into<ElementId>,
    handle: &ScrollHandle,
    list: ScrollList,
    rows: impl IntoElement,
) -> Stateful<Div> {
    let id = id.into();
    let scrolled = f32::from(handle.offset().y) < 0.0;
    div()
        .id(id.clone())
        .relative()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            div()
                .id((id, "area"))
                .size_full()
                .flex()
                .flex_col()
                .track_scroll(handle)
                .overflow_y_scroll()
                .child(list.inset.apply(div().w_full().flex_none().child(rows))),
        )
        .children(list.fade.filter(|_| scrolled).map(|ground| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(theme::list::TOP_FADE))
                .bg(gpui_kit::linear_gradient(
                    180.0,
                    gpui_kit::linear_color_stop(ground, 0.0),
                    gpui_kit::linear_color_stop(Color { a: 0.0, ..ground }, 1.0),
                ))
        }))
        // The bar fills the box it's mounted on: one inset from the ends.
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(list.bar_inset))
                .bottom(px(list.bar_inset))
                .vertical_scrollbar(handle),
        )
}

/// Scrolls `handle` just enough to show row `selected`, whose top and bottom
/// in the list's content `span` gives, when `selected` changes since the last
/// frame. Between changes the wheel owns the offset. The viewport is the
/// handle's own bounds, so nothing keeps its height by hand.
pub fn reveal_on_change(
    key: impl Into<ElementId>,
    handle: &ScrollHandle,
    selected: Option<usize>,
    span: impl FnOnce(usize) -> (f32, f32),
    window: &mut Window,
    cx: &mut App,
) {
    let last = window.use_keyed_state(key, cx, |_, _| None::<usize>);
    if *last.read(cx) == selected {
        return;
    }
    let viewport = f32::from(handle.bounds().size.height);
    if selected.is_some() && viewport <= 0.0 {
        // Not laid out yet: try again on the next frame.
        return;
    }
    last.update(cx, |last, _| *last = selected);
    let Some(row) = selected else {
        return;
    };
    let (top, bottom) = span(row);
    let scrolled = -f32::from(handle.offset().y);
    let to = if top < scrolled {
        top
    } else if bottom > scrolled + viewport {
        bottom - viewport
    } else {
        return;
    };
    handle.set_offset(point(px(0.0), px(-to.max(0.0))));
}

/// The scroll handle of list `id`, kept while the list is on screen.
pub fn scroll_handle(
    id: impl Into<ElementId>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ScrollHandle> {
    window.use_keyed_state(id, cx, |_, _| ScrollHandle::default())
}

/// Between the described element's edge and its tooltip.
const TOOLTIP_GAP: f32 = 4.0;
/// The least a tooltip keeps from the window's edges.
const TOOLTIP_MARGIN: f32 = 6.0;
const TOOLTIP_TEXT_SIZE: f32 = text_size::CAPTION;
const TOOLTIP_PAD_X: f32 = 8.0;
const TOOLTIP_PAD_Y: f32 = 3.0;

/// The hover label every tooltip in the app shows: an 11px caption on a small
/// card, like a macOS help tag. One line, never wrapped.
pub fn tooltip_card(label: impl Into<SharedString>, font: Font, theme: ThemeSpec) -> Div {
    let label = label.into();
    let (width, height) = tooltip_size(&label, font);
    theme::surface(div(), theme::Elevation::Tooltip, theme)
        .w(px(width))
        .h(px(height))
        .flex()
        .items_center()
        .px(px(TOOLTIP_PAD_X))
        .font(font.gpui())
        .text_size(px(TOOLTIP_TEXT_SIZE))
        .line_height(px(TOOLTIP_TEXT_SIZE * typography::DEFAULT_LINE_HEIGHT))
        .text_color(theme.text)
        .whitespace_nowrap()
        .child(label)
}

/// The outer size of [`tooltip_card`] holding `label`, its 1px edge included.
fn tooltip_size(label: &str, font: Font) -> (f32, f32) {
    let edge = 2.0;
    let width = crate::measure::line_width(label, TOOLTIP_TEXT_SIZE, font) + 2.0 * TOOLTIP_PAD_X;
    let height = TOOLTIP_TEXT_SIZE * typography::DEFAULT_LINE_HEIGHT + 2.0 * TOOLTIP_PAD_Y;
    (width.ceil() + edge, height.ceil() + edge)
}

/// Where a `size` tooltip goes for an element at `target` in a `viewport`
/// window: centred under it, a few px off; above it when there's no room
/// below; slid along to stay `TOOLTIP_MARGIN` inside the window.
pub fn tooltip_origin(
    target: crate::geometry::Rectangle,
    size: (f32, f32),
    viewport: (f32, f32),
) -> (f32, f32) {
    let (width, height) = size;
    let centre = target.x + target.width / 2.0;
    let x = (centre - width / 2.0)
        .min(viewport.0 - TOOLTIP_MARGIN - width)
        .max(TOOLTIP_MARGIN);
    let below = target.y + target.height + TOOLTIP_GAP;
    let above = target.y - TOOLTIP_GAP - height;
    let y = if below + height <= viewport.1 - TOOLTIP_MARGIN || above < TOOLTIP_MARGIN {
        below
    } else {
        above
    };
    (x, y)
}

/// [`tooltip_card`] for `label`, placed by [`tooltip_origin`]: the card and
/// its window position.
pub fn placed_tooltip(
    label: impl Into<SharedString>,
    target: crate::geometry::Rectangle,
    font: Font,
    theme: ThemeSpec,
    window: &Window,
) -> (Div, gpui_kit::Point<Pixels>) {
    let label = label.into();
    let viewport = window.viewport_size();
    let (x, y) = tooltip_origin(
        target,
        tooltip_size(&label, font),
        (f32::from(viewport.width), f32::from(viewport.height)),
    );
    (tooltip_card(label, font, theme), point(px(x), px(y)))
}

/// The hover label every chrome control uses: `.tip(label, font, theme)` on an
/// element with an id. GPUI drops a tooltip at the cursor, over what's under
/// it; this one is centred under the control instead (see [`tooltip_origin`]).
pub trait Tip: StatefulInteractiveElement + ParentElement + Sized {
    fn tip(self, label: impl Into<SharedString>, font: Font, theme: ThemeSpec) -> Self {
        let anchor: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::default();
        let recorded = anchor.clone();
        let label = label.into();
        // An absolute child sits on its parent's box (taffy positions it
        // against its own parent), so this reads the control's bounds.
        self.child(
            canvas(
                move |bounds, _, _| recorded.set(Some(bounds)),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .tooltip(move |_, cx| {
            let label = label.clone();
            let anchor = anchor.clone();
            cx.new(|_| TooltipView {
                label,
                font,
                theme,
                anchor,
            })
            .into()
        })
    }
}

impl<T: StatefulInteractiveElement + ParentElement + Sized> Tip for T {}

struct TooltipView {
    label: SharedString,
    font: Font,
    theme: ThemeSpec,
    anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl Render for TooltipView {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        match self.anchor.get() {
            Some(control) => {
                let (card, at) = placed_tooltip(
                    self.label.clone(),
                    control.into(),
                    self.font,
                    self.theme,
                    window,
                );
                anchored()
                    .position(at)
                    .snap_to_window_with_margin(px(TOOLTIP_MARGIN))
                    .child(card)
                    .into_any_element()
            }
            None => tooltip_card(self.label.clone(), self.font, self.theme).into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rectangle;

    const VIEWPORT: (f32, f32) = (800.0, 600.0);

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rectangle {
        Rectangle {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_tooltip_hangs_centred_just_under_its_element() {
        let (x, y) = tooltip_origin(rect(300.0, 100.0, 28.0, 28.0), (80.0, 20.0), VIEWPORT);
        assert_eq!(x, 314.0 - 40.0);
        assert_eq!(y, 128.0 + TOOLTIP_GAP);
    }

    #[test]
    fn a_tooltip_without_room_below_goes_above() {
        let (_, y) = tooltip_origin(rect(300.0, 570.0, 28.0, 28.0), (80.0, 20.0), VIEWPORT);
        assert_eq!(y, 570.0 - TOOLTIP_GAP - 20.0);
    }

    #[test]
    fn a_tooltip_stays_inside_the_window() {
        let (x, _) = tooltip_origin(rect(0.0, 100.0, 20.0, 20.0), (80.0, 20.0), VIEWPORT);
        assert_eq!(x, TOOLTIP_MARGIN);
        let (x, _) = tooltip_origin(rect(790.0, 100.0, 10.0, 20.0), (80.0, 20.0), VIEWPORT);
        assert_eq!(x, 800.0 - TOOLTIP_MARGIN - 80.0);
    }
}

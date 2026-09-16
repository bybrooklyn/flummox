//! Colours, styles and the small widget helpers the pages share.
//!
//! Kept separate from [`crate::view`] so that page code reads as layout rather
//! than as a list of colour values.

use iced::widget::{button, container, text};
use iced::{Background, Border, Color, Element, Font, Theme, color};

/// The one accent colour: used for the selected page and for savings.
const ACCENT: Color = color!(0x4FB477);
/// A dimmer accent, for hovered rows.
const ACCENT_DIM: Color = color!(0x2F6B47);
/// Ordinary text.
const TEXT: Color = color!(0xE6E8EA);
/// Secondary text: units, hints, paths.
const TEXT_MUTED: Color = color!(0x9AA1A8);
/// The window background.
const BACKGROUND: Color = color!(0x0F1214);
/// Panels and the sidebar.
const PANEL: Color = color!(0x161A1D);
/// Something went wrong.
const DANGER: Color = color!(0xD9544D);
/// Something needs attention but is not an error.
const WARNING: Color = color!(0xE0A458);

/// The font the whole window uses.
pub const BODY_FONT: Font = Font::DEFAULT;

/// The application's theme.
///
/// Built from a palette rather than hand-styling every widget, so ordinary
/// buttons, checkboxes and scrollbars inherit the right colours for free.
pub fn theme() -> Theme {
    Theme::custom("flummox".to_owned(), iced::theme::Palette {
        background: BACKGROUND,
        text: TEXT,
        primary: ACCENT,
        success: ACCENT,
        warning: WARNING,
        danger: DANGER,
    })
}

/// The window background.
pub fn app_background(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BACKGROUND)),
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

/// The left navigation strip.
pub fn sidebar(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(PANEL)),
        ..container::Style::default()
    }
}

/// A raised card holding one group of information.
pub fn panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(PANEL)),
        border: Border { radius: 6.0.into(), width: 0.0, color: PANEL },
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

/// A banner across the top of the content area.
///
/// `is_error` picks the colour, rather than the wording being sniffed for
/// words like "failed". The wording of a message does not reliably say what kind it is.
pub fn banner(is_error: bool) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(Background::Color(if is_error { DANGER } else { ACCENT_DIM })),
        border: Border { radius: 6.0.into(), width: 0.0, color: DANGER },
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

/// Blends `from` into `to`, with `t` from 0.0 to 1.0.
///
/// Animations interpolate a single number, and this turns that number into
/// the colours a widget style needs.
fn mix(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::from_rgb(
        from.r + (to.r - from.r) * t,
        from.g + (to.g - from.g) * t,
        from.b + (to.b - from.b) * t,
    )
}

/// A sidebar entry, drawn as a button with a filled background.
///
/// `highlight` runs from 0.0 to 1.0 so the caller can animate the selection
/// as it moves between entries instead of snapping.
pub fn nav_button(highlight: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hover = match status {
            button::Status::Hovered | button::Status::Pressed => 0.35,
            button::Status::Active | button::Status::Disabled => 0.0,
        };
        let fill = (highlight + hover).clamp(0.0, 1.0);
        button::Style {
            background: Some(Background::Color(mix(PANEL, ACCENT_DIM, fill))),
            text_color: mix(TEXT_MUTED, TEXT, highlight.clamp(0.0, 1.0).max(hover)),
            border: Border { radius: 6.0.into(), width: 0.0, color: Color::TRANSPARENT },
            ..button::Style::default()
        }
    }
}

/// The button for the main action on a page.
pub fn action_button(_theme: &Theme, status: button::Status) -> button::Style {
    let fill = match status {
        button::Status::Hovered | button::Status::Pressed => ACCENT,
        button::Status::Active => ACCENT_DIM,
        button::Status::Disabled => PANEL,
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: TEXT,
        border: Border { radius: 6.0.into(), width: 0.0, color: Color::TRANSPARENT },
        ..button::Style::default()
    }
}

/// A page heading.
pub fn page_title(label: &str) -> text::Text<'_> {
    text(label).size(26)
}

/// A heading inside a page.
pub fn section_title(label: &str) -> text::Text<'_> {
    text(label).size(16)
}

/// Secondary text, for units and explanations.
pub fn muted<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label).size(13).color(TEXT_MUTED)
}

/// A number worth reading from across the room, with its label beneath.
pub fn stat<'a, Message: 'a>(value: String, label: &'a str) -> Element<'a, Message> {
    iced::widget::column![text(value).size(30).color(ACCENT), muted(label)].spacing(2).into()
}

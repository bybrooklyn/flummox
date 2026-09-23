//! Colours, styles and the small widget helpers the pages share.
//!
//! Kept separate from [`crate::view`] so that page code reads as layout rather
//! than as a list of colour values.

#[cfg(target_os = "linux")]
use iced::widget::scrollable as scrollable_widget;
use iced::widget::{button, container, text};
use iced::{Background, Border, Color, Element, Font, Shadow, Theme, Vector, color};

#[derive(Clone, Copy)]
struct Colors {
    accent: Color,
    accent_dim: Color,
    text: Color,
    muted: Color,
    background: Color,
    #[cfg(target_os = "linux")]
    sidebar: Color,
    panel: Color,
    hero: Color,
    border: Color,
    danger: Color,
    warning: Color,
}

fn colors(theme: &Theme) -> Colors {
    if theme.extended_palette().is_dark {
        Colors {
            accent: color!(0x55C887),
            accent_dim: color!(0x2D6E49),
            text: color!(0xE9ECEB),
            muted: color!(0x9AA4A0),
            background: color!(0x111517),
            #[cfg(target_os = "linux")]
            sidebar: color!(0x161B1E),
            panel: color!(0x191F22),
            hero: color!(0x182720),
            border: color!(0x293135),
            danger: color!(0xE26861),
            warning: color!(0xE6AE5C),
        }
    } else {
        Colors {
            accent: color!(0x247A4B),
            accent_dim: color!(0xCDE8D8),
            text: color!(0x18201C),
            muted: color!(0x63706A),
            background: color!(0xF4F7F5),
            #[cfg(target_os = "linux")]
            sidebar: color!(0xECF1EE),
            panel: color!(0xFFFFFF),
            hero: color!(0xEAF5EE),
            border: color!(0xD7DFDA),
            danger: color!(0xB83D38),
            warning: color!(0x9A641D),
        }
    }
}

/// The font the whole window uses.
pub const BODY_FONT: Font = Font::DEFAULT;

/// The application's theme.
///
/// Built from a palette rather than hand-styling every widget, so ordinary
/// buttons, checkboxes and scrollbars inherit the right colours for free.
pub fn theme(dark: bool) -> Theme {
    let colors = if dark {
        colors(&Theme::Dark)
    } else {
        colors(&Theme::Light)
    };
    Theme::custom(
        if dark {
            "Flummox Dark"
        } else {
            "Flummox Light"
        }
        .to_owned(),
        iced::theme::Palette {
            background: colors.background,
            text: colors.text,
            primary: colors.accent,
            success: colors.accent,
            warning: colors.warning,
            danger: colors.danger,
        },
    )
}

/// The window background.
pub fn app_background(theme: &Theme) -> container::Style {
    let colors = colors(theme);
    container::Style {
        background: Some(Background::Color(colors.background)),
        text_color: Some(colors.text),
        ..container::Style::default()
    }
}

/// The left navigation strip.
#[cfg(target_os = "linux")]
pub fn sidebar(theme: &Theme) -> container::Style {
    let colors = colors(theme);
    container::Style {
        background: Some(Background::Color(colors.sidebar)),
        border: Border {
            radius: 0.0.into(),
            width: 0.0,
            color: colors.border,
        },
        ..container::Style::default()
    }
}

/// A raised card holding one group of information.
pub fn panel(theme: &Theme) -> container::Style {
    let colors = colors(theme);
    container::Style {
        background: Some(Background::Color(colors.panel)),
        border: Border {
            radius: 12.0.into(),
            width: 1.0,
            color: colors.border,
        },
        text_color: Some(colors.text),
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.10),
            offset: Vector::new(0.0, 1.0),
            blur_radius: 5.0,
        },
        ..container::Style::default()
    }
}

/// The main recommendation on Overview.
pub fn hero(theme: &Theme) -> container::Style {
    let colors = colors(theme);
    container::Style {
        background: Some(Background::Color(colors.hero)),
        border: Border {
            radius: 12.0.into(),
            width: 1.0,
            color: colors.accent_dim,
        },
        text_color: Some(colors.text),
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.24),
            offset: Vector::new(0.0, 5.0),
            blur_radius: 18.0,
        },
        ..container::Style::default()
    }
}

/// A banner across the top of the content area.
///
/// `is_error` picks the colour, rather than the wording being sniffed for
/// words like "failed". The wording of a message does not reliably say what kind it is.
#[cfg(windows)]
pub fn banner(is_error: bool) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let colors = colors(theme);
        let edge = if is_error {
            colors.danger
        } else {
            colors.accent
        };
        container::Style {
            background: Some(Background::Color(mix(colors.panel, edge, 0.14))),
            border: Border {
                radius: 8.0.into(),
                width: 1.0,
                color: edge,
            },
            text_color: Some(colors.text),
            ..container::Style::default()
        }
    }
}

/// A compact notification floating above the page without moving its content.
#[cfg(target_os = "linux")]
pub fn toast(is_error: bool, reveal: f32) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let colors = colors(theme);
        let reveal = reveal.clamp(0.0, 1.0);
        let surface = if is_error {
            mix(colors.panel, colors.danger, 0.14)
        } else {
            colors.panel
        };
        let edge = if is_error {
            colors.danger
        } else {
            colors.accent
        };
        container::Style {
            background: Some(Background::Color(surface.scale_alpha(reveal))),
            border: Border {
                radius: 10.0.into(),
                width: 1.0,
                color: edge.scale_alpha(reveal),
            },
            text_color: Some(colors.text.scale_alpha(reveal)),
            shadow: Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.38 * reveal),
                offset: Vector::new(0.0, 8.0 * reveal),
                blur_radius: 24.0 * reveal,
            },
            ..container::Style::default()
        }
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
#[cfg(target_os = "linux")]
pub fn nav_button(highlight: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let colors = colors(theme);
        let hover = match status {
            button::Status::Hovered | button::Status::Pressed => 0.35,
            button::Status::Active | button::Status::Disabled => 0.0,
        };
        let fill = (highlight + hover).clamp(0.0, 1.0);
        button::Style {
            background: Some(Background::Color(mix(
                colors.sidebar,
                colors.accent_dim,
                fill,
            ))),
            text_color: mix(
                colors.muted,
                colors.text,
                highlight.clamp(0.0, 1.0).max(hover),
            ),
            border: Border {
                radius: 8.0.into(),
                width: 0.0,
                color: Color::TRANSPARENT,
            },
            ..button::Style::default()
        }
    }
}

/// A navigation symbol that follows the animated selection color.
#[cfg(target_os = "linux")]
pub fn nav_icon(highlight: f32) -> impl Fn(&Theme) -> text::Style {
    move |theme| {
        let colors = colors(theme);
        text::Style {
            color: Some(mix(colors.muted, colors.text, highlight.clamp(0.0, 1.0))),
        }
    }
}

/// The button for the main action on a page.
pub fn action_button(theme: &Theme, status: button::Status) -> button::Style {
    let colors = colors(theme);
    let dark = theme.extended_palette().is_dark;
    let fill = match status {
        button::Status::Hovered => colors.accent,
        button::Status::Pressed => mix(colors.accent_dim, colors.background, 0.2),
        button::Status::Active => colors.accent_dim,
        button::Status::Disabled => colors.panel,
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: match status {
            button::Status::Hovered if dark => colors.background,
            button::Status::Hovered => Color::WHITE,
            button::Status::Disabled => colors.muted,
            button::Status::Active | button::Status::Pressed => colors.text,
        },
        border: Border {
            radius: 8.0.into(),
            width: 0.0,
            color: Color::TRANSPARENT,
        },
        ..button::Style::default()
    }
}

/// A lower-emphasis action that still belongs to the shared surface system.
pub fn secondary_button(theme: &Theme, status: button::Status) -> button::Style {
    let colors = colors(theme);
    let background = match status {
        button::Status::Hovered => Some(Background::Color(mix(
            colors.panel,
            colors.accent_dim,
            0.30,
        ))),
        button::Status::Pressed => Some(Background::Color(mix(
            colors.panel,
            colors.accent_dim,
            0.50,
        ))),
        button::Status::Active | button::Status::Disabled => None,
    };
    button::Style {
        background,
        text_color: if status == button::Status::Disabled {
            colors.muted
        } else {
            colors.text
        },
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: colors.border,
        },
        ..button::Style::default()
    }
}

/// Scrollbar rails and their corner use the page canvas.
#[cfg(target_os = "linux")]
pub fn scrollable(theme: &Theme, status: scrollable_widget::Status) -> scrollable_widget::Style {
    let colors = colors(theme);
    let mut style = scrollable_widget::default(theme, status);
    style.container.background = Some(Background::Color(colors.background));
    style.vertical_rail.background = Some(Background::Color(colors.background));
    style.horizontal_rail.background = Some(Background::Color(colors.background));
    style.gap = Some(Background::Color(colors.background));
    style
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
    text(label).size(13).style(|theme: &Theme| text::Style {
        color: Some(colors(theme).muted),
    })
}

/// Error text placed beside the control that needs correction.
#[cfg(target_os = "linux")]
pub fn danger_text<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label).size(13).style(|theme: &Theme| text::Style {
        color: Some(colors(theme).danger),
    })
}

/// The state marker in a toast.
#[cfg(target_os = "linux")]
pub fn toast_mark(is_error: bool) -> impl Fn(&Theme) -> text::Style {
    move |theme| {
        let colors = colors(theme);
        text::Style {
            color: Some(if is_error {
                colors.danger
            } else {
                colors.accent
            }),
        }
    }
}

/// A number worth reading from across the room, with its label beneath.
pub fn stat<'a, Message: 'a>(value: String, label: &'a str) -> Element<'a, Message> {
    iced::widget::column![
        text(value).size(30).style(|theme: &Theme| text::Style {
            color: Some(colors(theme).accent),
        }),
        muted(label)
    ]
    .spacing(2)
    .width(iced::Length::Fill)
    .into()
}

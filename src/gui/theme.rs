//! Colours, styles and the small widget helpers the pages share.
//!
//! Kept separate from [`crate::view`] so that page code reads as layout rather
//! than as a list of colour values.

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use iced::widget::{Space, button, column, pick_list as pick_list_widget, row};
use iced::widget::{container, text};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[cfg(any(target_os = "linux", test))]
use iced::widget::progress_bar as progress_widget;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use iced::widget::{scrollable as scrollable_widget, text_input as text_input_widget};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use iced::{Alignment, Border, Element, Length, Padding};
use iced::{Background, Color, Font, Theme, color};

/// The palette every style in this file draws from. Fields are gated to the
/// platforms whose front end reads them.
#[derive(Clone, Copy)]
struct Colors {
    accent: Color,
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    accent_dim: Color,
    text: Color,
    muted: Color,
    background: Color,
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    sidebar: Color,
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    panel: Color,
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    hero: Color,
    /// The edge of a card.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    border: Color,
    /// The edge of a control, at 3:1 against the surfaces it sits on.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    control_border: Color,
    danger: Color,
    warning: Color,
}

/// The dark or light palette, chosen by whether `theme` is a dark theme.
fn colors(theme: &Theme) -> Colors {
    if theme.extended_palette().is_dark {
        Colors {
            accent: color!(0x55C887),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            accent_dim: color!(0x2D6E49),
            text: color!(0xE9ECEB),
            muted: color!(0x9AA4A0),
            background: color!(0x141618),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            sidebar: color!(0x191C1F),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            panel: color!(0x1E2225),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            hero: color!(0x1E2225),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            border: color!(0x444C53),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            control_border: color!(0x6B7771),
            danger: color!(0xE26861),
            warning: color!(0xE6AE5C),
        }
    } else {
        Colors {
            accent: color!(0x247A4B),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            accent_dim: color!(0xCDE8D8),
            text: color!(0x18201C),
            muted: color!(0x63706A),
            background: color!(0xF5F6F7),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            sidebar: color!(0xECF1EE),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            panel: color!(0xFFFFFF),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            hero: color!(0xFFFFFF),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            border: color!(0xB9C4BE),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            control_border: color!(0x7C8882),
            danger: color!(0xB83D38),
            warning: color!(0x9A641D),
        }
    }
}

/// The font the whole window uses.
pub const BODY_FONT: Font = Font::DEFAULT;

/// The weight headings use, so a heading reads as one beside body text of a
/// similar size.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub const HEADING_FONT: Font = Font {
    weight: iced::font::Weight::Semibold,
    ..BODY_FONT
};

/// Size of a page heading.
pub const PAGE_TITLE_SIZE: f32 = 26.0;
/// Size of every heading inside a page: groups, cards and settings sections.
pub const SECTION_TITLE_SIZE: f32 = 16.0;
/// Vertical space between the blocks of a page column.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub const PAGE_GAP: f32 = 16.0;
/// Padding inside a card and a hero.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub const CARD_PADDING: f32 = 16.0;
/// Padding of every button, so a row of them has one height.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub const BUTTON_PADDING: [f32; 2] = [9.0, 14.0];
/// Padding of text inputs and pick lists, which match the buttons beside them.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub const INPUT_PADDING: [f32; 2] = [9.0, 12.0];
/// Width of the figures column of a list row, so sizes line up.
#[cfg(target_os = "linux")]
pub const TRAILING_WIDTH: f32 = 220.0;
/// Width of the slot that holds a list row's one button, empty or not.
#[cfg(target_os = "linux")]
pub const ACTION_SLOT_WIDTH: f32 = 140.0;
/// Height of the page header row, so titles sit at one baseline.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
const PAGE_HEADER_HEIGHT: f32 = 40.0;

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
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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
        ..container::Style::default()
    }
}

/// The main recommendation on Overview.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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
        ..container::Style::default()
    }
}

/// A card for something that went wrong or needs a decision. The edge is
/// `danger` when `is_error` and `warning` otherwise.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn attention_panel(is_error: bool) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let colors = colors(theme);
        let edge = if is_error {
            colors.danger
        } else {
            colors.warning
        };
        container::Style {
            background: Some(Background::Color(mix(colors.panel, edge, 0.06))),
            border: Border {
                radius: 12.0.into(),
                width: 1.0,
                color: edge,
            },
            text_color: Some(colors.text),
            ..container::Style::default()
        }
    }
}

/// A compact notification floating above the page without moving its content.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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
            ..container::Style::default()
        }
    }
}

/// Blends `from` into `to`, with `t` from 0.0 to 1.0.
///
/// Animations interpolate a single number, and this turns that number into
/// the colours a widget style needs.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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
/// as it moves between entries instead of snapping. The selected entry also
/// gets an `accent` edge, which stays visible when the fill is pale.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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
                width: 1.0,
                color: colors.accent.scale_alpha(highlight.clamp(0.0, 1.0)),
            },
            ..button::Style::default()
        }
    }
}

/// A navigation symbol that follows the animated selection color.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn nav_icon(highlight: f32) -> impl Fn(&Theme) -> text::Style {
    move |theme| {
        let colors = colors(theme);
        text::Style {
            color: Some(mix(colors.muted, colors.text, highlight.clamp(0.0, 1.0))),
        }
    }
}

/// The button for the main action on a page.
///
/// Light themes fill with `accent` and white text, since the pale
/// `accent_dim` is 1.3:1 against a white card. Disabled draws a tinted fill
/// and an outline, so it reads as a button that cannot be pressed.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn action_button(theme: &Theme, status: button::Status) -> button::Style {
    let colors = colors(theme);
    let dark = theme.extended_palette().is_dark;
    let fill = match (status, dark) {
        (button::Status::Hovered, true) => colors.accent,
        (button::Status::Hovered, false) => mix(colors.accent, Color::BLACK, 0.15),
        (button::Status::Pressed, true) => mix(colors.accent_dim, colors.background, 0.2),
        (button::Status::Pressed, false) => mix(colors.accent, Color::BLACK, 0.30),
        (button::Status::Active, true) => colors.accent_dim,
        (button::Status::Active, false) => colors.accent,
        (button::Status::Disabled, _) => mix(colors.panel, colors.accent_dim, 0.25),
    };
    let disabled = status == button::Status::Disabled;
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: match status {
            button::Status::Hovered if dark => colors.background,
            button::Status::Disabled => colors.muted,
            _ if dark => colors.text,
            _ => Color::WHITE,
        },
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: if disabled { colors.border } else { fill },
        },
        ..button::Style::default()
    }
}

/// A lower-emphasis action that still belongs to the shared surface system.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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
    let disabled = status == button::Status::Disabled;
    button::Style {
        background,
        text_color: if disabled { colors.muted } else { colors.text },
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: if disabled {
                mix(colors.control_border, colors.panel, 0.6)
            } else {
                colors.control_border
            },
        },
        ..button::Style::default()
    }
}

/// A toggle in its chosen state: an `accent` edge and a tinted fill, which
/// marks the choice without competing with the page's primary action.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn selected_button(theme: &Theme, status: button::Status) -> button::Style {
    let colors = colors(theme);
    let dark = theme.extended_palette().is_dark;
    let base = if dark { 0.45 } else { 0.6 };
    let fill = match status {
        button::Status::Hovered => mix(colors.panel, colors.accent_dim, base + 0.15),
        button::Status::Pressed => mix(colors.panel, colors.accent_dim, base + 0.3),
        button::Status::Active | button::Status::Disabled => {
            mix(colors.panel, colors.accent_dim, base)
        }
    };
    button::Style {
        background: Some(Background::Color(fill)),
        text_color: if status == button::Status::Disabled {
            colors.muted
        } else {
            colors.text
        },
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: colors.accent,
        },
        ..button::Style::default()
    }
}

/// A borderless text button for expanding and collapsing a section.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn disclosure_button(theme: &Theme, status: button::Status) -> button::Style {
    let colors = colors(theme);
    button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => colors.text,
            button::Status::Active => colors.accent,
            button::Status::Disabled => colors.muted,
        },
        border: Border {
            radius: 8.0.into(),
            width: 0.0,
            color: Color::TRANSPARENT,
        },
        ..button::Style::default()
    }
}

/// A closed pick list or the box of a text input, drawn from one set of
/// colours so a row of controls matches.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn field_border(colors: Colors, hot: bool) -> Border {
    Border {
        radius: 8.0.into(),
        width: 1.0,
        color: if hot {
            colors.accent
        } else {
            colors.control_border
        },
    }
}

/// A drop-down list.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn pick_list(theme: &Theme, status: pick_list_widget::Status) -> pick_list_widget::Style {
    let colors = colors(theme);
    pick_list_widget::Style {
        text_color: colors.text,
        placeholder_color: colors.muted,
        handle_color: colors.muted,
        background: Background::Color(colors.panel),
        border: field_border(colors, !matches!(status, pick_list_widget::Status::Active)),
    }
}

/// The open menu of a drop-down list.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn pick_menu(theme: &Theme) -> iced::overlay::menu::Style {
    let colors = colors(theme);
    iced::overlay::menu::Style {
        background: Background::Color(colors.panel),
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: colors.control_border,
        },
        text_color: colors.text,
        selected_text_color: colors.text,
        selected_background: Background::Color(mix(colors.panel, colors.accent_dim, 0.6)),
        shadow: iced::Shadow::default(),
    }
}

/// A single-line text field.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn text_input(theme: &Theme, status: text_input_widget::Status) -> text_input_widget::Style {
    let colors = colors(theme);
    let disabled = status == text_input_widget::Status::Disabled;
    text_input_widget::Style {
        background: Background::Color(colors.panel),
        border: if disabled {
            Border {
                color: mix(colors.control_border, colors.panel, 0.6),
                ..field_border(colors, false)
            }
        } else {
            field_border(
                colors,
                matches!(
                    status,
                    text_input_widget::Status::Hovered | text_input_widget::Status::Focused { .. }
                ),
            )
        },
        icon: colors.muted,
        placeholder: colors.muted,
        value: if disabled { colors.muted } else { colors.text },
        selection: colors.accent.scale_alpha(0.35),
    }
}

/// Scrollbar rails and their corner use the page canvas, and the scroller is
/// a neutral between the border and the muted text colour.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn scrollable(theme: &Theme, status: scrollable_widget::Status) -> scrollable_widget::Style {
    let colors = colors(theme);
    let mut style = scrollable_widget::default(theme, status);
    let (vertical_hot, horizontal_hot) = match status {
        scrollable_widget::Status::Active { .. } => (false, false),
        scrollable_widget::Status::Hovered {
            is_vertical_scrollbar_hovered,
            is_horizontal_scrollbar_hovered,
            ..
        } => (
            is_vertical_scrollbar_hovered,
            is_horizontal_scrollbar_hovered,
        ),
        scrollable_widget::Status::Dragged {
            is_vertical_scrollbar_dragged,
            is_horizontal_scrollbar_dragged,
            ..
        } => (
            is_vertical_scrollbar_dragged,
            is_horizontal_scrollbar_dragged,
        ),
    };
    let scroller = |hot: bool| {
        Background::Color(if hot {
            colors.muted
        } else {
            mix(colors.border, colors.muted, 0.7)
        })
    };
    style.container.background = Some(Background::Color(colors.background));
    style.vertical_rail.background = Some(Background::Color(colors.background));
    style.horizontal_rail.background = Some(Background::Color(colors.background));
    style.vertical_rail.scroller.background = scroller(vertical_hot);
    style.horizontal_rail.scroller.background = scroller(horizontal_hot);
    style.gap = Some(Background::Color(colors.background));
    style
}

/// How a job's bar and phase label are coloured.
#[cfg(any(target_os = "linux", windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Work is moving, or finished.
    Normal,
    /// Work is paused, or finished with errors to review.
    Warning,
    /// Work ended badly.
    Failed,
}

/// The colour a `tone` draws in.
#[cfg(any(target_os = "linux", windows, test))]
fn tone_color(colors: Colors, tone: Tone) -> Color {
    match tone {
        Tone::Normal => colors.accent,
        Tone::Warning => colors.warning,
        Tone::Failed => colors.danger,
    }
}

/// A thin progress bar in the colour of `tone`.
#[cfg(any(target_os = "linux", test))]
pub fn progress_bar(tone: Tone) -> impl Fn(&Theme) -> progress_widget::Style {
    move |theme| {
        let colors = colors(theme);
        progress_widget::Style {
            background: Background::Color(mix(colors.panel, colors.control_border, 0.3)),
            bar: Background::Color(tone_color(colors, tone)),
            border: Border {
                radius: 3.0.into(),
                ..Border::default()
            },
        }
    }
}

/// Text in the colour of `tone`, for a phase label.
#[cfg(any(target_os = "linux", windows))]
pub fn tone_text(tone: Tone) -> impl Fn(&Theme) -> text::Style {
    move |theme| text::Style {
        color: Some(match tone {
            Tone::Normal => colors(theme).muted,
            other => tone_color(colors(theme), other),
        }),
    }
}

/// A page heading.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn page_title(label: &str) -> text::Text<'_> {
    text(label).size(PAGE_TITLE_SIZE).font(HEADING_FONT)
}

/// A heading inside a page, used for groups, cards and settings sections.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn section_title(label: &str) -> text::Text<'_> {
    text(label).size(SECTION_TITLE_SIZE).font(HEADING_FONT)
}

/// As [`section_title`], for a heading built at run time.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn section_text<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label).size(SECTION_TITLE_SIZE).font(HEADING_FONT)
}

/// Secondary text, for units and explanations.
pub fn muted<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label).size(13).style(|theme: &Theme| text::Style {
        color: Some(colors(theme).muted),
    })
}

/// Error text placed beside the control that needs correction.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn danger_text<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label).size(13).style(|theme: &Theme| text::Style {
        color: Some(colors(theme).danger),
    })
}

/// Text for something that needs a look but has not failed.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn warning_text<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label).size(13).style(|theme: &Theme| text::Style {
        color: Some(colors(theme).warning),
    })
}

/// A success marker, such as the tick on a finished job.
#[cfg(any(target_os = "linux", windows))]
pub fn accent_text<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label).style(|theme: &Theme| text::Style {
        color: Some(colors(theme).accent),
    })
}

/// The state marker in a toast.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn stat<'a, Message: 'a>(value: String, label: &'a str) -> Element<'a, Message> {
    column![
        text(value).size(30).style(|theme: &Theme| text::Style {
            color: Some(colors(theme).accent),
        }),
        muted(label)
    ]
    .spacing(2)
    .width(Length::Fill)
    .into()
}

/// The filled button for the main action of a row or page.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn action<'a, Message: Clone + 'a>(
    label: impl Into<String>,
    message: Message,
) -> Element<'a, Message> {
    action_maybe(label, Some(message))
}

/// As [`action`], drawn disabled when `message` is `None`.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn action_maybe<'a, Message: Clone + 'a>(
    label: impl Into<String>,
    message: Option<Message>,
) -> Element<'a, Message> {
    button(text(label.into()))
        .padding(BUTTON_PADDING)
        .style(action_button)
        .on_press_maybe(message)
        .into()
}

/// The outlined button for every other action.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn secondary<'a, Message: Clone + 'a>(
    label: impl Into<String>,
    message: Message,
) -> Element<'a, Message> {
    secondary_maybe(label, Some(message))
}

/// As [`secondary`], drawn disabled when `message` is `None`.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn secondary_maybe<'a, Message: Clone + 'a>(
    label: impl Into<String>,
    message: Option<Message>,
) -> Element<'a, Message> {
    button(text(label.into()))
        .padding(BUTTON_PADDING)
        .style(secondary_button)
        .on_press_maybe(message)
        .into()
}

/// A toggle button, drawn in its chosen state when `chosen`.
#[cfg(target_os = "linux")]
pub fn choice<'a, Message: Clone + 'a>(
    label: impl Into<String>,
    chosen: bool,
    message: Message,
) -> Element<'a, Message> {
    let button = button(text(label.into()))
        .padding(BUTTON_PADDING)
        .on_press(message);
    if chosen {
        button.style(selected_button).into()
    } else {
        button.style(secondary_button).into()
    }
}

/// The one control that opens and closes an Advanced section.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn disclosure<'a, Message: Clone + 'a>(
    label: &'a str,
    open: bool,
    message: Message,
) -> Element<'a, Message> {
    button(
        row![text(if open { "▾" } else { "▸" }).size(13), text(label)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .style(disclosure_button)
    .padding([BUTTON_PADDING[0], 4.0])
    .on_press(message)
    .into()
}

/// A card.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn panel_card<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(content)
        .padding(CARD_PADDING)
        .width(Length::Fill)
        .style(panel)
        .into()
}

/// The recommendation card.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn hero_card<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(content)
        .padding(CARD_PADDING)
        .width(Length::Fill)
        .style(hero)
        .into()
}

/// A card for a failure (`is_error`) or something that needs attention.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn attention_card<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    is_error: bool,
) -> Element<'a, Message> {
    container(content)
        .padding(CARD_PADDING)
        .width(Length::Fill)
        .style(attention_panel(is_error))
        .into()
}

/// The top of a page: the title with `trailing` at the right of a row of
/// fixed height, and the optional `subtitle` under it.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn page_header<'a, Message: 'a>(
    title: &'a str,
    subtitle: Option<&'a str>,
    trailing: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut top = row![
        container(page_title(title))
            .height(PAGE_HEADER_HEIGHT)
            .align_y(Alignment::Center),
        Space::new().width(Length::Fill)
    ]
    .align_y(Alignment::Center);
    if let Some(trailing) = trailing {
        top = top.push(trailing);
    }
    let mut header = column![top].spacing(4);
    if let Some(subtitle) = subtitle {
        header = header.push(muted(subtitle));
    }
    header.into()
}

/// A label above its input.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn field<'a, Message: 'a>(
    label: &'a str,
    input: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![muted(label), input.into()].spacing(4).into()
}

/// A line of label and value, with the label in a fixed-width column.
#[cfg(target_os = "linux")]
pub fn fact<'a, Message: 'a>(label: &'a str, value: String) -> Element<'a, Message> {
    row![
        container(muted(label)).width(Length::Fixed(110.0)),
        text(value).size(13).width(Length::Fill)
    ]
    .spacing(12)
    .into()
}

/// The padding of a page's outer gutter.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub fn page_gutter(compact: bool) -> Padding {
    Padding::from(if compact { 16.0 } else { 24.0 })
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    use super::*;
    use crate::testutil::{TestResult, check};

    fn luminance(color: Color) -> f32 {
        let channel = |value: f32| {
            if value <= 0.039_28 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
    }

    fn contrast(a: Color, b: Color) -> f32 {
        let (high, low) = (
            luminance(a).max(luminance(b)),
            luminance(a).min(luminance(b)),
        );
        (high + 0.05) / (low + 0.05)
    }

    fn fill(style: button::Style) -> Color {
        match style.background {
            Some(Background::Color(color)) => color,
            _ => Color::TRANSPARENT,
        }
    }

    #[test]
    fn headings_step_down_from_page_to_section() -> TestResult {
        check(
            PAGE_TITLE_SIZE > SECTION_TITLE_SIZE && SECTION_TITLE_SIZE > 13.0,
            "page title above section title above muted text",
        )
    }

    #[test]
    fn controls_meet_the_boundary_and_text_contrast_floors() -> TestResult {
        for dark in [true, false] {
            let theme = theme(dark);
            let colors = colors(&theme);
            let name = if dark { "dark" } else { "light" };
            for (what, ground) in [("panel", colors.panel), ("background", colors.background)] {
                check(
                    contrast(colors.control_border, ground) >= 3.0,
                    format!("{name} control border on {what} reaches 3:1"),
                )?;
            }
            let primary = action_button(&theme, button::Status::Active);
            check(
                contrast(primary.text_color, fill(primary)) >= 4.5,
                format!("{name} primary label reaches 4.5:1"),
            )?;
            check(
                contrast(fill(primary), colors.panel) >= 3.0 || dark,
                format!("{name} primary fill stands out from a card"),
            )?;
            for (what, text_color) in [
                ("warning", colors.warning),
                ("danger", colors.danger),
                ("muted", colors.muted),
                ("accent", colors.accent),
            ] {
                check(
                    contrast(text_color, colors.panel) >= 4.5,
                    format!("{name} {what} text on a card reaches 4.5:1"),
                )?;
            }
            for (what, is_error) in [("warning", false), ("danger", true)] {
                let style = attention_panel(is_error)(&theme);
                let Some(Background::Color(tint)) = style.background else {
                    return Err(format!("{name} {what} card has no fill"));
                };
                for (kind, text_color) in [
                    ("body", colors.text),
                    ("muted", colors.muted),
                    (
                        what,
                        if is_error {
                            colors.danger
                        } else {
                            colors.warning
                        },
                    ),
                ] {
                    check(
                        contrast(text_color, tint) >= 4.5,
                        format!("{name} {kind} text on a {what} card reaches 4.5:1"),
                    )?;
                }
            }
            for tone in [Tone::Normal, Tone::Warning, Tone::Failed] {
                let style = progress_bar(tone)(&theme);
                let (Background::Color(track), Background::Color(bar)) =
                    (style.background, style.bar)
                else {
                    return Err(format!("{name} {tone:?} bar has no fill"));
                };
                check(
                    contrast(bar, track) >= 3.0,
                    format!("{name} {tone:?} bar stands out from its track"),
                )?;
            }
            let selected = selected_button(&theme, button::Status::Active);
            check(
                contrast(selected.border.color, colors.panel) >= 3.0,
                format!("{name} selected edge reaches 3:1"),
            )?;
            check(
                contrast(selected.text_color, fill(selected)) >= 4.5,
                format!("{name} selected label reaches 4.5:1"),
            )?;
        }
        Ok(())
    }

    #[test]
    fn a_disabled_primary_button_differs_from_plain_text() -> TestResult {
        for dark in [true, false] {
            let theme = theme(dark);
            let off = action_button(&theme, button::Status::Disabled);
            let on = action_button(&theme, button::Status::Active);
            check(off.border.width > 0.0, "disabled has an outline")?;
            check(
                fill(off) != colors(&theme).panel,
                "disabled fill differs from the card under it",
            )?;
            check(fill(off) != fill(on), "disabled differs from enabled")?;
            let quiet = secondary_button(&theme, button::Status::Disabled);
            let live = secondary_button(&theme, button::Status::Active);
            check(
                quiet.border.color != live.border.color,
                "a disabled outlined button dims its border",
            )?;
        }
        Ok(())
    }

    #[test]
    fn buttons_in_one_row_share_a_box() -> TestResult {
        let theme = theme(true);
        let primary = action_button(&theme, button::Status::Active);
        let secondary = secondary_button(&theme, button::Status::Active);
        check(
            primary.border.width == secondary.border.width,
            "both styles draw a 1 px border, so padding gives one height",
        )
    }
}

//! Icons painted from rectangles, so they do not depend on a system font.
//!
//! Each icon is a list of rectangles in a unit box. [`shapes`] returns them
//! and [`icon`] paints them at a pixel size, rounding every edge to a whole
//! pixel so small icons stay crisp.
use iced::advanced::widget::Tree;
use iced::advanced::{Layout, Widget, layout, mouse, renderer};
use iced::{Border, Color, Element, Length, Rectangle, Shadow, Size, Theme};

/// What an icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Overview,
    Games,
    Jobs,
    Settings,
    /// A stack of two disks.
    #[cfg(target_os = "linux")]
    Drives,
    /// An arrow pointing back.
    #[cfg(target_os = "linux")]
    Recovery,
    /// A cross for dismissing.
    Close,
    /// A tick.
    #[cfg(any(target_os = "linux", windows))]
    Check,
    /// A filled circle.
    Dot,
    /// A disclosure arrow pointing right.
    ChevronClosed,
    /// A disclosure arrow pointing down.
    ChevronOpen,
}

/// Which colour paints an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    /// The text colour of the widget that holds the icon, such as a button.
    Inherit,
    Accent,
    Danger,
    #[cfg(any(target_os = "linux", windows))]
    Muted,
}

/// A rectangle in the unit box. Every value is a fraction of the box side,
/// and `radius` is the corner radius.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
}

/// The icon grid is this many units on a side.
const GRID: f32 = 12.0;

/// A rectangle given in grid units.
fn cell(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Rect {
    Rect {
        x: x / GRID,
        y: y / GRID,
        w: w / GRID,
        h: h / GRID,
        radius: radius / GRID,
    }
}

/// A stair-step line: `size` squares at `points` on a grid of cells `pitch`
/// units apart, offset by `(x, y)`.
fn steps(points: &[(f32, f32)], pitch: f32, size: f32, x: f32, y: f32) -> Vec<Rect> {
    points
        .iter()
        .map(|(col, row)| cell(x + col * pitch, y + row * pitch, size, size, 0.3))
        .collect()
}

/// The rectangles that make up `icon`, in the unit box.
pub fn shapes(icon: Icon) -> Vec<Rect> {
    match icon {
        Icon::Overview => [(1.0, 1.0), (6.5, 1.0), (1.0, 6.5), (6.5, 6.5)]
            .iter()
            .map(|(x, y)| cell(*x, *y, 4.5, 4.5, 1.2))
            .collect(),
        Icon::Games => vec![
            cell(1.0, 4.25, 10.0, 3.5, 1.0),
            cell(4.25, 1.0, 3.5, 10.0, 1.0),
        ],
        Icon::Jobs => [1.5, 5.0, 8.5]
            .iter()
            .flat_map(|y| [cell(1.0, *y, 2.0, 2.0, 0.5), cell(4.5, *y, 6.5, 2.0, 1.0)])
            .collect(),
        Icon::Settings => {
            let mut parts = vec![];
            for (center, knob) in [(2.5, 2.5), (6.0, 5.5), (9.5, 3.5)] {
                // The bar stops short of the knob so the knob reads as round.
                let left = knob - 1.0 - 0.75;
                parts.push(cell(1.0, center - 0.75, left, 1.5, left.min(1.5) / 2.0));
                let start = knob + 3.5 + 0.75;
                parts.push(cell(
                    start,
                    center - 0.75,
                    11.0 - start,
                    1.5,
                    (11.0 - start).min(1.5) / 2.0,
                ));
                parts.push(cell(knob, center - 1.75, 3.5, 3.5, 1.75));
            }
            parts
        }
        #[cfg(target_os = "linux")]
        Icon::Drives => vec![
            cell(1.0, 2.0, 10.0, 3.5, 1.2),
            cell(1.0, 6.5, 10.0, 3.5, 1.2),
        ],
        #[cfg(target_os = "linux")]
        Icon::Recovery => vec![
            cell(1.5, 5.0, 2.0, 2.0, 0.4),
            cell(3.5, 3.5, 2.0, 5.0, 0.4),
            cell(5.5, 2.0, 2.0, 8.0, 0.4),
            cell(7.5, 5.0, 3.5, 2.0, 0.8),
        ],
        Icon::Close => {
            let diagonal = [(0.0, 0.0), (1.0, 1.0), (2.0, 2.0), (3.0, 3.0), (4.0, 4.0)];
            let other = [(4.0, 0.0), (3.0, 1.0), (1.0, 3.0), (0.0, 4.0)];
            let points: Vec<(f32, f32)> = diagonal.iter().chain(other.iter()).copied().collect();
            steps(&points, 2.0, 2.6, 0.7, 0.7)
        }
        #[cfg(any(target_os = "linux", windows))]
        Icon::Check => {
            let points = [
                (0.0, 2.0),
                (1.0, 3.0),
                (2.0, 4.0),
                (3.0, 3.0),
                (4.0, 2.0),
                (5.0, 1.0),
                (6.0, 0.0),
            ];
            steps(&points, 1.5, 2.2, 0.5, 2.0)
        }
        Icon::Dot => vec![cell(2.0, 2.0, 8.0, 8.0, 4.0)],
        Icon::ChevronClosed => vec![
            cell(3.0, 1.0, 1.75, 10.0, 0.3),
            cell(4.75, 2.25, 1.75, 7.5, 0.3),
            cell(6.5, 3.5, 1.75, 5.0, 0.3),
            cell(8.25, 4.75, 1.75, 2.5, 0.3),
        ],
        Icon::ChevronOpen => vec![
            cell(1.0, 3.0, 10.0, 1.75, 0.3),
            cell(2.25, 4.75, 7.5, 1.75, 0.3),
            cell(3.5, 6.5, 5.0, 1.75, 0.3),
            cell(4.75, 8.25, 2.5, 1.75, 0.3),
        ],
    }
}

/// An icon `size` pixels on a side, painted in `tint`.
pub fn icon<'a, Message: 'a>(icon: Icon, size: f32, tint: Tint) -> Element<'a, Message> {
    Element::new(Painted { icon, size, tint })
}

/// The widget behind [`icon`].
struct Painted {
    icon: Icon,
    size: f32,
    tint: Tint,
}

impl<Message> Widget<Message, Theme, iced::Renderer> for Painted {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.size), Length::Fixed(self.size))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fixed(self.size), Length::Fixed(self.size))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer;
        let color: Color = match self.tint {
            Tint::Inherit => style.text_color,
            Tint::Accent => super::theme::accent_color(theme),
            Tint::Danger => super::theme::danger_color(theme),
            #[cfg(any(target_os = "linux", windows))]
            Tint::Muted => super::theme::muted_color(theme),
        };
        let bounds = layout.bounds();
        for rect in shapes(self.icon) {
            let left = (bounds.x + rect.x * bounds.width).round();
            let top = (bounds.y + rect.y * bounds.height).round();
            let right = (bounds.x + (rect.x + rect.w) * bounds.width).round();
            let bottom = (bounds.y + (rect.y + rect.h) * bounds.height).round();
            let (width, height) = ((right - left).max(1.0), (bottom - top).max(1.0));
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        iced::Point::new(left, top),
                        Size::new(width, height),
                    ),
                    border: Border {
                        radius: (rect.radius * bounds.width.min(bounds.height)).into(),
                        ..Border::default()
                    },
                    shadow: Shadow::default(),
                    snap: true,
                },
                color,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestResult, check, check_eq};

    /// Every icon this platform draws.
    fn all() -> Vec<Icon> {
        vec![
            Icon::Overview,
            Icon::Games,
            Icon::Jobs,
            Icon::Settings,
            Icon::Close,
            #[cfg(any(target_os = "linux", windows))]
            Icon::Check,
            Icon::Dot,
            Icon::ChevronClosed,
            Icon::ChevronOpen,
            #[cfg(target_os = "linux")]
            Icon::Drives,
            #[cfg(target_os = "linux")]
            Icon::Recovery,
        ]
    }

    #[test]
    fn every_icon_has_shapes_inside_the_box() -> TestResult {
        for icon in all() {
            let rects = shapes(icon);
            check(!rects.is_empty(), format!("{icon:?} has shapes"))?;
            for rect in rects {
                check(
                    rect.w > 0.0 && rect.h > 0.0 && rect.radius >= 0.0,
                    format!("{icon:?} has a positive size: {rect:?}"),
                )?;
                check(
                    rect.x >= 0.0
                        && rect.y >= 0.0
                        && rect.x + rect.w <= 1.0 + 1e-6
                        && rect.y + rect.h <= 1.0 + 1e-6,
                    format!("{icon:?} stays inside the box: {rect:?}"),
                )?;
                check(
                    rect.radius <= rect.w.min(rect.h) / 2.0 + 1e-6,
                    format!("{icon:?} has a corner no larger than half its side: {rect:?}"),
                )?;
            }
        }
        Ok(())
    }

    #[test]
    fn icons_differ_from_each_other() -> TestResult {
        let icons = all();
        for (index, first) in icons.iter().enumerate() {
            for second in icons.iter().skip(index + 1) {
                check(
                    shapes(*first) != shapes(*second),
                    format!("{first:?} and {second:?} differ"),
                )?;
            }
        }
        check_eq(
            shapes(Icon::Dot),
            shapes(Icon::Dot),
            "control: an icon equals itself",
        )
    }

    #[test]
    fn the_two_chevrons_point_different_ways() -> TestResult {
        let widest = |icon| {
            shapes(icon)
                .iter()
                .map(|rect| rect.w)
                .fold(0.0_f32, f32::max)
        };
        let tallest = |icon| {
            shapes(icon)
                .iter()
                .map(|rect| rect.h)
                .fold(0.0_f32, f32::max)
        };
        check(
            widest(Icon::ChevronOpen) > tallest(Icon::ChevronOpen),
            "the open arrow is wide at the top",
        )?;
        check(
            tallest(Icon::ChevronClosed) > widest(Icon::ChevronClosed),
            "the closed arrow is tall at the left",
        )
    }
}

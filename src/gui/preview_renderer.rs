//! Offscreen desktop rendering shared by Linux and native fixture tests.
use super::theme;
use crate::testutil::{Ctx, TestResult, check};
use iced::{Color, Rectangle, Size};
use iced_tiny_skia::core::{Layout, clipboard, layout, mouse, renderer::Style, widget::Tree};

/// Lays out and draws `element` with the software renderer into a pixmap.
///
/// One `RedrawRequested` event goes through `update` before the draw. Buttons,
/// checkboxes and text inputs take their status from that event and draw as
/// disabled until they have seen it.
fn pixels<Message>(
    mut element: iced::Element<'_, Message>,
    palette: &iced::Theme,
    width: u32,
    height: u32,
) -> Result<tiny_skia::Pixmap, String> {
    let widget = element.as_widget_mut();
    let mut tree = Tree::new(&*widget);
    let mut renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
        theme::BODY_FONT,
        iced::Pixels(16.),
    ));
    let size = Size::new(width as f32, height as f32);
    let viewport = Rectangle::with_size(size);
    let node = widget.layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, size));
    let mut messages = Vec::new();
    let mut shell = iced::advanced::Shell::new(&mut messages);
    widget.update(
        &mut tree,
        &iced::Event::Window(iced::window::Event::RedrawRequested(
            std::time::Instant::now(),
        )),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut clipboard::Null,
        &mut shell,
        &viewport,
    );
    widget.draw(
        &tree,
        &mut renderer,
        palette,
        &Style {
            text_color: Color::WHITE,
        },
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &viewport,
    );
    let iced::Renderer::Secondary(mut renderer) = renderer else {
        return Err("Fixture selected an unexpected renderer".into());
    };
    let mut pixels = tiny_skia::Pixmap::new(width, height).ctx("preview pixels")?;
    let mut mask = tiny_skia::Mask::new(width, height).ctx("preview mask")?;
    let viewport =
        iced_tiny_skia::graphics::Viewport::with_physical_size(Size::new(width, height), 1.);
    renderer.draw(
        &mut pixels.as_mut(),
        &mut mask,
        &viewport,
        &[Rectangle::with_size(size)],
        Color::BLACK,
    );
    Ok(pixels)
}

/// How many pixels in the page area differ from the page's own background.
///
/// The area is everything right of the 232 pixels that hold the sidebar and
/// the page gutter, and left of the right gutter, whose pixel supplies the
/// background. A window with a sidebar and a blank page gives zero.
fn page_ink(pixels: &tiny_skia::Pixmap) -> usize {
    let width = pixels.width() as usize;
    let rows = || pixels.data().chunks_exact(width * 4);
    let background = rows()
        .nth(2)
        .and_then(|row| row.get(width.saturating_sub(4) * 4..width.saturating_sub(3) * 4));
    let Some(background) = background else {
        return 0;
    };
    rows()
        .filter_map(|row| row.get(232 * 4..width.saturating_sub(8) * 4))
        .map(|span| {
            span.as_chunks::<4>()
                .0
                .iter()
                .filter(|found| found.as_slice() != background)
                .count()
        })
        .sum()
}

/// Smallest `page_ink` that counts as a page with content on it. A single
/// heading is several hundred pixels.
const MINIMUM_INK: usize = 300;

/// Lays out and draws `element` with the software renderer and saves a PNG
/// at `path`. Fails when the page area is blank, which is what a layout that
/// drew nothing looks like.
pub fn render<Message>(
    element: iced::Element<'_, Message>,
    palette: iced::Theme,
    width: u32,
    height: u32,
    path: &std::path::Path,
) -> TestResult {
    let mut pixels = pixels(element, &palette, width, height)?;
    let ink = page_ink(&pixels);
    check(
        ink >= MINIMUM_INK,
        format!(
            "{} has content in its page area, found {ink} pixels",
            path.display()
        ),
    )?;
    // Iced renders BGRA for its desktop surface; PNG requires RGBA.
    for pixel in pixels.data_mut().as_chunks_mut::<4>().0 {
        let [blue, _, red, _] = pixel;
        std::mem::swap(blue, red);
    }
    image::save_buffer(path, pixels.data(), width, height, image::ColorType::Rgba8)
        .ctx("save headless preview")
}

#[cfg(target_os = "linux")]
mod tests {
    use super::*;
    use iced::widget::{button, column, container, text};

    #[test]
    fn a_blank_page_fails_the_ink_check_and_a_drawn_one_passes() -> TestResult {
        let palette = theme::theme(true);
        let blank: iced::Element<'_, ()> = container(iced::widget::Space::new())
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .style(theme::app_background)
            .into();
        let empty = pixels(blank, &palette, 1100, 700)?;
        check(page_ink(&empty) == 0, "control: a blank window has no ink")?;
        let drawn: iced::Element<'_, ()> = container(column![
            text("A heading that is drawn").size(26),
            text("A line under it")
        ])
        .padding(iced::Padding::default().left(560.0))
        .width(iced::Length::Fill)
        .height(iced::Length::Fill)
        .style(theme::app_background)
        .into();
        let page = pixels(drawn, &palette, 1100, 700)?;
        check(
            page_ink(&page) >= MINIMUM_INK,
            "text in the page area counts",
        )
    }

    /// The pixel at the centre of the button's face, which differs between its
    /// enabled and disabled styles.
    fn button_face(enabled: bool) -> Result<[u8; 4], String> {
        let palette = theme::theme(true);
        let label = button(text("Compress"))
            .padding(40)
            .style(theme::action_button)
            .on_press_maybe(enabled.then_some(()));
        let element: iced::Element<'_, ()> = container(label)
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
            .style(theme::app_background)
            .into();
        let drawn = pixels(element, &palette, 300, 200)?;
        let face = drawn.pixel(60, 100).ctx("button face pixel")?;
        Ok([face.blue(), face.green(), face.red(), face.alpha()])
    }

    #[test]
    fn widgets_draw_in_their_real_enabled_state() -> TestResult {
        let enabled = button_face(true)?;
        let disabled = button_face(false)?;
        crate::testutil::check_ne(
            enabled,
            disabled,
            "an enabled button and a disabled one draw differently",
        )
    }
}

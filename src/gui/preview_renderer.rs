//! Offscreen desktop rendering shared by Linux and native fixture tests.
use super::theme;
use crate::testutil::{Ctx, TestResult, check};
use iced::{Color, Rectangle, Size};
use iced_tiny_skia::core::{Layout, layout, mouse, renderer::Style, widget::Tree};
pub fn render<Message>(
    mut element: iced::Element<'_, Message>,
    palette: iced::Theme,
    width: u32,
    height: u32,
    path: &std::path::Path,
) -> TestResult {
    let widget = element.as_widget_mut();
    let mut tree = Tree::new(&*widget);
    let mut renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
        theme::BODY_FONT,
        iced::Pixels(16.),
    ));
    let size = Size::new(width as f32, height as f32);
    let node = widget.layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, size));
    widget.draw(
        &tree,
        &mut renderer,
        &palette,
        &Style {
            text_color: Color::WHITE,
        },
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &Rectangle::with_size(size),
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
    check(
        pixels
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel.first().is_some_and(|value| *value > 40)),
        "desktop widgets render visible content",
    )?;
    // Iced renders BGRA for its desktop surface; PNG requires RGBA.
    for pixel in pixels.data_mut().as_chunks_mut::<4>().0 {
        let [blue, _, red, _] = pixel;
        std::mem::swap(blue, red);
    }
    image::save_buffer(path, pixels.data(), width, height, image::ColorType::Rgba8)
        .ctx("save headless preview")
}

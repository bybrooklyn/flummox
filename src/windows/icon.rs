//! The tray icon, drawn from pixels at run time so no resource file is needed.
#![allow(unsafe_code)]
use windows_sys::Win32::{
    Graphics::Gdi::{CreateBitmap, DeleteObject},
    UI::WindowsAndMessaging::{CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, SM_CXSMICON},
};

/// The accent green of the window.
const GREEN: u32 = 0xFF55_C887;
const WHITE: u32 = 0xFFFF_FFFF;
const CLEAR: u32 = 0;

/// The mark as top-down 0xAARRGGBB pixels, `size` wide and high. `size` must be a
/// multiple of 16. It is a rounded green square with a white F made of three
/// rectangles, laid out on a 16 by 16 grid and scaled up.
pub fn mark_pixels(size: usize) -> Vec<u32> {
    let scale = (size / 16).max(1);
    // The F in 16-grid cells as (left, top, width, height): stem, top bar, middle bar.
    const LETTER: [(usize, usize, usize, usize); 3] = [(5, 4, 2, 8), (5, 4, 7, 2), (5, 7, 5, 2)];
    let radius = size / 5;
    let mut pixels = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let in_letter = LETTER.iter().any(|&(left, top, width, height)| {
                (left * scale..(left + width) * scale).contains(&x)
                    && (top * scale..(top + height) * scale).contains(&y)
            });
            pixels.push(if !inside_rounded_square(x, y, size, radius) {
                CLEAR
            } else if in_letter {
                WHITE
            } else {
                GREEN
            });
        }
    }
    pixels
}

/// True when the pixel centre is inside a square of `size` with rounded corners.
fn inside_rounded_square(x: usize, y: usize, size: usize, radius: usize) -> bool {
    // Twice the distance from the nearest corner circle's centre, so that pixel
    // centres at half units stay in integers.
    let near = |value: usize| {
        let doubled = 2 * value + 1;
        if doubled < 2 * radius {
            2 * radius - doubled
        } else {
            doubled.saturating_sub(2 * (size - radius))
        }
    };
    let (dx, dy) = (near(x), near(y));
    dx * dx + dy * dy <= 4 * radius * radius
}

/// Builds the icon at the size the notification area uses, 32 pixels on a scaled
/// display and 16 otherwise. Returns `None` if Windows refuses any step. The caller
/// owns the icon and frees it with `DestroyIcon`.
pub fn create() -> Option<HICON> {
    // SAFETY: GetSystemMetrics takes a constant index and no pointers.
    let small = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let size = if small >= 24 { 32 } else { 16 };
    let pixels = mark_pixels(size);
    let side = i32::try_from(size).ok()?;
    // A 1-bit mask pads each row to 16 bits. All zero, since the colour bitmap's own
    // alpha decides what is drawn.
    let mask = vec![0u8; size.div_ceil(16) * 2 * size];
    // SAFETY: pixels holds side * side 32-bit values and mask holds the padded rows
    // of a 1-bit bitmap of the same size. CreateBitmap copies both before returning.
    let color = unsafe { CreateBitmap(side, side, 1, 32, pixels.as_ptr().cast()) };
    // SAFETY: as above.
    let shape = unsafe { CreateBitmap(side, side, 1, 1, mask.as_ptr().cast()) };
    let icon = if color.is_null() || shape.is_null() {
        None
    } else {
        let info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: shape,
            hbmColor: color,
        };
        // SAFETY: info names two live bitmaps. CreateIconIndirect copies them.
        let icon = unsafe { CreateIconIndirect(&info) };
        (!icon.is_null()).then_some(icon)
    };
    for bitmap in [color, shape] {
        if !bitmap.is_null() {
            // SAFETY: the bitmap was created above and nothing else refers to it.
            unsafe {
                DeleteObject(bitmap);
            }
        }
    }
    icon
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Ctx, TestResult, check_eq};

    fn at(pixels: &[u32], size: usize, x: usize, y: usize) -> Option<u32> {
        pixels.get(y * size + x).copied()
    }

    #[test]
    fn the_mark_is_a_green_square_with_a_white_f_at_both_sizes() -> TestResult {
        for size in [16usize, 32] {
            let pixels = mark_pixels(size);
            check_eq(pixels.len(), size * size, "one value per pixel")?;
            let scale = size / 16;
            check_eq(at(&pixels, size, 0, 0), Some(CLEAR), "the corner is clear")?;
            check_eq(
                at(&pixels, size, size / 2, 1),
                Some(GREEN),
                "the top edge is green",
            )?;
            check_eq(
                at(&pixels, size, 5 * scale, 5 * scale),
                Some(WHITE),
                "the stem is white",
            )?;
            check_eq(
                at(&pixels, size, 11 * scale, 4 * scale),
                Some(WHITE),
                "the top bar reaches right",
            )?;
            check_eq(
                at(&pixels, size, 9 * scale, 7 * scale),
                Some(WHITE),
                "the middle bar is white",
            )?;
            check_eq(
                at(&pixels, size, 11 * scale, 7 * scale),
                Some(GREEN),
                "control: the middle bar is shorter than the top bar",
            )?;
            check_eq(
                at(&pixels, size, 9 * scale, 5 * scale + scale),
                Some(GREEN),
                "control: the gap between the bars is green",
            )?;
        }
        Ok(())
    }

    #[test]
    fn windows_accepts_the_drawn_icon() -> TestResult {
        let icon = create().ctx("the icon is created")?;
        // SAFETY: icon was returned by create and is destroyed once.
        let destroyed = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon(icon) };
        check_eq(destroyed != 0, true, "the icon is a live handle")
    }
}

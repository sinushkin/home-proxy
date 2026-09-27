//! QR-код ссылки сопряжения в картинку Slint: чёрное на белом с полем в 4 модуля (как требует
//! стандарт — иначе камеры читают код хуже).

use anyhow::Result;
use qrcode::{Color, EcLevel, QrCode};
use slint::{Image, Rgb8Pixel, SharedPixelBuffer};

/// Пикселей на модуль: картинка масштабируется окном без сглаживания.
const SCALE: usize = 4;
/// Белое поле вокруг кода, в модулях.
const QUIET: usize = 4;

pub fn image(text: &str) -> Result<Image> {
    // Уровень M: ссылка ~1 КБ умещается, а код переживает блики на экране.
    let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::M)?;
    let width = code.width();
    let colors = code.to_colors();
    let modules = width + 2 * QUIET;
    let size = modules * SCALE;
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(size as u32, size as u32);
    let pixels = buffer.make_mut_slice();
    for y in 0..size {
        for x in 0..size {
            let (mx, my) = ((x / SCALE).wrapping_sub(QUIET), (y / SCALE).wrapping_sub(QUIET));
            let dark = mx < width && my < width && colors[my * width + mx] == Color::Dark;
            let v = if dark { 0 } else { 255 };
            pixels[y * size + x] = Rgb8Pixel { r: v, g: v, b: v };
        }
    }
    Ok(Image::from_rgb8(buffer))
}

//! Значок трея: круг цвета состояния (рисуется в памяти, без файлов).

/// Размер значка в пикселях.
pub const SIZE: u32 = 32;

/// Цвет уровня: 0 — нет связи со службой, 1 — нет дыр, 2 — частично / пробив, 3 — всё хорошо.
fn color(level: u8) -> [u8; 3] {
    match level {
        3 => [0x2e, 0xa0, 0x43],
        2 => [0xe3, 0xa0, 0x08],
        1 => [0xd9, 0x53, 0x4f],
        _ => [0x8a, 0x8f, 0x98],
    }
}

/// RGBA, строка за строкой: цветной круг с белой точкой в центре и сглаженным краем.
pub fn rgba(level: u8) -> Vec<u8> {
    let [r, g, b] = color(level);
    let center = (SIZE as f32 - 1.0) / 2.0;
    let radius = SIZE as f32 / 2.0 - 1.0;
    let mut data = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt();
            let alpha = (radius + 0.5 - d).clamp(0.0, 1.0);
            let inner = d < radius * 0.35;
            let (pr, pg, pb) = if inner { (255, 255, 255) } else { (r, g, b) };
            data.extend_from_slice(&[pr, pg, pb, (alpha * 255.0) as u8]);
        }
    }
    data
}

#[cfg(test)]
mod tests {
    #[test]
    fn corners_are_transparent_center_is_opaque() {
        let data = super::rgba(3);
        assert_eq!(data.len(), (super::SIZE * super::SIZE * 4) as usize);
        assert_eq!(data[3], 0, "угол прозрачный");
        let center = ((super::SIZE / 2) * super::SIZE + super::SIZE / 2) as usize * 4;
        assert_eq!(data[center + 3], 255);
    }
}

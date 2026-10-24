use eframe::egui::IconData;

const SIZE: u32 = 256;

/// Horseshoe magnet used for the window / dock icon.
pub fn app_icon() -> IconData {
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    fill(&mut rgba, [10, 10, 10, 255]);

    let cx = 128i32;
    let cy = 148i32;
    let outer = 90i32;
    let inner = 46i32;
    let top = 40i32;
    let cap = 38i32;
    let body = [110, 255, 110, 255];
    let pole = [220, 230, 220, 255];

    for y in 0..SIZE as i32 {
        for x in 0..SIZE as i32 {
            let dx = x - cx;
            let dy = y - cy;
            let dist2 = dx * dx + dy * dy;
            let in_bend = dy >= 0 && dist2 <= outer * outer && dist2 >= inner * inner;
            let in_left = y > top && y <= cy && dx >= -outer && dx <= -inner;
            let in_right = y > top && y <= cy && dx >= inner && dx <= outer;
            if in_bend || in_left || in_right {
                let is_cap = (in_left || in_right) && y < top + cap;
                put(&mut rgba, x, y, if is_cap { pole } else { body });
            }
        }
    }

    IconData {
        rgba,
        width: SIZE,
        height: SIZE,
    }
}

fn put(buf: &mut [u8], x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
        return;
    }
    let i = ((y as u32 * SIZE + x as u32) * 4) as usize;
    buf[i..i + 4].copy_from_slice(&color);
}

fn fill(buf: &mut [u8], color: [u8; 4]) {
    for pixel in buf.chunks_exact_mut(4) {
        pixel.copy_from_slice(&color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_square_rgba() {
        let icon = app_icon();
        assert_eq!(icon.width, 256);
        assert_eq!(icon.height, 256);
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert!(icon.rgba.chunks(4).any(|px| px[1] > 200 && px[0] < 140));
    }
}

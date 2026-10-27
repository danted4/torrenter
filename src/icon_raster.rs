//! Shared dock / taskbar glyph: squircle plate + horseshoe magnet.

const PLATE: [u8; 4] = [16, 16, 16, 255];
const BODY: [u8; 4] = [110, 255, 110, 255];
const POLE: [u8; 4] = [220, 230, 220, 255];
const SAMPLES: i32 = 3;
const SQUIRCLE_N: f32 = 5.0;
const CONTENT_PAD: f32 = 0.18;

pub fn raster(size: u32) -> Vec<u8> {
    let mut rgba = vec![0_u8; (size as usize) * (size as usize) * 4];
    let s = size as f32;
    for y in 0..size as i32 {
        for x in 0..size as i32 {
            let mut plate = 0.0_f32;
            let mut body = 0.0_f32;
            let mut pole = 0.0_f32;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    if squircle(px, py, s) > 1.0 {
                        continue;
                    }
                    plate += 1.0;
                    match magnet_part(px, py, s) {
                        MagnetPart::Pole => pole += 1.0,
                        MagnetPart::Body => body += 1.0,
                        MagnetPart::None => {}
                    }
                }
            }
            let denom = (SAMPLES * SAMPLES) as f32;
            plate /= denom;
            body /= denom;
            pole /= denom;
            if plate <= 0.0 {
                continue;
            }
            let mut color = PLATE;
            if body > 0.0 {
                color = lerp(color, BODY, (body / plate).clamp(0.0, 1.0));
            }
            if pole > 0.0 {
                color = lerp(color, POLE, (pole / plate).clamp(0.0, 1.0));
            }
            color[3] = (plate * 255.0).round() as u8;
            put(&mut rgba, size, x, y, color);
        }
    }
    rgba
}

enum MagnetPart {
    None,
    Body,
    Pole,
}

fn squircle(x: f32, y: f32, size: f32) -> f32 {
    let c = size * 0.5;
    let nx = (x - c) / c;
    let ny = (y - c) / c;
    nx.abs().powf(SQUIRCLE_N) + ny.abs().powf(SQUIRCLE_N)
}

fn magnet_part(x: f32, y: f32, size: f32) -> MagnetPart {
    let pad = size * CONTENT_PAD;
    let inner = size - 2.0 * pad;
    if inner <= 0.0 {
        return MagnetPart::None;
    }
    let u = (x - pad) / inner * 256.0;
    let v = (y - pad) / inner * 256.0;
    let dx = u - 128.0;
    let dy = v - 148.0;
    let dist2 = dx * dx + dy * dy;
    let outer = 90.0;
    let hole = 46.0;
    let top = 40.0;
    let cap = 38.0;
    let in_bend = dy >= 0.0 && dist2 <= outer * outer && dist2 >= hole * hole;
    let in_left = v > top && v <= 148.0 && dx >= -outer && dx <= -hole;
    let in_right = v > top && v <= 148.0 && dx >= hole && dx <= outer;
    if !(in_bend || in_left || in_right) {
        return MagnetPart::None;
    }
    if (in_left || in_right) && v < top + cap {
        MagnetPart::Pole
    } else {
        MagnetPart::Body
    }
}

fn lerp(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
    let mix = |x, y| ((x as f32) * (1.0 - t) + (y as f32) * t).round() as u8;
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2]), 255]
}

fn put(buf: &mut [u8], size: u32, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= size as i32 || y >= size as i32 {
        return;
    }
    let i = ((y as u32 * size + x as u32) * 4) as usize;
    buf[i..i + 4].copy_from_slice(&color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_transparent_and_magnet_is_green() {
        let icon = raster(64);
        let a = |x: u32, y: u32| icon[((y * 64 + x) * 4 + 3) as usize];
        assert_eq!(a(0, 0), 0);
        assert_eq!(a(63, 0), 0);
        assert_eq!(a(0, 63), 0);
        assert_eq!(a(63, 63), 0);
        assert!(icon
            .chunks(4)
            .any(|px| px[1] > 200 && px[0] < 140 && px[3] > 200));
        let mid = ((32 * 64 + 32) * 4) as usize;
        assert!(icon[mid + 3] > 200, "plate should fill the center");
    }
}

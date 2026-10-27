use eframe::egui::IconData;

use crate::icon_raster;

const SIZE: u32 = 256;

/// Squircle magnet used for the window / dock icon.
pub fn app_icon() -> IconData {
    IconData {
        rgba: icon_raster::raster(SIZE),
        width: SIZE,
        height: SIZE,
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
        assert_eq!(
            icon.rgba[3], 0,
            "corners must be transparent for a round dock tile"
        );
        assert!(icon
            .rgba
            .chunks(4)
            .any(|px| px[1] > 200 && px[0] < 140 && px[3] > 200));
    }
}

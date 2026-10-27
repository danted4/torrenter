pub mod http;
pub mod identity;
pub mod paths;
pub mod progress;
pub mod torrent;
pub mod update;
pub mod validate;

#[cfg(feature = "gui")]
pub mod gui;
#[cfg(feature = "gui")]
pub mod icon;
#[cfg(feature = "gui")]
/// Shared dock / taskbar glyph: squircle plate + horseshoe magnet.
mod icon_raster;

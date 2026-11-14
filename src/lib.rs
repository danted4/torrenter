pub mod credentials;
pub mod debrid;
pub(crate) mod fsutil;
pub mod http;
pub mod identity;
pub mod jobs;
pub mod paths;
pub mod progress;
pub mod session;
pub mod torrent;
pub mod update;
pub mod validate;

#[cfg(feature = "ffi")]
uniffi::setup_scaffolding!();
#[cfg(feature = "ffi")]
pub mod ffi;

#[cfg(feature = "gui")]
mod glass;
#[cfg(feature = "gui")]
pub mod gui;
#[cfg(feature = "gui")]
pub mod icon;
#[cfg(feature = "gui")]
/// Shared dock / taskbar glyph: squircle plate + horseshoe magnet.
mod icon_raster;

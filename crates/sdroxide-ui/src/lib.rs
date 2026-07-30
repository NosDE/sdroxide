//! The sdroxide GUI: an egui app that talks to any [`sdroxide_types::RadioController`].
//!
//! Compiles native and for wasm32. All custom wgpu rendering that the browser
//! build shares is written to WebGL2 downlevel limits (fragment-only, sampled
//! textures + uniforms). The one exception is [`solar3d`], which uses depth,
//! MSAA and vertex buffers — but does so entirely inside an offscreen pass of
//! its own, so the shared egui pass never sees them and the module runs on both
//! targets.

mod app;
pub mod chrome;
mod colormap;
mod digi_map;
mod download;
mod fuzzy;
mod hell;
mod help;
mod input;
#[cfg(feature = "remote")]
mod remote;
mod rf_paint;
/// Solar-system 3D view. A second OS window natively, a second browser tab on
/// the web; the sole consumer of depth/MSAA/vertex buffers either way.
mod solar3d;
mod sstv;
pub mod theme;
mod time;
mod view;
mod waterfall_gpu;
mod wefax;
mod widgets;

pub use app::SdroxideApp;
#[cfg(feature = "remote")]
pub use remote::{AudioBridge, RemoteController};
/// The solar-system view as a standalone app, for the browser tab the ☀ 3D
/// chip opens. Natively the same view is a child viewport of the main window
/// instead — see `solar3d::Solar3d`.
#[cfg(feature = "remote")]
pub use solar3d::SolarApp;

/// Wgpu access must go through this re-export so every crate agrees on the
/// wgpu version (project rule).
pub use eframe::egui_wgpu;

/// The application icon, for [`eframe::egui::ViewportBuilder::with_icon`].
///
/// This is what window managers show in the taskbar/dock and in alt-tab; the
/// desktop-menu entry gets its icon from the installed hicolor theme instead
/// (see `packaging/`). Both come from `packaging/icons/sdroxide.svg`.
#[cfg(not(target_arch = "wasm32"))]
pub fn app_icon() -> eframe::egui::IconData {
    const PNG: &[u8] = include_bytes!("../../../packaging/icons/sdroxide-256.png");
    // Decoding a 256x256 PNG once at startup; a failure here would only cost
    // the icon, so fall back to no icon rather than refusing to open a window.
    match image::load_from_memory_with_format(PNG, image::ImageFormat::Png) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            eframe::egui::IconData { rgba: rgba.into_raw(), width, height }
        }
        Err(e) => {
            eprintln!("sdroxide: decoding the app icon: {e}");
            eframe::egui::IconData::default()
        }
    }
}

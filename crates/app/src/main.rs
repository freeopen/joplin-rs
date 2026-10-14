//! joplin-rs GUI: read-only note browser.
//!
//! Single GPUI frontend via the gpui-kit facade (gpui-pre 0.3.9 +
//! gpui-base + gpui-component). The framework-agnostic state machine lives
//! in [`app`], background jobs in [`jobs`], and all presentation in [`view`].

mod app;
mod jobs;
mod read_password;
mod view;

#[cfg(target_os = "macos")]
mod macos_about;

/// Human-facing version (see core::VERSION_DISPLAY), e.g. `v1.0.0 (b30d7ede3)`.
const APP_VERSION: &str = joplin_rs_core::VERSION_DISPLAY;

/// Official Joplin app icon (512x512 PNG), embedded for the window/taskbar
/// icon and the About dialog. Sourced from joplin-dev Assets/SquareIcon512.png.
const APP_ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn main() {
    view::gpui_main();
}

#[cfg(test)]
mod tests {
    use super::APP_ICON_PNG;

    #[test]
    fn embedded_icon_decodes_to_square_rgba() {
        // Guards the window icon: the PNG must decode to a square image so
        // the RgbaImage handed to WindowOptions is always square.
        let img = image::load_from_memory_with_format(APP_ICON_PNG, image::ImageFormat::Png)
            .expect("icon png decodes");
        assert_eq!(img.width(), img.height());
        assert!(img.width() >= 64);
    }
}

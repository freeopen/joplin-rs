//! Core library for the joplin-rs tools.

/// Full build version derived from git by `build.rs`
/// (plain `M.I.P`); falls back to the Cargo.toml version outside a git
/// checkout.
pub const GIT_VERSION: &str = match option_env!("JOPLIN_RS_GIT_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

pub mod crypto;
pub mod error;
pub mod items;
pub mod secret;
pub mod storage;
pub mod sync;
pub mod types;
pub mod webdav;

/// Current unix time in milliseconds.
pub fn time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

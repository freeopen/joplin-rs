pub mod crypto;
pub mod error;
pub mod items;
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

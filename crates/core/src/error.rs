#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("webdav: {0}")]
    WebDav(String),
    #[error("sync: {0}")]
    Sync(String),
    #[error("storage: {0}")]
    Storage(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

use std::path::PathBuf;

/// Errors produced by UnionDesk crates.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid configuration at {path}: {source}")]
    Config {
        path: PathBuf,
        #[source]
        source: Box<Error>,
    },

    #[error("invalid base64 payload: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error("invalid hex payload: {0}")]
    Hex(#[from] hex::FromHexError),

    #[error("invalid key material: {0}")]
    Key(String),

    #[error("invalid argument: {0}")]
    Invalid(String),

    #[error("peer not found: {0}")]
    UnknownPeer(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}

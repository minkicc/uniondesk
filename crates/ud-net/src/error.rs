#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("cryptographic error: {0}")]
    Crypto(String),

    #[error("the remote peer spoke an incompatible protocol version ({remote}, we speak {local})")]
    Version { local: u32, remote: u32 },

    #[error("the peer identity changed: expected key {expected}, received {actual}")]
    IdentityChanged { expected: String, actual: String },

    #[error("pairing was rejected: {0}")]
    PairRejected(String),

    #[error("connection closed: {0}")]
    Closed(String),

    #[error("value is out of range: {0}")]
    Range(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }

    pub fn crypto(msg: impl Into<String>) -> Self {
        Error::Crypto(msg.into())
    }
}

impl From<ud_core::Error> for Error {
    fn from(value: ud_core::Error) -> Self {
        match value {
            ud_core::Error::Json(err) => Error::Protocol(err.to_string()),
            ud_core::Error::Io(err) => Error::Io(err),
            other => Error::Other(other.to_string()),
        }
    }
}

impl From<snow::Error> for Error {
    fn from(value: snow::Error) -> Self {
        Error::Crypto(value.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::Protocol(value.to_string())
    }
}

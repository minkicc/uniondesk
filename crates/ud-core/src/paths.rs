use std::path::PathBuf;

use crate::{Error, Result};

/// Root directory holding configuration, identity and trust material.
///
/// `UNIONDESK_CONFIG_DIR` overrides the platform default, which is useful for
/// tests and for running several instances on a single machine.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("UNIONDESK_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(crate::PRODUCT_NAME)
}

/// Directory used as the default destination for received files.
pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("UnionDesk")
}

pub fn ensure_dir(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(Error::Io)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| Error::Config {
                path: path.to_path_buf(),
                source: Box::new(Error::Json(err)),
            }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::Io(err)),
    }
}

/// Writes JSON atomically (temp file + rename) so a crash cannot leave a
/// half-written identity file behind.
pub fn write_json<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

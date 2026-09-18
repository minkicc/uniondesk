use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths;
use crate::{Error, Result};

/// Stable identifier for a machine, independent of its network address.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct DeviceId(pub String);

impl DeviceId {
    pub fn new() -> Self {
        DeviceId(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Short form used in logs and in the UI.
    pub fn short(&self) -> &str {
        self.0.split('-').next().unwrap_or(&self.0)
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for DeviceId {
    fn from(value: &str) -> Self {
        DeviceId(value.to_string())
    }
}

impl From<String> for DeviceId {
    fn from(value: String) -> Self {
        DeviceId(value)
    }
}

/// Long lived Noise static key material for this machine.
///
/// The private key never leaves the device and is stored with owner-only
/// permissions. Peers are pinned to the matching public key, which is what makes
/// reconnect automatic after the first pairing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub version: u32,
    pub device_id: DeviceId,
    pub created_at: u64,
    private_key: String,
    public_key: String,
}

impl DeviceIdentity {
    pub const FILE_VERSION: u32 = 1;

    pub fn generate(device_id: DeviceId) -> Result<Self> {
        let params = crate::protocol::noise_params()?;
        let keypair = snow::Builder::new(params)
            .generate_keypair()
            .map_err(|e| Error::Key(e.to_string()))?;
        Ok(Self {
            version: Self::FILE_VERSION,
            device_id,
            created_at: crate::now_unix(),
            private_key: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &keypair.private,
            ),
            public_key: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &keypair.public,
            ),
        })
    }

    pub fn private_key(&self) -> Result<Vec<u8>> {
        let raw = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            self.private_key.as_bytes(),
        )?;
        if raw.len() != 32 {
            return Err(Error::Key(format!(
                "expected a 32 byte private key, found {}",
                raw.len()
            )));
        }
        Ok(raw)
    }

    pub fn public_key_raw(&self) -> Result<Vec<u8>> {
        let raw = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            self.public_key.as_bytes(),
        )?;
        Ok(raw)
    }

    pub fn public_key_base64(&self) -> &str {
        &self.public_key
    }

    pub fn fingerprint(&self) -> String {
        let bytes = self
            .public_key_raw()
            .unwrap_or_default();
        fingerprint_of(&bytes)
    }

    /// Loads the identity from `path`, creating a fresh one on first run.
    pub fn load_or_create(path: &std::path::Path) -> Result<Self> {
        if let Some(existing) = paths::read_json::<DeviceIdentity>(path)? {
            return Ok(existing);
        }
        let identity = Self::generate(DeviceId::new())?;
        paths::write_json(path, &identity)?;
        restrict_permissions(path);
        Ok(identity)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        paths::write_json(path, self)?;
        restrict_permissions(path);
        Ok(())
    }
}

/// Short human readable digest of a public key, shown during pairing.
pub fn fingerprint_of(public_key: &[u8]) -> String {
    let digest = Sha256::digest(public_key);
    let hexed = hex::encode(&digest[..8]).to_uppercase();
    hexed
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_identity_is_stable_and_well_formed() {
        let id = DeviceIdentity::generate(DeviceId::new()).unwrap();
        assert_eq!(id.private_key().unwrap().len(), 32);
        assert_eq!(id.public_key_raw().unwrap().len(), 32);
        assert_eq!(id.fingerprint().len(), "xxxx-xxxx-xxxx-xxxx".len());
    }

    #[test]
    fn short_id_is_first_group() {
        let id = DeviceId("a1b2c3d4-0000-0000-0000-000000000000".into());
        assert_eq!(id.short(), "a1b2c3d4");
    }
}

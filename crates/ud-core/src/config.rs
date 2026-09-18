use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::geom::{DisplayInfo, Side};
use crate::identity::DeviceId;
use crate::paths;
use crate::{DEFAULT_PORT, Result};

/// Describes how one neighbouring machine is arranged relative to this one.
///
/// `local_range` and `remote_range` are fractions (0.0 - 1.0) along the shared
/// edge, which lets a 27" display be aligned with only the lower half of a
/// stacked display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeLink {
    pub peer: DeviceId,
    #[serde(default)]
    pub peer_name: String,
    /// Edge of this machine's desktop that the peer sits beyond.
    pub local_side: Side,
    /// Edge of the peer's desktop that faces this machine.
    pub remote_side: Side,
    #[serde(default = "full_range")]
    pub local_range: (f64, f64),
    #[serde(default = "full_range")]
    pub remote_range: (f64, f64),
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn full_range() -> (f64, f64) {
    (0.0, 1.0)
}

fn yes() -> bool {
    true
}

impl EdgeLink {
    pub fn new(peer: DeviceId, peer_name: impl Into<String>, local_side: Side) -> Self {
        Self {
            peer,
            peer_name: peer_name.into(),
            local_side,
            remote_side: local_side.opposite(),
            local_range: full_range(),
            remote_range: full_range(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseHotkey {
    /// Two taps of Scroll Lock inside 400 ms. Chosen because the key is rarely
    /// used for anything else and exists on both Windows and macOS keyboards.
    ScrollLockTwice,
    /// Escape while holding ctrl+alt.
    CtrlAltEscape,
    /// Escape while holding ctrl+alt+cmd, the macOS friendly variant.
    CtrlAltCmdEscape,
    Disabled,
}

impl Default for ReleaseHotkey {
    fn default() -> Self {
        ReleaseHotkey::ScrollLockTwice
    }
}

impl ReleaseHotkey {
    pub fn label(self) -> &'static str {
        match self {
            ReleaseHotkey::ScrollLockTwice => "Scroll Lock twice",
            ReleaseHotkey::CtrlAltEscape => "Ctrl + Alt + Esc",
            ReleaseHotkey::CtrlAltCmdEscape => "Ctrl + Alt + Cmd + Esc",
            ReleaseHotkey::Disabled => "Disabled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputSettings {
    /// Master switch. While false, UnionDesk never hooks the input devices.
    pub enabled: bool,
    /// Multiplier applied to relayed mouse movement.
    pub mouse_speed: f64,
    pub relay_keyboard: bool,
    pub release_hotkey: ReleaseHotkey,
    /// How close to the edge the cursor must be before a handover is armed.
    pub edge_armed_pixels: f64,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mouse_speed: 1.0,
            relay_keyboard: true,
            release_hotkey: ReleaseHotkey::default(),
            edge_armed_pixels: 2.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardSettings {
    pub enabled: bool,
    pub sync_text: bool,
    pub sync_images: bool,
    pub poll_interval_ms: u64,
    /// Payloads above this size are ignored instead of being relayed.
    pub max_bytes: usize,
    /// Also relay while this machine is not the active controller.
    pub always: bool,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sync_text: true,
            sync_images: true,
            poll_interval_ms: 350,
            max_bytes: 8 * 1024 * 1024,
            always: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferSettings {
    pub enabled: bool,
    pub download_dir: PathBuf,
    pub auto_accept_trusted: bool,
    pub overwrite_existing: bool,
}

impl Default for TransferSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            download_dir: paths::default_download_dir(),
            auto_accept_trusted: true,
            overwrite_existing: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub device_name: String,
    pub port: u16,
    pub discoverable: bool,
    pub input: InputSettings,
    pub clipboard: ClipboardSettings,
    pub transfer: TransferSettings,
    pub links: Vec<EdgeLink>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            device_name: default_device_name(),
            port: DEFAULT_PORT,
            discoverable: true,
            input: InputSettings::default(),
            clipboard: ClipboardSettings::default(),
            transfer: TransferSettings::default(),
            links: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        Ok(paths::read_json::<Settings>(path)?.unwrap_or_default())
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        paths::write_json(path, self)
    }

    pub fn link_for(&self, peer: &DeviceId) -> Option<&EdgeLink> {
        self.links.iter().find(|l| &l.peer == peer)
    }

    pub fn set_link(&mut self, link: EdgeLink) {
        match self.links.iter_mut().find(|l| l.peer == link.peer) {
            Some(existing) => *existing = link,
            None => self.links.push(link),
        }
    }

    pub fn remove_link(&mut self, peer: &DeviceId) {
        self.links.retain(|l| &l.peer != peer);
    }
}

/// A machine we have paired with. The public key is pinned, which is what makes
/// reconnects silent and MITM attempts detectable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerRecord {
    pub device_id: DeviceId,
    pub name: String,
    pub public_key: String,
    pub fingerprint: String,
    pub os: crate::protocol::OsKind,
    #[serde(default)]
    pub last_seen: Option<u64>,
    #[serde(default)]
    pub last_address: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerStore {
    pub peers: Vec<PeerRecord>,
}

impl PeerStore {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        Ok(paths::read_json::<PeerStore>(path)?.unwrap_or_default())
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        paths::write_json(path, self)
    }

    pub fn get(&self, id: &DeviceId) -> Option<&PeerRecord> {
        self.peers.iter().find(|p| &p.device_id == id)
    }

    pub fn get_mut(&mut self, id: &DeviceId) -> Option<&mut PeerRecord> {
        self.peers.iter_mut().find(|p| &p.device_id == id)
    }

    pub fn is_trusted_key(&self, id: &DeviceId, public_key: &str) -> bool {
        self.get(id)
            .map(|p| p.public_key == public_key)
            .unwrap_or(false)
    }

    /// Inserts or updates a record and reports whether the stored key had to
    /// change, which the caller surfaces as a warning.
    pub fn upsert(&mut self, record: PeerRecord) -> KeyChange {
        match self.peers.iter_mut().find(|p| p.device_id == record.device_id) {
            Some(existing) => {
                let changed = existing.public_key != record.public_key;
                *existing = record;
                if changed {
                    KeyChange::Replaced
                } else {
                    KeyChange::Unchanged
                }
            }
            None => {
                self.peers.push(record);
                KeyChange::New
            }
        }
    }

    pub fn remove(&mut self, id: &DeviceId) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| &p.device_id != id);
        before != self.peers.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyChange {
    New,
    Unchanged,
    Replaced,
}

pub fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "UnionDesk device".to_string())
}

/// Everything the engine needs to know about the local machine at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalDisplays {
    pub displays: Vec<DisplayInfo>,
}

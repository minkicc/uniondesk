//! Read-only projections of engine state, shaped for the desktop UI.
//!
//! The UI simply redraws whatever snapshot arrives, which keeps the two sides
//! from drifting apart and removes a whole class of "the list did not update"
//! bugs.

use serde::{Deserialize, Serialize};

use ud_core::config::{EdgeLink, Settings};
use ud_core::geom::DisplayInfo;
use ud_core::identity::DeviceId;
use ud_core::protocol::{OsKind, TransferId};

/// Identity of the machine the engine is running on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceView {
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub fingerprint: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    Offline,
    Connecting,
    /// Connected but not yet trusted by both sides.
    Pairing,
    Connected,
    Failed { reason: String },
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }

    pub fn label(&self) -> &'static str {
        match self {
            ConnectionState::Offline => "Offline",
            ConnectionState::Connecting => "Connecting",
            ConnectionState::Pairing => "Pairing",
            ConnectionState::Connected => "Connected",
            ConnectionState::Failed { .. } => "Failed",
        }
    }
}

/// Pairing prompt state for one peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairingView {
    /// Shown on the machine that started the connection; the user reads it out.
    pub code_to_share: Option<String>,
    /// True when this machine has to ask the user for the code.
    pub awaiting_code: bool,
    pub remote_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerView {
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub fingerprint: String,
    pub trusted: bool,
    pub connection: ConnectionState,
    /// How the machine was found: Bonjour, broadcast or entered by hand.
    pub discovered_via: Option<String>,
    pub address: Option<String>,
    pub last_seen: Option<u64>,
    pub display_count: usize,
    pub display_summary: Option<String>,
    pub pairing: Option<PairingView>,
    pub link: Option<EdgeLink>,
}

impl PeerView {
    pub fn can_connect(&self) -> bool {
        matches!(
            self.connection,
            ConnectionState::Offline | ConnectionState::Failed { .. }
        ) && self.address.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ControlModeView {
    /// This machine is using its own keyboard and mouse.
    Local,
    /// This machine's input is being sent to a peer.
    Controlling { device_id: DeviceId, name: String },
    /// A peer is driving this machine.
    Controlled { device_id: DeviceId, name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlView {
    pub mode: ControlModeView,
    pub release_hotkey: String,
    pub sharing_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Sending,
    Receiving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    /// Offered by the peer, waiting for the local user to accept.
    Pending,
    Active,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferFileView {
    pub index: u32,
    pub name: String,
    pub relative_path: String,
    pub size: u64,
    pub done_bytes: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferView {
    pub id: TransferId,
    pub peer: DeviceId,
    pub peer_name: String,
    pub direction: TransferDirection,
    pub state: TransferState,
    pub files: Vec<TransferFileView>,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub bytes_per_second: u64,
    pub started_at: u64,
    pub destination: Option<String>,
    pub error: Option<String>,
    /// Set when a received transfer is waiting for the user to accept it.
    pub needs_acceptance: bool,
}

impl TransferView {
    pub fn progress(&self) -> f32 {
        if self.total_bytes == 0 {
            return if self.state == TransferState::Completed {
                1.0
            } else {
                0.0
            };
        }
        (self.done_bytes as f64 / self.total_bytes as f64).min(1.0) as f32
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputStatusView {
    /// False on platforms without a backend, so the UI can explain itself.
    pub backend_available: bool,
    pub enabled: bool,
    pub capture_active: bool,
    /// Set when the operating system needs the user to grant a permission.
    pub permission_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipboardStatusView {
    pub enabled: bool,
    pub last_kind: Option<String>,
    pub last_at: Option<u64>,
    pub last_direction: Option<TransferDirection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlatformView {
    pub os: OsKind,
    pub displays: Vec<DisplayInfo>,
    pub desktop: ud_core::geom::Rect,
}

/// Everything the UI needs to render one frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub device: DeviceView,
    pub settings: Settings,
    pub peers: Vec<PeerView>,
    pub control: ControlView,
    pub transfers: Vec<TransferView>,
    pub input: InputStatusView,
    pub clipboard: ClipboardStatusView,
    pub platform: PlatformView,
    pub listening_port: u16,
}

/// One-off notifications that are not part of the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum Notice {
    Info { message: String },
    Warning { message: String },
    Error { message: String },
}

impl Notice {
    pub fn info(message: impl Into<String>) -> Self {
        Notice::Info {
            message: message.into(),
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Notice::Warning {
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Notice::Error {
            message: message.into(),
        }
    }
}

/// What the engine pushes to its host application.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineEvent {
    /// A new complete picture of the world.
    Snapshot(Box<Snapshot>),
    Notice(Notice),
}

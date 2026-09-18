use serde::{Deserialize, Serialize};

use crate::geom::DisplayInfo;
use crate::identity::{DeviceId, fingerprint_of};
use crate::input::InputEvent;
use crate::{Error, Result};

/// Noise pattern used for every peer session.
///
/// `XX` is used because it authenticates both sides with static keys while still
/// allowing a first-time connection with an unknown peer, which is exactly the
/// situation a pairing flow needs.
pub const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

pub fn noise_params() -> Result<snow::params::NoiseParams> {
    NOISE_PATTERN
        .parse()
        .map_err(|e: snow::Error| Error::Key(e.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsKind {
    Windows,
    Macos,
    Linux,
    Unknown,
}

impl OsKind {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            OsKind::Windows
        } else if cfg!(target_os = "macos") {
            OsKind::Macos
        } else if cfg!(target_os = "linux") {
            OsKind::Linux
        } else {
            OsKind::Unknown
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OsKind::Windows => "Windows",
            OsKind::Macos => "macOS",
            OsKind::Linux => "Linux",
            OsKind::Unknown => "Unknown",
        }
    }
}

/// Identity a peer announces once the encrypted channel is up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub version: String,
    pub fingerprint: String,
    pub displays: Vec<DisplayInfo>,
}

impl DeviceInfo {
    pub fn new(
        device_id: DeviceId,
        name: impl Into<String>,
        displays: Vec<DisplayInfo>,
        public_key: &[u8],
    ) -> Self {
        Self {
            device_id,
            name: name.into(),
            os: OsKind::current(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            fingerprint: fingerprint_of(public_key),
            displays,
        }
    }
}

/// Identifies one file transfer across a session.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct TransferId(pub u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub index: u32,
    pub name: String,
    pub relative_path: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileOffer {
    pub transfer: TransferId,
    pub sender_name: String,
    pub total_bytes: u64,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileAccept {
    pub transfer: TransferId,
    pub rejected: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileProgressReport {
    pub transfer: TransferId,
    pub index: u32,
    pub bytes_done: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileFinished {
    pub transfer: TransferId,
    pub failed: Vec<(u32, String)>,
}

/// Bytes that travel as base64 inside an otherwise textual envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct Blob(pub Vec<u8>);

impl Blob {
    pub fn new(bytes: Vec<u8>) -> Self {
        Blob(bytes)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn encode(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn decode(text: &str) -> std::result::Result<Vec<u8>, base64::DecodeError> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(text.as_bytes())
    }
}

impl Serialize for Blob {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&Self::encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Blob {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::decode(&text).map(Blob).map_err(serde::de::Error::custom)
    }
}

/// One clipboard flavour. Only the newest flavour is propagated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClipboardPayload {
    Text {
        text: String,
    },
    /// PNG encoded image plus its pixel dimensions.
    Image {
        width: u32,
        height: u32,
        png: Blob,
    },
    /// A list of local paths, used by the "copy files" gesture.
    Files {
        paths: Vec<String>,
    },
    Clear,
}

impl ClipboardPayload {
    pub fn kind(&self) -> &'static str {
        match self {
            ClipboardPayload::Text { .. } => "text",
            ClipboardPayload::Image { .. } => "image",
            ClipboardPayload::Files { .. } => "files",
            ClipboardPayload::Clear => "clear",
        }
    }

    pub fn approx_bytes(&self) -> usize {
        match self {
            ClipboardPayload::Text { text } => text.len(),
            ClipboardPayload::Image { png, .. } => png.len(),
            ClipboardPayload::Files { paths } => paths.iter().map(|p| p.len()).sum(),
            ClipboardPayload::Clear => 0,
        }
    }
}

/// Control messages that drive the keyboard/mouse handover.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum InputControl {
    /// Take control of the peer's cursor, entering at `x`/`y` in the peer's own
    /// desktop coordinates. `return_side` names the edge the peer must send the
    /// cursor back through.
    Enter {
        x: f64,
        y: f64,
        return_side: crate::geom::Side,
        keyboard: bool,
    },
    /// Give control back; the peer parks its cursor at `x`/`y`.
    Release { x: f64, y: f64 },
    /// The peer's cursor reached the returning edge and control should come home.
    ReturnHome { x: f64, y: f64 },
    /// Every modifier and button was released by the peer.
    ResetKeys,
}

/// The complete peer to peer vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    Hello {
        protocol: u32,
        device: DeviceInfo,
    },
    HelloAck {
        protocol: u32,
        device: DeviceInfo,
    },
    /// Sent by the machine that is being asked to trust someone.
    PairRequired {
        code_hint: String,
    },
    /// The code the user typed on the local machine.
    PairResponse {
        code: String,
        device: DeviceInfo,
    },
    PairResult {
        accepted: bool,
        reason: Option<String>,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    Displays {
        displays: Vec<DisplayInfo>,
    },
    Input(InputEvent),
    Control(InputControl),
    Clipboard(ClipboardPayload),
    FileOffer(FileOffer),
    FileAccept(FileAccept),
    FileProgress(FileProgressReport),
    FileFinished(FileFinished),
    FileCancel {
        transfer: TransferId,
        reason: String,
    },
    Bye {
        reason: String,
    },
    Error {
        message: String,
    },
}

impl Message {
    /// Input events are latency critical and are always written ahead of bulk
    /// traffic, regardless of when they were queued.
    pub fn is_urgent(&self) -> bool {
        matches!(self, Message::Input(_) | Message::Control(_))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Message::Hello { .. } => "hello",
            Message::HelloAck { .. } => "hello_ack",
            Message::PairRequired { .. } => "pair_required",
            Message::PairResponse { .. } => "pair_response",
            Message::PairResult { .. } => "pair_result",
            Message::Ping { .. } => "ping",
            Message::Pong { .. } => "pong",
            Message::Displays { .. } => "displays",
            Message::Input(_) => "input",
            Message::Control(_) => "control",
            Message::Clipboard(_) => "clipboard",
            Message::FileOffer(_) => "file_offer",
            Message::FileAccept(_) => "file_accept",
            Message::FileProgress(_) => "file_progress",
            Message::FileFinished(_) => "file_finished",
            Message::FileCancel { .. } => "file_cancel",
            Message::Bye { .. } => "bye",
            Message::Error { .. } => "error",
        }
    }
}

/// Binary side channel frame: raw file payload, never base64.
#[derive(Debug, Clone, PartialEq)]
pub struct FileChunk {
    pub transfer: TransferId,
    pub index: u32,
    pub offset: u64,
    pub data: Vec<u8>,
}

impl FileChunk {
    /// transfer(8) + index(4) + offset(8) + len(4)
    pub const HEADER_LEN: usize = 24;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.data.len());
        out.extend_from_slice(&self.transfer.0.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Error::invalid("file chunk frame is truncated"));
        }
        let transfer = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let index = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let offset = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
        if bytes.len() < Self::HEADER_LEN + len {
            return Err(Error::invalid("file chunk payload is truncated"));
        }
        Ok(FileChunk {
            transfer: TransferId(transfer),
            index,
            offset,
            data: bytes[Self::HEADER_LEN..Self::HEADER_LEN + len].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_chunk_round_trips() {
        let chunk = FileChunk {
            transfer: TransferId(7),
            index: 3,
            offset: 4096,
            data: vec![1, 2, 3, 4, 5],
        };
        let decoded = FileChunk::decode(&chunk.encode()).unwrap();
        assert_eq!(decoded, chunk);
    }

    #[test]
    fn blob_uses_base64_on_the_wire() {
        let blob = Blob::new(vec![0, 255, 16]);
        let json = serde_json::to_string(&blob).unwrap();
        assert_eq!(json, "\"AP8Q\"");
        let back: Blob = serde_json::from_str(&json).unwrap();
        assert_eq!(back, blob);
    }

    #[test]
    fn messages_tag_themselves() {
        let msg = Message::Ping { nonce: 42 };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["t"], "ping");
    }
}

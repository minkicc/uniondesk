//! The UnionDesk engine: discovery, encrypted sessions, input relaying,
//! clipboard sync and file transfer, driven through a small command channel.
//!
//! The engine is an actor. One Tokio task owns every piece of mutable state and
//! receives [`Command`]s from the UI plus events from the network, the input
//! backend, the clipboard watcher and the discovery services. Nothing is shared
//! behind a lock, so there is no way for the state to be observed half updated.

pub mod engine;
pub mod transfer;
pub mod view;

pub use engine::{start, Command, EngineError, EngineHandle, EngineOptions};
pub use transfer::{CHUNK_BYTES, TransferError};
pub use view::{
    ClipboardStatusView, ConnectionState, ControlModeView, ControlView, DeviceView, EngineEvent,
    InputStatusView, Notice, PairingView, PeerView, PlatformView, Snapshot, TransferDirection,
    TransferFileView, TransferState, TransferView,
};

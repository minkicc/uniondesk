//! The engine actor.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use ud_core::config::{EdgeLink, PeerRecord, PeerStore, ReleaseHotkey, Settings};
use ud_core::geom::{desktop_bounds, DisplayInfo, Point, Rect, Side};
use ud_core::identity::{DeviceId, DeviceIdentity};
use ud_core::input::{key, InputEvent, KeyCode, MouseButton};
use ud_core::layout::{entry_point, hits_edge, is_outward, place_peer, return_point, PlacedPeer};
use ud_core::protocol::{
    ClipboardPayload, DeviceInfo, FileAccept, FileChunk, FileFinished, FileOffer,
    InputControl, Message, OsKind, TransferId,
};
use ud_core::paths;
use ud_input::{CaptureOptions, CapturedEvent, InputController};
use ud_net::discovery::{Advertise, Discovery, DiscoveryEvent, DiscoveryTable};
use ud_net::session::Connection;
use ud_net::codec::Incoming;

use crate::transfer::{
    collect_entries, sanitize_relative, unique_path, IncomingTransfer, OutgoingFile, OutgoingTransfer,
    CHUNK_BYTES,
};
use crate::view::{
    ClipboardStatusView, ConnectionState, ControlModeView, ControlView, DeviceView, EngineEvent,
    InputStatusView, Notice, PairingView, PeerView, PlatformView, Snapshot, TransferDirection,
    TransferState, TransferView,
};

/// How often the cursor is sampled while this machine owns its own input.
const CURSOR_TICK: Duration = Duration::from_millis(8);
/// Idle housekeeping: keepalives, discovery pruning, UI refresh.
const HOUSEKEEPING_TICK: Duration = Duration::from_secs(2);
/// Silence from a peer after which the session is considered dead.
const PEER_TIMEOUT: Duration = Duration::from_secs(25);
/// Window in which two Scroll Lock taps count as the panic release gesture.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);
/// How long the non-displaying side waits before it asks for the code instead.
const PAIRING_HANDOFF_DELAY: Duration = Duration::from_millis(1500);
/// How long a handover is allowed to settle before its edge is armed again.
/// The cursor is moved to the edge as part of entering and leaving, and that
/// jump must not be mistaken for the user still pushing outwards.
const HANDOVER_SETTLE: Duration = Duration::from_millis(300);

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("the engine is not running")]
    NotRunning,

    #[error("the engine must be started from inside a Tokio runtime")]
    NoRuntime,

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

/// Where the engine keeps its files.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub identity_path: PathBuf,
    pub settings_path: PathBuf,
    pub peers_path: PathBuf,
}

impl EngineOptions {
    pub fn discover() -> Self {
        Self::in_dir(paths::config_dir())
    }

    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        EngineOptions {
            identity_path: dir.join("identity.json"),
            settings_path: dir.join("settings.json"),
            peers_path: dir.join("peers.json"),
        }
    }
}

/// Everything the host application can ask the engine to do.
#[derive(Debug, Clone)]
pub enum Command {
    Refresh,
    UpdateSettings(Box<Settings>),
    SetSharing(bool),
    Connect(DeviceId),
    ConnectAddress {
        address: SocketAddr,
        name: Option<String>,
    },
    Disconnect(DeviceId),
    Forget(DeviceId),
    AnswerPairing {
        peer: DeviceId,
        code: Option<String>,
        accept: bool,
    },
    SetLink(Box<EdgeLink>),
    RemoveLink(DeviceId),
    ReleaseControl,
    SendFiles {
        peer: DeviceId,
        paths: Vec<PathBuf>,
    },
    AcceptTransfer(TransferId),
    CancelTransfer(TransferId),
    ClearFinishedTransfers,
    OpenDownloadDir,
    Shutdown,
}

/// Handle used by the UI layer. Every method just queues work; the UI learns
/// the outcome from the next snapshot, which keeps the two sides simple.
#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::UnboundedSender<Event>,
}

impl EngineHandle {
    pub fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(Event::Command(command))
            .map_err(|_| EngineError::NotRunning)
    }

    pub fn refresh(&self) -> Result<()> {
        self.send(Command::Refresh)
    }

    pub fn shutdown(&self) -> Result<()> {
        self.send(Command::Shutdown)
    }
}

/// Starts the engine and returns a handle plus the stream of UI updates.
///
/// Must be called from inside a Tokio runtime, because the engine runs as a
/// Tokio task. Hosts that are not already running one can enter it with
/// `tauri::async_runtime::block_on` or `Runtime::enter`.
pub fn start(
    options: EngineOptions,
) -> Result<(EngineHandle, mpsc::UnboundedReceiver<EngineEvent>)> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| EngineError::NoRuntime)?;
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (handle_tx, handle_rx) = mpsc::unbounded_channel();

    let identity = DeviceIdentity::load_or_create(&options.identity_path)
        .map_err(|e| EngineError::Other(e.to_string()))?;
    let settings =
        Settings::load(&options.settings_path).map_err(|e| EngineError::Other(e.to_string()))?;
    let peers =
        PeerStore::load(&options.peers_path).map_err(|e| EngineError::Other(e.to_string()))?;

    let engine = Engine::new(options, identity, settings, peers, event_tx.clone(), handle_tx.clone());
    runtime.spawn(async move {
        if let Err(err) = engine.run(handle_rx).await {
            let _ = event_tx.send(EngineEvent::Notice(Notice::error(err.to_string())));
        }
    });
    Ok((EngineHandle { commands: handle_tx }, event_rx))
}

enum Event {
    Command(Command),
    SessionEstablished {
        connection: Arc<Connection>,
        receiver: mpsc::Receiver<Incoming>,
    },
    ConnectionFailed {
        peer: Option<DeviceId>,
        message: String,
    },
    Incoming(DeviceId, Incoming),
    SessionEnded {
        peer: DeviceId,
        reason: String,
    },
    PairingHandoff {
        peer: DeviceId,
    },
    Captured(CapturedEvent),
    Clipboard(ud_clipboard::ClipboardEvent),
    Discovered(DiscoveryEvent),
    ListenerBound(u16),
    ListenerFailed(String),
    TransferProgress {
        id: TransferId,
        bytes: u64,
    },
    TransferDone {
        id: TransferId,
        error: Option<String>,
    },
}

struct Session {
    connection: Arc<Connection>,
    info: DeviceInfo,
    trusted: bool,
    state: ConnectionState,
    pairing: Option<Pairing>,
    displays: Vec<DisplayInfo>,
    last_rx: Instant,
}

struct Pairing {
    /// Set on the side that displays the code.
    code_to_share: Option<String>,
    /// Set on the side that has to ask the user for the code.
    awaiting_code: bool,
    attempts: u8,
}

enum ControlState {
    Local,
    Controlling {
        peer: DeviceId,
        placed: PlacedPeer,
        /// Where the peer's cursor currently is, in the peer's own coordinates.
        peer_cursor: Point,
    },
    Controlled {
        peer: DeviceId,
        return_side: Side,
        keyboard: bool,
    },
}

enum Tracked {
    Outgoing {
        transfer: OutgoingTransfer,
        state: TransferState,
    },
    Incoming {
        transfer: IncomingTransfer,
        handles: HashMap<u32, tokio::fs::File>,
        state: TransferState,
        error: Option<String>,
    },
}

impl Tracked {
    fn state(&self) -> TransferState {
        match self {
            Tracked::Outgoing { state, .. } | Tracked::Incoming { state, .. } => *state,
        }
    }

    fn set_state(&mut self, value: TransferState) {
        match self {
            Tracked::Outgoing { state, .. } | Tracked::Incoming { state, .. } => *state = value,
        }
    }

    fn view(&self) -> TransferView {
        match self {
            Tracked::Outgoing { transfer, state } => {
                transfer.view(*state, average_rate(transfer.started_at, transfer.done_bytes))
            }
            Tracked::Incoming {
                transfer,
                state,
                error,
                ..
            } => transfer.view(
                *state,
                average_rate(transfer.started_at, transfer.done_bytes),
                error.clone(),
            ),
        }
    }
}

struct Engine {
    options: EngineOptions,
    identity: DeviceIdentity,
    device: DeviceView,
    settings: Settings,
    peers: PeerStore,
    out: mpsc::UnboundedSender<EngineEvent>,
    events: mpsc::UnboundedSender<Event>,
    discovery: Option<Discovery>,
    table: DiscoveryTable,
    sessions: HashMap<DeviceId, Session>,
    connecting: HashSet<DeviceId>,
    control: ControlState,
    input: Option<InputController>,
    clipboard: Option<ud_clipboard::ClipboardWatcher>,
    transfers: HashMap<TransferId, Tracked>,
    displays: Vec<DisplayInfo>,
    desktop: Rect,
    last_cursor: Option<Point>,
    held_keys: HashSet<KeyCode>,
    held_buttons: HashSet<MouseButton>,
    last_scroll_lock: Option<Instant>,
    /// Set while a handover is settling, in either direction.
    settle_until: Option<Instant>,
    /// Whether the current handover has already reported its first relayed event.
    relay_logged: bool,
    /// Whether this session has already reported input arriving from a peer.
    inject_logged: bool,
    /// Counted so a failing injection is reported without flooding the log.
    inject_errors: u64,
    /// A live description of any permission the platform is still waiting for.
    permission_hint: Option<String>,
    /// Throttles the "pushed at an edge but nothing happened" explanation.
    edge_log_at: Option<Instant>,
    listening_port: u16,
    capture_active: bool,
    clipboard_status: ClipboardStatusView,
    discovery_dirty: bool,
}

impl Engine {
    fn new(
        options: EngineOptions,
        identity: DeviceIdentity,
        settings: Settings,
        peers: PeerStore,
        out: mpsc::UnboundedSender<EngineEvent>,
        events: mpsc::UnboundedSender<Event>,
    ) -> Self {
        let device = DeviceView {
            device_id: identity.device_id.clone(),
            name: settings.device_name.clone(),
            os: OsKind::current(),
            fingerprint: identity.fingerprint(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        Engine {
            options,
            identity,
            device,
            settings,
            peers,
            out,
            events,
            discovery: None,
            table: DiscoveryTable::default(),
            sessions: HashMap::new(),
            connecting: HashSet::new(),
            control: ControlState::Local,
            input: None,
            clipboard: None,
            transfers: HashMap::new(),
            displays: Vec::new(),
            desktop: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            last_cursor: None,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            last_scroll_lock: None,
            settle_until: None,
            relay_logged: false,
            inject_logged: false,
            inject_errors: 0,
            permission_hint: None,
            edge_log_at: None,
            listening_port: 0,
            capture_active: false,
            clipboard_status: ClipboardStatusView {
                enabled: false,
                last_kind: None,
                last_at: None,
                last_direction: None,
            },
            discovery_dirty: true,
        }
    }

    async fn run(mut self, commands: mpsc::UnboundedReceiver<Event>) -> Result<()> {
        info!(device = %self.device.name, "engine starting");
        self.start_platform_services();
        self.spawn_listener();
        self.rebuild_discovery().await;

        let mut cursor_tick = tokio::time::interval(CURSOR_TICK);
        cursor_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut housekeeping = tokio::time::interval(HOUSEKEEPING_TICK);
        housekeeping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut commands = commands;
        loop {
            tokio::select! {
                biased;
                maybe = commands.recv() => {
                    let Some(event) = maybe else { break };
                    if !self.handle(event).await {
                        break;
                    }
                }
                _ = cursor_tick.tick() => self.on_cursor_tick().await,
                _ = housekeeping.tick() => self.on_housekeeping().await,
            }
        }

        info!("engine stopping");
        self.shutdown().await;
        Ok(())
    }

    // ---------------------------------------------------------------- platform

    fn start_platform_services(&mut self) {
        // Checked up front because macOS refuses these calls silently: without
        // this, a machine missing Accessibility looks exactly like a machine
        // that is receiving nothing.
        self.permission_hint = ud_input::permission_status();
        if let Some(hint) = &self.permission_hint {
            warn!(%hint, "input permissions are incomplete");
        }
        match InputController::start() {
            Ok((controller, receiver)) => {
                self.displays = controller.displays();
                self.input = Some(controller);
                self.spawn_input_forwarder(receiver);
            }
            Err(err) => {
                warn!(error = %err, "input backend unavailable");
                self.notify(Notice::error(format!(
                    "Keyboard and mouse sharing could not start on this machine: {err}"
                )));
                self.displays = Vec::new();
            }
        }
        if self.displays.is_empty() {
            self.displays = vec![DisplayInfo {
                id: "primary".into(),
                name: "Display".into(),
                bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                scale_factor: 1.0,
                is_primary: true,
            }];
        }
        self.desktop = desktop_bounds(&self.displays);

        let limits = self.clipboard_limits();
        match ud_clipboard::ClipboardWatcher::start(
            Duration::from_millis(self.settings.clipboard.poll_interval_ms.max(100)),
            limits,
        ) {
            Ok((watcher, mut receiver)) => {
                self.clipboard = Some(watcher);
                self.clipboard_status.enabled = self.settings.clipboard.enabled;
                let events = self.events.clone();
                tokio::spawn(async move {
                    while let Some(event) = receiver.recv().await {
                        if events.send(Event::Clipboard(event)).is_err() {
                            break;
                        }
                    }
                });
            }
            Err(err) => {
                warn!(error = %err, "clipboard watcher unavailable");
            }
        }
    }

    fn spawn_input_forwarder(&self, mut receiver: mpsc::UnboundedReceiver<CapturedEvent>) {
        let events = self.events.clone();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                if events.send(Event::Captured(event)).is_err() {
                    break;
                }
            }
        });
    }

    fn clipboard_limits(&self) -> ud_clipboard::Limits {
        ud_clipboard::Limits {
            text: self.settings.clipboard.enabled && self.settings.clipboard.sync_text,
            images: self.settings.clipboard.enabled && self.settings.clipboard.sync_images,
            max_bytes: self.settings.clipboard.max_bytes,
        }
    }

    fn spawn_listener(&self) {
        let events = self.events.clone();
        let port = self.settings.port;
        let identity = self.identity.clone();
        let device = self.device_info();
        tokio::spawn(async move {
            let listener = match TcpListener::bind(("0.0.0.0", port)).await {
                Ok(listener) => listener,
                Err(err) => {
                    let _ = events.send(Event::ListenerFailed(err.to_string()));
                    return;
                }
            };
            let bound = listener
                .local_addr()
                .map(|addr| addr.port())
                .unwrap_or(port);
            let _ = events.send(Event::ListenerBound(bound));
            info!(port = bound, "listening for peers");
            loop {
                let Ok((stream, from)) = listener.accept().await else {
                    break;
                };
                debug!(%from, "incoming connection");
                let events = events.clone();
                let identity = identity.clone();
                let device = device.clone();
                tokio::spawn(async move {
                    match ud_net::session::accept(stream, &identity, device).await {
                        Ok((connection, receiver)) => {
                            let _ = events.send(Event::SessionEstablished {
                                connection: Arc::new(connection),
                                receiver,
                            });
                        }
                        Err(err) => {
                            debug!(error = %err, "incoming handshake failed");
                        }
                    }
                });
            }
        });
    }

    async fn rebuild_discovery(&mut self) {
        self.discovery = None;
        self.table = DiscoveryTable::default();
        if !self.settings.discoverable {
            self.discovery_dirty = false;
            return;
        }
        let advertise = Advertise {
            device_id: self.identity.device_id.clone(),
            name: self.settings.device_name.clone(),
            os: OsKind::current(),
            port: self.settings.port,
            fingerprint: self.identity.fingerprint(),
            enabled: true,
        };
        match Discovery::start(advertise).await {
            Ok((discovery, mut receiver)) => {
                self.discovery = Some(discovery);
                let events = self.events.clone();
                tokio::spawn(async move {
                    while let Some(event) = receiver.recv().await {
                        if events.send(Event::Discovered(event)).is_err() {
                            break;
                        }
                    }
                });
                self.discovery_dirty = false;
            }
            Err(err) => {
                warn!(error = %err, "peer discovery could not start");
                self.notify(Notice::warning(format!("Peer discovery failed: {err}")));
            }
        }
    }

    fn device_info(&self) -> DeviceInfo {
        DeviceInfo::new(
            self.identity.device_id.clone(),
            self.settings.device_name.clone(),
            self.displays.clone(),
            &self.identity.public_key_raw().unwrap_or_default(),
        )
    }

    // ------------------------------------------------------------ event intake

    /// Returns false when the engine should stop.
    async fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Command(command) => self.on_command(command).await,
            Event::SessionEstablished {
                connection,
                receiver,
            } => {
                self.on_session_established(connection, receiver).await;
                true
            }
            Event::ConnectionFailed { peer, message } => {
                if let Some(peer) = peer {
                    self.connecting.remove(&peer);
                }
                self.notify(Notice::error(message));
                self.publish().await;
                true
            }
            Event::Incoming(peer, incoming) => {
                if let Some(session) = self.sessions.get_mut(&peer) {
                    session.last_rx = Instant::now();
                }
                self.on_incoming(peer, incoming).await;
                true
            }
            Event::SessionEnded { peer, reason } => {
                self.on_session_ended(&peer, &reason).await;
                true
            }
            Event::PairingHandoff { peer } => {
                self.on_pairing_handoff(&peer).await;
                true
            }
            Event::Captured(event) => {
                self.on_captured(event).await;
                true
            }
            Event::Clipboard(event) => {
                self.on_clipboard(event).await;
                true
            }
            Event::Discovered(event) => {
                self.on_discovered(event).await;
                true
            }
            Event::ListenerBound(port) => {
                self.listening_port = port;
                self.notify(Notice::info(format!(
                    "Listening for peers on port {port}."
                )));
                self.publish().await;
                true
            }
            Event::ListenerFailed(message) => {
                self.notify(Notice::error(format!(
                    "Port {} is not available: {message}",
                    self.settings.port
                )));
                true
            }
            Event::TransferProgress { id, bytes } => {
                if let Some(Tracked::Outgoing { transfer, .. }) = self.transfers.get_mut(&id) {
                    transfer.done_bytes = transfer.done_bytes.saturating_add(bytes);
                }
                true
            }
            Event::TransferDone { id, error } => {
                self.on_transfer_done(id, error).await;
                true
            }
        }
    }

    async fn on_command(&mut self, command: Command) -> bool {
        match command {
            Command::Refresh => self.publish().await,
            Command::UpdateSettings(settings) => self.on_update_settings(*settings).await,
            Command::SetSharing(enabled) => {
                self.settings.input.enabled = enabled;
                if !enabled {
                    self.release_control("sharing was turned off").await;
                }
                self.persist_settings();
                self.publish().await;
            }
            Command::Connect(peer) => self.connect_peer(&peer),
            Command::ConnectAddress { address, name } => {
                self.connect_address(address, name, None);
            }
            Command::Disconnect(peer) => {
                if let Some(session) = self.sessions.remove(&peer) {
                    let _ = session
                        .connection
                        .send(Message::Bye {
                            reason: "disconnected locally".into(),
                        })
                        .await;
                    session.connection.close();
                }
                if matches!(&self.control, ControlState::Controlling { peer: active, .. } if active == &peer)
                {
                    self.release_control("peer disconnected").await;
                }
                self.publish().await;
            }
            Command::Forget(peer) => {
                if self.peers.remove(&peer) {
                    self.settings.remove_link(&peer);
                    self.persist_peers();
                    self.persist_settings();
                }
                self.publish().await;
            }
            Command::AnswerPairing {
                peer,
                code,
                accept,
            } => self.answer_pairing(&peer, code, accept).await,
            Command::SetLink(link) => {
                self.settings.set_link(*link);
                self.persist_settings();
                self.publish().await;
            }
            Command::RemoveLink(peer) => {
                self.settings.remove_link(&peer);
                self.persist_settings();
                self.publish().await;
            }
            Command::ReleaseControl => self.release_control("released from the UI").await,
            Command::SendFiles { peer, paths } => self.send_files(&peer, paths).await,
            Command::AcceptTransfer(id) => self.accept_transfer(id).await,
            Command::CancelTransfer(id) => self.cancel_transfer(id).await,
            Command::ClearFinishedTransfers => {
                self.transfers.retain(|_, tracked| {
                    !matches!(
                        tracked.state(),
                        TransferState::Completed
                            | TransferState::Cancelled
                            | TransferState::Failed
                    )
                });
                self.publish().await;
            }
            Command::OpenDownloadDir => {
                let dir = self.settings.transfer.download_dir.clone();
                if let Err(err) = std::fs::create_dir_all(&dir) {
                    self.notify(Notice::error(format!("Could not open {dir:?}: {err}")));
                } else {
                    open_in_file_manager(&dir);
                }
            }
            Command::Shutdown => return false,
        }
        true
    }

    async fn on_update_settings(&mut self, settings: Settings) {
        let discovery_changed = settings.device_name != self.settings.device_name
            || settings.port != self.settings.port
            || settings.discoverable != self.settings.discoverable;
        let clipboard_changed = settings.clipboard != self.settings.clipboard;
        self.settings = settings;
        self.device.name = self.settings.device_name.clone();
        self.persist_settings();

        if clipboard_changed {
            let limits = self.clipboard_limits();
            if let Some(clipboard) = &self.clipboard {
                let _ = clipboard.set_limits(limits);
            }
            self.clipboard_status.enabled = self.settings.clipboard.enabled;
        }
        if discovery_changed {
            self.discovery_dirty = true;
            self.rebuild_discovery().await;
        }
        self.publish().await;
    }

    fn persist_settings(&self) {
        if let Err(err) = self.settings.save(&self.options.settings_path) {
            warn!(error = %err, "could not save settings");
        }
    }

    fn persist_peers(&self) {
        if let Err(err) = self.peers.save(&self.options.peers_path) {
            warn!(error = %err, "could not save the peer list");
        }
    }

    // ------------------------------------------------------------- connections

    fn connect_peer(&mut self, peer: &DeviceId) {
        if self.sessions.contains_key(peer) || self.connecting.contains(peer) {
            return;
        }
        let Some(found) = self.table.get(peer).cloned() else {
            self.notify(Notice::warning(
                "That machine is not visible right now. Check that UnionDesk is running there.",
            ));
            return;
        };
        let Some(address) = found.best_address() else {
            return;
        };
        self.connecting.insert(peer.clone());
        self.connect_address(address, Some(found.name), Some(peer.clone()));
    }

    fn connect_address(
        &self,
        address: SocketAddr,
        name: Option<String>,
        expected: Option<DeviceId>,
    ) {
        let identity = self.identity.clone();
        let device = self.device_info();
        let events = self.events.clone();
        debug!(%address, "dialling peer");
        tokio::spawn(async move {
            let stream = match tokio::time::timeout(
                Duration::from_secs(8),
                TcpStream::connect(address),
            )
            .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(err)) => {
                    let _ = events.send(Event::ConnectionFailed {
                        peer: expected,
                        message: format!(
                            "Could not reach {}: {err}",
                            name.unwrap_or_else(|| address.to_string())
                        ),
                    });
                    return;
                }
                Err(_) => {
                    let _ = events.send(Event::ConnectionFailed {
                        peer: expected,
                        message: format!("Timed out reaching {address}"),
                    });
                    return;
                }
            };
            match ud_net::session::dial(stream, &identity, device).await {
                Ok((connection, receiver)) => {
                    let _ = events.send(Event::SessionEstablished {
                        connection: Arc::new(connection),
                        receiver,
                    });
                }
                Err(err) => {
                    let _ = events.send(Event::ConnectionFailed {
                        peer: expected,
                        message: format!("Handshake failed: {err}"),
                    });
                }
            }
        });
    }

    async fn on_session_established(
        &mut self,
        connection: Arc<Connection>,
        receiver: mpsc::Receiver<Incoming>,
    ) {
        let info = connection.peer.clone();
        let peer = info.device_id.clone();
        let key = base64_key(&connection.remote_static);
        self.connecting.remove(&peer);

        if let Some(record) = self.peers.get(&peer) {
            if record.public_key != key {
                self.notify(Notice::error(format!(
                    "{} presented a different key than the one you paired with. \
                     The connection was refused; forget the machine and pair again \
                     if you replaced it.",
                    info.name
                )));
                let _ = connection
                    .send(Message::Bye {
                        reason: "identity changed".into(),
                    })
                    .await;
                connection.close();
                self.publish().await;
                return;
            }
        }
        let trusted = self.peers.get(&peer).is_some();

        // Both sides may dial at the same moment; the machine with the smaller
        // device id is the one whose connection is kept.
        let preferred = connection.initiator == (self.identity.device_id < peer);
        if let Some(existing) = self.sessions.get(&peer) {
            let existing_preferred =
                existing.connection.initiator == (self.identity.device_id < peer);
            if existing_preferred && !preferred {
                debug!(peer = %info.name, "dropping a redundant connection");
                connection.close();
                return;
            }
        }

        let displays = info.displays.clone();
        let session_state = if trusted {
            ConnectionState::Connected
        } else {
            ConnectionState::Pairing
        };
        if let Some(previous) = self.sessions.insert(
            peer.clone(),
            Session {
                connection: connection.clone(),
                info: info.clone(),
                trusted,
                state: session_state,
                pairing: None,
                displays,
                last_rx: Instant::now(),
            },
        ) {
            previous.connection.close();
        }

        let events = self.events.clone();
        let forward_peer = peer.clone();
        tokio::spawn(async move {
            let mut receiver = receiver;
            while let Some(item) = receiver.recv().await {
                if events
                    .send(Event::Incoming(forward_peer.clone(), item))
                    .is_err()
                {
                    return;
                }
            }
            let _ = events.send(Event::SessionEnded {
                peer: forward_peer,
                reason: "the connection closed".into(),
            });
        });

        if let Some(record) = self.peers.get_mut(&peer) {
            record.last_seen = Some(ud_core::now_unix());
            record.name = info.name.clone();
            record.last_address = None;
        }
        self.persist_peers();

        if trusted {
            self.notify(Notice::info(format!("Connected to {}.", info.name)));
            self.send_displays(&peer).await;
        } else {
            // The larger device id shows the code so only one side ever offers one.
            if self.identity.device_id > peer {
                self.request_pairing(&peer).await;
            } else {
                let events = self.events.clone();
                let peer = peer.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(PAIRING_HANDOFF_DELAY).await;
                    let _ = events.send(Event::PairingHandoff { peer });
                });
            }
        }
        self.publish().await;
    }

    async fn on_session_ended(&mut self, peer: &DeviceId, reason: &str) {
        let Some(session) = self.sessions.remove(peer) else {
            return;
        };
        debug!(peer = %session.info.name, reason, "session ended");
        self.connecting.remove(peer);
        if matches!(&self.control, ControlState::Controlling { peer: active, .. } if active == peer) {
            self.release_control("the peer went away").await;
        } else if matches!(&self.control, ControlState::Controlled { peer: active, .. } if active == peer) {
            self.release_held_input().await;
            self.control = ControlState::Local;
        }
        for tracked in self.transfers.values_mut() {
            let matches_peer = match tracked {
                Tracked::Outgoing { transfer, .. } => &transfer.peer == peer,
                Tracked::Incoming { transfer, .. } => &transfer.peer == peer,
            };
            if matches_peer && tracked.state() == TransferState::Active {
                tracked.set_state(TransferState::Failed);
            }
        }
        self.publish().await;
    }

    // ------------------------------------------------------------ message flow

    async fn on_incoming(&mut self, peer: DeviceId, incoming: Incoming) {
        match incoming {
            Incoming::Message(message) => self.on_message(peer, message).await,
            Incoming::Chunk(chunk) => self.on_chunk(chunk).await,
        }
    }

    async fn on_message(&mut self, peer: DeviceId, message: Message) {
        match message {
            Message::Hello { .. } | Message::HelloAck { .. } => {}
            Message::Ping { nonce } => {
                self.send_to(&peer, Message::Pong { nonce }).await;
            }
            Message::Pong { .. } => {}
            Message::Displays { displays } => {
                if let Some(session) = self.sessions.get_mut(&peer) {
                    session.displays = displays;
                }
                self.publish().await;
            }
            Message::PairRequired { .. } => {
                if let Some(session) = self.sessions.get_mut(&peer) {
                    if !session.trusted {
                        session.state = ConnectionState::Pairing;
                        session.pairing = Some(Pairing {
                            code_to_share: None,
                            awaiting_code: true,
                            attempts: 0,
                        });
                    }
                }
                self.publish().await;
            }
            Message::PairResponse { code, .. } => self.on_pair_response(&peer, code).await,
            Message::PairResult { accepted, reason } => {
                self.on_pair_result(&peer, accepted, reason).await
            }
            Message::Input(event) => self.on_remote_input(event).await,
            Message::Control(control) => self.on_control(peer, control).await,
            Message::Clipboard(payload) => self.on_remote_clipboard(peer, payload).await,
            Message::FileOffer(offer) => self.on_file_offer(peer, offer).await,
            Message::FileAccept(accept) => self.on_file_accept(accept).await,
            Message::FileProgress(report) => {
                if let Some(Tracked::Incoming { transfer, .. }) =
                    self.transfers.get_mut(&report.transfer)
                {
                    if let Some(position) = transfer.position_of(report.index) {
                        let previous = transfer.written[position];
                        transfer.written[position] = report.bytes_done;
                        transfer.done_bytes = transfer.done_bytes.saturating_sub(previous)
                            + report.bytes_done;
                    }
                }
            }
            Message::FileFinished(finished) => self.on_file_finished(finished).await,
            Message::FileCancel { transfer, reason } => {
                self.on_file_cancelled(transfer, reason).await
            }
            Message::Bye { reason } => {
                self.on_session_ended(&peer, &reason).await;
            }
            Message::Error { message } => {
                self.notify(Notice::error(format!("Peer error: {message}")));
            }
        }
    }

    async fn send_to(&self, peer: &DeviceId, message: Message) {
        if let Some(session) = self.sessions.get(peer) {
            if !session.connection.try_send_urgent(message.clone()) {
                let _ = session.connection.send(message).await;
            }
        }
    }

    fn connected_peers(&self) -> Vec<DeviceId> {
        self.sessions
            .iter()
            .filter(|(_, session)| session.state.is_connected())
            .map(|(id, _)| id.clone())
            .collect()
    }

    // ----------------------------------------------------------------- pairing

    /// Called by the side that wants the other user to type a code.
    async fn request_pairing(&mut self, peer: &DeviceId) {
        let code = format!("{:06}", rand::random::<u32>() % 1_000_000);
        let connection = {
            let Some(session) = self.sessions.get_mut(peer) else {
                return;
            };
            session.state = ConnectionState::Pairing;
            session.pairing = Some(Pairing {
                code_to_share: Some(code.clone()),
                awaiting_code: false,
                attempts: 0,
            });
            session.connection.clone()
        };
        let _ = connection
            .send(Message::PairRequired {
                code_hint: code.clone(),
            })
            .await;
        self.publish().await;
    }

    async fn on_pairing_handoff(&mut self, peer: &DeviceId) {
        let needs_code = self
            .sessions
            .get(peer)
            .map(|session| {
                !session.trusted
                    && session
                        .pairing
                        .as_ref()
                        .map(|pairing| !pairing.awaiting_code)
                        .unwrap_or(true)
            })
            .unwrap_or(false);
        if needs_code {
            self.request_pairing(peer).await;
        }
    }

    async fn on_pair_response(&mut self, peer: &DeviceId, code: String) {
        let Some((connection, info, remote_static, expected)) = ({
            match self.sessions.get(peer) {
                Some(session) => Some((
                    session.connection.clone(),
                    session.info.clone(),
                    base64_key(&session.connection.remote_static),
                    session
                        .pairing
                        .as_ref()
                        .and_then(|pairing| pairing.code_to_share.clone()),
                )),
                None => None,
            }
        }) else {
            return;
        };

        let Some(expected) = expected else {
            let _ = connection
                .send(Message::PairResult {
                    accepted: false,
                    reason: Some("this machine is not showing a pairing code".into()),
                })
                .await;
            return;
        };

        if normalize_code(&code) == normalize_code(&expected) {
            self.remember_peer(peer, &info, &remote_static);
            if let Some(session) = self.sessions.get_mut(peer) {
                session.trusted = true;
                session.state = ConnectionState::Connected;
                session.pairing = None;
            }
            self.ensure_link(peer, &info.name);
            let _ = connection
                .send(Message::PairResult {
                    accepted: true,
                    reason: None,
                })
                .await;
            self.notify(Notice::info(format!("Paired with {}.", info.name)));
            self.send_displays(peer).await;
        } else {
            let attempts = self
                .sessions
                .get_mut(peer)
                .and_then(|session| session.pairing.as_mut())
                .map(|pairing| {
                    pairing.attempts += 1;
                    pairing.attempts
                })
                .unwrap_or(3);
            let _ = connection
                .send(Message::PairResult {
                    accepted: false,
                    reason: Some("the code did not match".into()),
                })
                .await;
            if attempts >= 3 {
                self.notify(Notice::warning(
                    "Too many wrong pairing codes; the connection was closed.",
                ));
                if let Some(session) = self.sessions.remove(peer) {
                    session.connection.close();
                }
            } else {
                self.notify(Notice::warning(format!(
                    "That code did not match. {attempts} of 3 attempts used."
                )));
            }
        }
        self.publish().await;
    }

    async fn on_pair_result(&mut self, peer: &DeviceId, accepted: bool, reason: Option<String>) {
        let Some((connection, info)) = self
            .sessions
            .get(peer)
            .map(|session| (session.connection.clone(), session.info.clone()))
        else {
            return;
        };
        if accepted {
            let remote_static = base64_key(&connection.remote_static);
            self.remember_peer(peer, &info, &remote_static);
            self.ensure_link(peer, &info.name);
            if let Some(session) = self.sessions.get_mut(peer) {
                session.trusted = true;
                session.state = ConnectionState::Connected;
                session.pairing = None;
            }
            self.notify(Notice::info(format!("Paired with {}.", info.name)));
            self.send_displays(peer).await;
        } else {
            let message = reason.clone().unwrap_or_else(|| "pairing rejected".into());
            if let Some(session) = self.sessions.get_mut(peer) {
                session.state = ConnectionState::Failed {
                    reason: message.clone(),
                };
                session.pairing = None;
            }
            self.notify(Notice::warning(format!("Pairing rejected: {message}")));
        }
        self.publish().await;
    }

    async fn answer_pairing(&mut self, peer: &DeviceId, code: Option<String>, accept: bool) {
        let Some(connection) = self.sessions.get(peer).map(|s| s.connection.clone()) else {
            return;
        };
        if !accept {
            let _ = connection
                .send(Message::PairResult {
                    accepted: false,
                    reason: Some("the request was declined".into()),
                })
                .await;
            if let Some(session) = self.sessions.remove(peer) {
                session.connection.close();
            }
            self.publish().await;
            return;
        }
        let Some(code) = code else { return };
        let device = self.device_info();
        let _ = connection
            .send(Message::PairResponse { code, device })
            .await;
    }

    fn remember_peer(&mut self, peer: &DeviceId, info: &DeviceInfo, key: &str) {
        let record = PeerRecord {
            device_id: peer.clone(),
            name: info.name.clone(),
            public_key: key.to_string(),
            fingerprint: info.fingerprint.clone(),
            os: info.os,
            last_seen: Some(ud_core::now_unix()),
            last_address: None,
        };
        self.peers.upsert(record);
        self.persist_peers();
    }

    /// Gives a freshly paired machine a default edge so it is usable at once.
    fn ensure_link(&mut self, peer: &DeviceId, name: &str) {
        if self.settings.link_for(peer).is_some() {
            return;
        }
        let side = if self.identity.device_id < *peer {
            Side::Right
        } else {
            Side::Left
        };
        self.settings
            .set_link(EdgeLink::new(peer.clone(), name, side));
        self.persist_settings();
    }

    async fn send_displays(&self, peer: &DeviceId) {
        if let Some(session) = self.sessions.get(peer) {
            let _ = session
                .connection
                .send(Message::Displays {
                    displays: self.displays.clone(),
                })
                .await;
        }
    }

    // ------------------------------------------------------------------- input

    async fn on_cursor_tick(&mut self) {
        if self.discovery_dirty {
            self.rebuild_discovery().await;
        }
        let Some(input) = &self.input else { return };
        let Some(position) = input.cursor_position() else {
            return;
        };
        let delta = match self.last_cursor.replace(position) {
            Some(previous) => (position.x - previous.x, position.y - previous.y),
            None => (0.0, 0.0),
        };

        match &self.control {
            ControlState::Local => {
                if self.settings.input.enabled && !self.is_settling() {
                    self.try_engage(position, delta).await;
                }
            }
            ControlState::Controlling { .. } => {}
            ControlState::Controlled { return_side, .. } => {
                // The peer moved our cursor to its edge a moment ago; ignore the
                // jump that produced.
                if self.is_settling() {
                    return;
                }
                let side = *return_side;
                let tolerance = self.settings.input.edge_armed_pixels.max(1.0);
                if hits_edge(self.desktop, side, position, tolerance)
                    && is_outward(side, delta.0, delta.1)
                {
                    let peer = match &self.control {
                        ControlState::Controlled { peer, .. } => peer.clone(),
                        _ => return,
                    };
                    self.send_to(
                        &peer,
                        Message::Control(InputControl::ReturnHome {
                            x: position.x,
                            y: position.y,
                        }),
                    )
                    .await;
                    info!(
                        %peer,
                        x = position.x,
                        y = position.y,
                        "our cursor reached the returning edge, handing control back"
                    );
                    self.release_held_input().await;
                    self.control = ControlState::Local;
                    self.notify(Notice::info("Control returned to this machine."));
                    self.publish().await;
                }
            }
        }
    }

    async fn try_engage(&mut self, position: Point, delta: (f64, f64)) {
        let tolerance = self.settings.input.edge_armed_pixels.max(1.0);
        let mut candidate: Option<(EdgeLink, PlacedPeer, Point)> = None;
        let mut blocked: Option<(Side, String)> = None;
        for link in self.settings.links.iter().filter(|l| l.enabled) {
            let side = link.local_side;
            if !hits_edge(self.desktop, side, position, tolerance)
                || !is_outward(side, delta.0, delta.1)
            {
                continue;
            }
            // The cursor is being pushed at a configured edge. Either we hand
            // over, or we say why we cannot: a machine that silently does
            // nothing is the hardest kind of bug to report.
            match self.sessions.get(&link.peer) {
                None => {
                    blocked = Some((side, format!("{} is not connected", link.peer_name)));
                    break;
                }
                Some(session) if !session.state.is_connected() => {
                    blocked = Some((
                        side,
                        format!("the connection to {} is {}", link.peer_name, session.state.label()),
                    ));
                    break;
                }
                Some(session) => {
                    let peer_desktop = desktop_bounds(&session.displays);
                    let placed = place_peer(self.desktop, peer_desktop, link);
                    let entry = entry_point(&placed, position);
                    candidate = Some((link.clone(), placed, entry));
                    break;
                }
            }
        }

        let Some((link, placed, entry)) = candidate else {
            if let Some((side, reason)) = blocked {
                let due = self
                    .edge_log_at
                    .map(|last| Instant::now().duration_since(last) >= Duration::from_secs(3))
                    .unwrap_or(true);
                if due {
                    self.edge_log_at = Some(Instant::now());
                    warn!(
                        edge = side.label(),
                        %reason,
                        "cursor is pushed at a configured edge but no handover can happen"
                    );
                }
            }
            return;
        };

        // Refuse up front when the platform has already told us it cannot
        // capture. Trying anyway produces a handover that starts and dies within
        // milliseconds, which reads as "the mouse does not come over" and gives
        // the user nothing to act on.
        if let Some(hint) = self.permission_hint.clone() {
            let due = self
                .edge_log_at
                .map(|last| Instant::now().duration_since(last) >= Duration::from_secs(3))
                .unwrap_or(true);
            if due {
                self.edge_log_at = Some(Instant::now());
                warn!(%hint, "refusing to take control because a permission is missing");
                self.notify(Notice::warning(hint));
            }
            return;
        }

        let keyboard = self.settings.input.relay_keyboard;
        if let Some(input) = &self.input {
            if let Err(err) = input.set_capture(CaptureOptions {
                mouse: true,
                keyboard,
            }) {
                self.notify(Notice::error(format!("Could not take over the input: {err}")));
                return;
            }
        }
        self.capture_active = true;
        self.relay_logged = false;
        // The platform moves the cursor while it captures; forget where it was
        // so the resulting jump is not mistaken for movement.
        self.last_cursor = None;
        self.control = ControlState::Controlling {
            peer: link.peer.clone(),
            placed,
            peer_cursor: entry,
        };
        let name = self
            .sessions
            .get(&link.peer)
            .map(|session| session.info.name.clone())
            .unwrap_or_default();
        info!(
            peer = %name,
            side = link.local_side.label(),
            entry_x = entry.x,
            entry_y = entry.y,
            "taking control of the peer"
        );
        self.notify(Notice::info(format!("Controlling {name}.")));
        self.send_to(
            &link.peer,
            Message::Control(InputControl::Enter {
                x: entry.x,
                y: entry.y,
                return_side: link.remote_side,
                keyboard,
            }),
        )
        .await;
        self.publish().await;
    }

    async fn release_control(&mut self, reason: &str) {
        // Peek before taking the state apart: this is also called from the UI
        // and on shutdown, where the engine may well be in another mode, and
        // clobbering that would strand the peer in "controlled" forever.
        let previous = std::mem::replace(&mut self.control, ControlState::Local);
        let ControlState::Controlling {
            peer,
            placed,
            peer_cursor,
        } = previous
        else {
            self.control = previous;
            return;
        };
        // Info rather than debug: this line explains why a handover ended, and
        // it is the first thing worth reading when one misbehaves.
        info!(reason, "releasing control");
        if let Some(input) = &self.input {
            let _ = input.release_capture();
            let _ = input.warp(return_point(&placed, peer_cursor));
        }
        self.capture_active = false;
        self.last_cursor = None;
        self.settle_until = Some(Instant::now() + HANDOVER_SETTLE);
        self.send_to(
            &peer,
            Message::Control(InputControl::Release {
                x: peer_cursor.x,
                y: peer_cursor.y,
            }),
        )
        .await;
        self.publish().await;
    }

    /// The peer's cursor reached the edge that faces us, so control comes home.
    ///
    /// The peer releases its own half of the handover before sending this, which
    /// means the reply below is what puts *this* machine back into local mode.
    /// Forgetting to do that left the local cursor captured and stuck at the
    /// screen edge with no way back.
    async fn on_return_home(&mut self, peer: DeviceId, point: Point) {
        if !matches!(&self.control, ControlState::Controlling { peer: active, .. } if active == &peer)
        {
            return;
        }
        let previous = std::mem::replace(&mut self.control, ControlState::Local);
        let ControlState::Controlling { placed, .. } = previous else {
            self.control = previous;
            return;
        };
        if let Some(input) = &self.input {
            let _ = input.release_capture();
            let _ = input.warp(return_point(&placed, point));
        }
        self.capture_active = false;
        self.last_cursor = None;
        self.settle_until = Some(Instant::now() + HANDOVER_SETTLE);
        self.send_to(
            &peer,
            Message::Control(InputControl::Release {
                x: point.x,
                y: point.y,
            }),
        )
        .await;
        info!(%peer, x = point.x, y = point.y, "control returned home");
        self.notify(Notice::info("Control came back to this machine."));
        self.publish().await;
    }

    fn is_settling(&self) -> bool {
        self.settle_until
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    async fn on_captured(&mut self, event: CapturedEvent) {
        if let CapturedEvent::CaptureLost { reason } = &event {
            warn!(%reason, "the platform stopped capturing input");
            self.notify(Notice::error(format!("Input capture stopped: {reason}")));
            self.release_control("capture was lost").await;
            return;
        }
        let ControlState::Controlling { peer, .. } = &self.control else {
            return;
        };
        let peer = peer.clone();

        if self.is_release_gesture(&event) {
            self.release_control("the release shortcut was pressed").await;
            return;
        }

        let speed = self.settings.input.mouse_speed.max(0.05);
        let outgoing = match event {
            CapturedEvent::MoveDelta { dx, dy } => {
                if let ControlState::Controlling { peer_cursor, .. } = &mut self.control {
                    peer_cursor.x += dx * speed;
                    peer_cursor.y += dy * speed;
                }
                InputEvent::MoveRel {
                    dx: dx * speed,
                    dy: dy * speed,
                }
            }
            CapturedEvent::Button { button, down } => InputEvent::Button { button, down },
            CapturedEvent::Wheel { dx, dy } => InputEvent::Wheel { dx, dy },
            CapturedEvent::Key {
                code,
                down,
                modifiers,
            } => InputEvent::Key {
                code,
                down,
                modifiers,
            },
            CapturedEvent::CaptureLost { .. } => return,
        };
        if let Some(session) = self.sessions.get(&peer) {
            let forwarded = session
                .connection
                .try_send_urgent(Message::Input(outgoing));
            if forwarded && !self.relay_logged {
                self.relay_logged = true;
                info!(%peer, "relaying input to the peer");
            } else if !forwarded {
                trace!(%peer, "the outbound queue is full, dropping an input event");
            }
        }
    }

    fn is_release_gesture(&mut self, event: &CapturedEvent) -> bool {
        let CapturedEvent::Key {
            code,
            down,
            modifiers,
        } = event
        else {
            return false;
        };
        if !*down {
            return false;
        }
        match self.settings.input.release_hotkey {
            ReleaseHotkey::Disabled => false,
            ReleaseHotkey::ScrollLockTwice => {
                if *code != key::ScrollLock {
                    return false;
                }
                let now = Instant::now();
                let double = self
                    .last_scroll_lock
                    .map(|previous| now.duration_since(previous) < DOUBLE_TAP_WINDOW)
                    .unwrap_or(false);
                self.last_scroll_lock = Some(now);
                double
            }
            ReleaseHotkey::CtrlAltEscape => {
                *code == key::Escape && modifiers.ctrl() && modifiers.alt() && !modifiers.meta()
            }
            ReleaseHotkey::CtrlAltCmdEscape => {
                *code == key::Escape && modifiers.ctrl() && modifiers.alt() && modifiers.meta()
            }
        }
    }

    async fn on_remote_input(&mut self, event: InputEvent) {
        let (peer, keys_allowed) = match &self.control {
            ControlState::Controlled { peer, keyboard, .. } => (peer.clone(), *keyboard),
            _ => return,
        };
        // The controllers can decline to relay keys, in which case the local
        // keyboard stays ours even while the pointer is borrowed.
        if !keys_allowed && matches!(event, InputEvent::Key { .. }) {
            return;
        }
        match &event {
            InputEvent::Key { code, down, .. } => {
                if *down {
                    self.held_keys.insert(*code);
                } else {
                    self.held_keys.remove(code);
                }
            }
            InputEvent::Button { button, down } => {
                if *down {
                    self.held_buttons.insert(*button);
                } else {
                    self.held_buttons.remove(button);
                }
            }
            _ => {}
        }
        let Some(input) = &self.input else { return };
        if let Err(err) = input.inject(&event) {
            // Warn loudly the first time and then occasionally: an injection
            // failure means the peer is driving a machine that is not moving,
            // which is exactly the symptom worth chasing.
            self.inject_errors += 1;
            if self.inject_errors == 1 || self.inject_errors % 200 == 0 {
                warn!(
                    %peer,
                    failures = self.inject_errors,
                    error = %err,
                    "could not inject a remote event"
                );
            }
        } else if !self.inject_logged {
            self.inject_logged = true;
            info!(%peer, "receiving input from the peer");
        }
    }

    async fn on_control(&mut self, peer: DeviceId, control: InputControl) {
        match control {
            InputControl::Enter {
                x,
                y,
                return_side,
                keyboard,
            } => {
                if let Some(input) = &self.input {
                    let _ = input.warp(Point::new(x, y));
                }
                self.control = ControlState::Controlled {
                    peer: peer.clone(),
                    return_side,
                    keyboard,
                };
                // The warp lands the cursor on the edge we are expected to
                // return through, so forget the previous sample and give the
                // move time to land before watching for an outward push.
                self.last_cursor = None;
                self.settle_until = Some(Instant::now() + HANDOVER_SETTLE);
                let name = self
                    .sessions
                    .get(&peer)
                    .map(|session| session.info.name.clone())
                    .unwrap_or_default();
                info!(peer = %name, x, y, return_side = return_side.label(), "peer took control");
                self.notify(Notice::info(format!("{name} is controlling this machine.")));
                self.publish().await;
            }
            InputControl::Release { x, y } => {
                self.release_held_input().await;
                if let Some(input) = &self.input {
                    let _ = input.warp(Point::new(x, y));
                }
                self.last_cursor = None;
                self.control = ControlState::Local;
                self.publish().await;
            }
            InputControl::ReturnHome { x, y } => self.on_return_home(peer, Point::new(x, y)).await,
            InputControl::ResetKeys => self.release_held_input().await,
        }
    }

    /// Releases anything the remote side might have left pressed, so the local
    /// machine can never be left with a stuck modifier.
    async fn release_held_input(&mut self) {
        let keys = std::mem::take(&mut self.held_keys);
        let buttons = std::mem::take(&mut self.held_buttons);
        let Some(input) = &self.input else { return };
        for code in keys {
            let _ = input.inject(&InputEvent::Key {
                code,
                down: false,
                modifiers: Default::default(),
            });
        }
        for button in buttons {
            let _ = input.inject(&InputEvent::Button {
                button,
                down: false,
            });
        }
    }

    // --------------------------------------------------------------- clipboard

    async fn on_clipboard(&mut self, event: ud_clipboard::ClipboardEvent) {
        match event {
            ud_clipboard::ClipboardEvent::Error(message) => {
                trace!(%message, "clipboard error");
            }
            ud_clipboard::ClipboardEvent::Changed(payload) => {
                if !self.settings.clipboard.enabled {
                    return;
                }
                if !self.settings.clipboard.always
                    && !matches!(self.control, ControlState::Controlling { .. })
                {
                    return;
                }
                self.clipboard_status.last_kind = Some(payload.kind().to_string());
                self.clipboard_status.last_at = Some(ud_core::now_unix());
                self.clipboard_status.last_direction = Some(TransferDirection::Sending);
                for peer in self.connected_peers() {
                    if let Some(session) = self.sessions.get(&peer) {
                        let _ = session
                            .connection
                            .send(Message::Clipboard(payload.clone()))
                            .await;
                    }
                }
                self.publish().await;
            }
        }
    }

    async fn on_remote_clipboard(&mut self, peer: DeviceId, payload: ClipboardPayload) {
        if !self.settings.clipboard.enabled {
            return;
        }
        if payload.approx_bytes() > self.settings.clipboard.max_bytes {
            debug!(
                bytes = payload.approx_bytes(),
                "ignoring an oversized clipboard payload"
            );
            return;
        }
        if let Some(clipboard) = &self.clipboard {
            if let Err(err) = clipboard.apply(payload.clone()) {
                debug!(error = %err, "could not apply the remote clipboard");
            }
        }
        self.clipboard_status.last_kind = Some(payload.kind().to_string());
        self.clipboard_status.last_at = Some(ud_core::now_unix());
        self.clipboard_status.last_direction = Some(TransferDirection::Receiving);
        trace!(%peer, kind = payload.kind(), "clipboard received");
        self.publish().await;
    }

    // ------------------------------------------------------------ file transfer

    async fn send_files(&mut self, peer: &DeviceId, paths: Vec<PathBuf>) {
        if !self.settings.transfer.enabled {
            self.notify(Notice::warning("File transfer is turned off."));
            return;
        }
        let Some(session) = self.sessions.get(peer) else {
            self.notify(Notice::warning("That machine is not connected."));
            return;
        };
        if !session.state.is_connected() {
            self.notify(Notice::warning(
                "Pair with that machine before sending files.",
            ));
            return;
        }
        let entries = match collect_entries(&paths) {
            Ok(entries) if !entries.is_empty() => entries,
            Ok(_) => {
                self.notify(Notice::warning("Nothing to send."));
                return;
            }
            Err(err) => {
                self.notify(Notice::error(format!("Could not read those files: {err}")));
                return;
            }
        };
        let id = TransferId(rand::random());
        let transfer =
            OutgoingTransfer::new(id, peer.clone(), session.info.name.clone(), entries.clone());
        let offer = FileOffer {
            transfer: id,
            sender_name: self.settings.device_name.clone(),
            total_bytes: transfer.total_bytes,
            files: transfer.entries(),
        };
        if let Err(err) = session.connection.send(Message::FileOffer(offer)).await {
            self.notify(Notice::error(format!("Could not offer the files: {err}")));
            return;
        }
        self.transfers.insert(
            id,
            Tracked::Outgoing {
                transfer,
                state: TransferState::Pending,
            },
        );
        self.publish().await;
    }

    async fn on_file_offer(&mut self, peer: DeviceId, offer: FileOffer) {
        if !self.settings.transfer.enabled {
            if let Some(session) = self.sessions.get(&peer) {
                let _ = session
                    .connection
                    .send(Message::FileCancel {
                        transfer: offer.transfer,
                        reason: "file transfer is disabled here".into(),
                    })
                    .await;
            }
            return;
        }
        let peer_name = offer.sender_name.clone();
        let root = self.settings.transfer.download_dir.clone();
        let transfer = IncomingTransfer::new(
            offer.transfer,
            peer.clone(),
            peer_name.clone(),
            offer.files.clone(),
            root,
        );
        let trusted = self
            .sessions
            .get(&peer)
            .map(|session| session.trusted)
            .unwrap_or(false);
        self.transfers.insert(
            offer.transfer,
            Tracked::Incoming {
                transfer,
                handles: HashMap::new(),
                state: TransferState::Pending,
                error: None,
            },
        );
        self.notify(Notice::info(format!(
            "{peer_name} wants to send {} item(s).",
            offer.files.len()
        )));
        if trusted && self.settings.transfer.auto_accept_trusted {
            self.accept_transfer(offer.transfer).await;
        } else {
            self.publish().await;
        }
    }

    async fn accept_transfer(&mut self, id: TransferId) {
        let Some(Tracked::Incoming { transfer, state, .. }) = self.transfers.get_mut(&id) else {
            return;
        };
        if *state != TransferState::Pending {
            return;
        }
        *state = TransferState::Active;
        let peer = transfer.peer.clone();
        let directories: Vec<(usize, String)> = transfer
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.is_dir)
            .map(|(position, entry)| (position, entry.relative_path.clone()))
            .collect();
        let root = transfer.root.clone();

        for (position, relative) in directories {
            match sanitize_relative(&relative, &root) {
                Ok(path) => {
                    if let Err(err) = tokio::fs::create_dir_all(&path).await {
                        if let Some(Tracked::Incoming { error, .. }) = self.transfers.get_mut(&id) {
                            *error = Some(err.to_string());
                        }
                    }
                    if let Some(Tracked::Incoming { transfer, .. }) = self.transfers.get_mut(&id) {
                        transfer.resolved[position] = Some(path);
                    }
                }
                Err(err) => {
                    warn!(%err, "refusing an unsafe directory from a peer");
                }
            }
        }

        self.send_to(
            &peer,
            Message::FileAccept(FileAccept {
                transfer: id,
                rejected: Vec::new(),
            }),
        )
        .await;
        self.publish().await;
    }

    async fn cancel_transfer(&mut self, id: TransferId) {
        let peer = match self.transfers.get(&id) {
            Some(Tracked::Outgoing { transfer, .. }) => Some(transfer.peer.clone()),
            Some(Tracked::Incoming { transfer, .. }) => Some(transfer.peer.clone()),
            None => None,
        };
        if let Some(peer) = peer {
            self.send_to(
                &peer,
                Message::FileCancel {
                    transfer: id,
                    reason: "cancelled by the user".into(),
                },
            )
            .await;
        }
        if let Some(tracked) = self.transfers.get_mut(&id) {
            tracked.set_state(TransferState::Cancelled);
        }
        self.publish().await;
    }

    async fn on_file_accept(&mut self, accept: FileAccept) {
        let Some(Tracked::Outgoing { transfer, state }) = self.transfers.get_mut(&accept.transfer)
        else {
            return;
        };
        *state = TransferState::Active;
        transfer.accepted = true;
        let rejected: HashSet<u32> = accept.rejected.iter().copied().collect();
        let peer = transfer.peer.clone();
        let id = transfer.id;
        let files: Vec<OutgoingFile> = transfer
            .files
            .iter()
            .filter(|file| !rejected.contains(&file.entry.index))
            .cloned()
            .collect();
        let Some(session) = self.sessions.get(&peer) else {
            return;
        };
        let connection = session.connection.clone();
        let events = self.events.clone();
        spawn_sender(id, files, connection, events);
        self.publish().await;
    }

    async fn on_transfer_done(&mut self, id: TransferId, error: Option<String>) {
        let mut notice = None;
        if let Some(Tracked::Outgoing { transfer, state }) = self.transfers.get_mut(&id) {
            transfer.finished = true;
            *state = if error.is_some() {
                TransferState::Failed
            } else {
                TransferState::Completed
            };
            if transfer.done_bytes == 0 {
                transfer.done_bytes = transfer.total_bytes;
            }
            let name = transfer.peer_name.clone();
            notice = Some(match &error {
                Some(message) => {
                    Notice::error(format!("Sending to {name} failed: {message}"))
                }
                None => Notice::info(format!("Files sent to {name}.")),
            });
        }
        if let Some(notice) = notice {
            self.notify(notice);
        }
        self.publish().await;
    }

    async fn on_chunk(&mut self, chunk: FileChunk) {
        let overwrite = self.settings.transfer.overwrite_existing;
        let Some(Tracked::Incoming {
            transfer,
            handles,
            state,
            error,
        }) = self.transfers.get_mut(&chunk.transfer)
        else {
            return;
        };
        if *state != TransferState::Active {
            return;
        }
        let Some(position) = transfer.position_of(chunk.index) else {
            return;
        };
        if transfer.entries[position].is_dir {
            return;
        }

        let root = transfer.root.clone();
        let relative = transfer.entries[position].relative_path.clone();
        let path = match transfer.resolved[position].clone() {
            Some(path) => path,
            None => match sanitize_relative(&relative, &root) {
                Ok(path) => {
                    let path = if overwrite { path } else { unique_path(&path) };
                    if let Some(parent) = path.parent() {
                        if let Err(err) = tokio::fs::create_dir_all(parent).await {
                            *error = Some(err.to_string());
                            *state = TransferState::Failed;
                            return;
                        }
                    }
                    transfer.resolved[position] = Some(path.clone());
                    path
                }
                Err(err) => {
                    warn!(%err, "refusing an unsafe file path from a peer");
                    *error = Some(err.to_string());
                    *state = TransferState::Failed;
                    return;
                }
            },
        };

        // Take the handle out of the map so the borrow ends before the await.
        let mut handle = handles.remove(&chunk.index);
        if handle.is_none() {
            match tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .await
            {
                Ok(file) => handle = Some(file),
                Err(err) => {
                    *error = Some(err.to_string());
                    *state = TransferState::Failed;
                    return;
                }
            }
        }

        if let Some(file) = handle.as_mut() {
            let result = async {
                file.seek(std::io::SeekFrom::Start(chunk.offset)).await?;
                file.write_all(&chunk.data).await?;
                Ok::<(), std::io::Error>(())
            }
            .await;
            match result {
                Ok(()) => {
                    transfer.written[position] =
                        transfer.written[position].saturating_add(chunk.data.len() as u64);
                    transfer.done_bytes = transfer
                        .done_bytes
                        .saturating_add(chunk.data.len() as u64);
                }
                Err(err) => {
                    *error = Some(err.to_string());
                    *state = TransferState::Failed;
                }
            }
        }
        if let Some(file) = handle {
            handles.insert(chunk.index, file);
        }
    }

    async fn on_file_finished(&mut self, finished: FileFinished) {
        let mut notice = None;
        if let Some(Tracked::Incoming {
            transfer,
            handles,
            state,
            error,
        }) = self.transfers.get_mut(&finished.transfer)
        {
            handles.clear();
            *state = if let Some(failed) = finished.failed.first() {
                *error = Some(failed.1.clone());
                TransferState::Failed
            } else {
                TransferState::Completed
            };
            if transfer.done_bytes == 0 {
                transfer.done_bytes = transfer.total_bytes;
            }
            notice = Some(Notice::info(format!(
                "Files from {} saved to {}",
                transfer.peer_name,
                transfer.root.display()
            )));
        }
        if let Some(notice) = notice {
            self.notify(notice);
        }
        self.publish().await;
    }

    async fn on_file_cancelled(&mut self, id: TransferId, reason: String) {
        if let Some(tracked) = self.transfers.get_mut(&id) {
            tracked.set_state(TransferState::Cancelled);
        }
        self.notify(Notice::warning(format!("Transfer cancelled: {reason}")));
        self.publish().await;
    }

    // ------------------------------------------------------------ housekeeping

    async fn on_discovered(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::Found(found) => {
                let found = *found;
                if found.device_id == self.identity.device_id {
                    return;
                }
                self.table.upsert(found.clone());
                let trusted = self
                    .peers
                    .get(&found.device_id)
                    .map(|record| record.fingerprint == found.fingerprint)
                    .unwrap_or(false);
                let busy = self.sessions.contains_key(&found.device_id)
                    || self.connecting.contains(&found.device_id);
                if trusted && !busy && self.identity.device_id < found.device_id {
                    // Only the machine with the smaller id dials, so the two
                    // sides never connect to each other at the same moment.
                    self.connect_peer(&found.device_id);
                }
                self.publish().await;
            }
            DiscoveryEvent::Lost(id) => {
                if self.table.remove(&id) {
                    self.publish().await;
                }
            }
        }
    }

    async fn on_housekeeping(&mut self) {
        if self.discovery_dirty {
            self.rebuild_discovery().await;
        }
        // Permission grants can change while the application is running, and on
        // macOS they only take effect after a restart, so keep the reported
        // state honest rather than frozen at startup.
        self.permission_hint = ud_input::permission_status();
        self.table.prune(90);

        let now = Instant::now();
        let stale: Vec<DeviceId> = self
            .sessions
            .iter()
            .filter(|(_, session)| now.duration_since(session.last_rx) > PEER_TIMEOUT)
            .map(|(id, _)| id.clone())
            .collect();
        for peer in stale {
            warn!(%peer, "peer went silent");
            self.on_session_ended(&peer, "no response").await;
        }

        for peer in self.connected_peers() {
            self.send_to(&peer, Message::Ping {
                nonce: rand::random(),
            })
            .await;
        }
        self.publish().await;
    }

    async fn shutdown(&mut self) {
        if let Some(discovery) = &self.discovery {
            discovery.say_goodbye().await;
        }
        for (_, session) in self.sessions.drain() {
            let _ = session
                .connection
                .send(Message::Bye {
                    reason: "shutting down".into(),
                })
                .await;
            session.connection.close();
        }
        self.release_held_input().await;
        if let Some(input) = self.input.as_mut() {
            let _ = input.release_capture();
            input.shutdown();
        }
        if let Some(clipboard) = self.clipboard.as_mut() {
            clipboard.shutdown();
        }
        self.persist_settings();
        self.persist_peers();
    }

    // ------------------------------------------------------------------ output

    fn notify(&self, notice: Notice) {
        let _ = self.out.send(EngineEvent::Notice(notice));
    }

    async fn publish(&mut self) {
        let snapshot = self.snapshot();
        let _ = self.out.send(EngineEvent::Snapshot(Box::new(snapshot)));
    }

    fn snapshot(&self) -> Snapshot {
        let mut peers: Vec<PeerView> = Vec::new();
        let mut seen: HashSet<DeviceId> = HashSet::new();

        for found in self.table.list() {
            let session = self.sessions.get(&found.device_id);
            let record = self.peers.get(&found.device_id);
            seen.insert(found.device_id.clone());
            peers.push(PeerView {
                device_id: found.device_id.clone(),
                name: session
                    .map(|s| s.info.name.clone())
                    .unwrap_or_else(|| found.name.clone()),
                os: session.map(|s| s.info.os).unwrap_or(found.os),
                fingerprint: record
                    .map(|r| r.fingerprint.clone())
                    .unwrap_or_else(|| found.fingerprint.clone()),
                trusted: record.is_some(),
                connection: session
                    .map(|s| s.state.clone())
                    .unwrap_or(ConnectionState::Offline),
                discovered_via: Some(found.source.label().to_string()),
                address: found.best_address().map(|a| a.to_string()),
                last_seen: Some(found.last_seen),
                display_count: session.map(|s| s.displays.len()).unwrap_or(0),
                display_summary: session.and_then(summarize_displays),
                pairing: session.and_then(pairing_view),
                link: self.settings.link_for(&found.device_id).cloned(),
            });
        }

        for (id, session) in &self.sessions {
            if seen.contains(id) {
                continue;
            }
            peers.push(PeerView {
                device_id: id.clone(),
                name: session.info.name.clone(),
                os: session.info.os,
                fingerprint: session.info.fingerprint.clone(),
                trusted: session.trusted,
                connection: session.state.clone(),
                discovered_via: None,
                address: None,
                last_seen: Some(ud_core::now_unix()),
                display_count: session.displays.len(),
                display_summary: summarize_displays(session),
                pairing: pairing_view(session),
                link: self.settings.link_for(id).cloned(),
            });
        }

        // Trusted machines stay listed while they are away so they can still be
        // dialled by address or forgotten.
        for record in &self.peers.peers {
            if seen.contains(&record.device_id) || self.sessions.contains_key(&record.device_id) {
                continue;
            }
            peers.push(PeerView {
                device_id: record.device_id.clone(),
                name: record.name.clone(),
                os: record.os,
                fingerprint: record.fingerprint.clone(),
                trusted: true,
                connection: ConnectionState::Offline,
                discovered_via: None,
                address: record.last_address.clone(),
                last_seen: record.last_seen,
                display_count: 0,
                display_summary: None,
                pairing: None,
                link: self.settings.link_for(&record.device_id).cloned(),
            });
        }

        peers.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        let mut transfers: Vec<TransferView> =
            self.transfers.values().map(Tracked::view).collect();
        transfers.sort_by(|a, b| b.started_at.cmp(&a.started_at));

        let mode = match &self.control {
            ControlState::Local => ControlModeView::Local,
            ControlState::Controlling { peer, .. } => ControlModeView::Controlling {
                device_id: peer.clone(),
                name: self.name_of(peer),
            },
            ControlState::Controlled { peer, .. } => ControlModeView::Controlled {
                device_id: peer.clone(),
                name: self.name_of(peer),
            },
        };

        Snapshot {
            device: self.device.clone(),
            settings: self.settings.clone(),
            peers,
            control: ControlView {
                mode,
                release_hotkey: self.settings.input.release_hotkey.label().to_string(),
                sharing_enabled: self.settings.input.enabled,
            },
            transfers,
            input: InputStatusView {
                // Reflects reality rather than the compile time capability: if
                // the platform backend failed to start, the UI must not offer a
                // switch that cannot work.
                backend_available: self.input.is_some(),
                enabled: self.settings.input.enabled,
                capture_active: self.capture_active,
                permission_hint: self.permission_hint.clone(),
            },
            clipboard: self.clipboard_status.clone(),
            platform: PlatformView {
                os: OsKind::current(),
                displays: self.displays.clone(),
                desktop: self.desktop,
            },
            listening_port: if self.listening_port == 0 {
                self.settings.port
            } else {
                self.listening_port
            },
        }
    }

    fn name_of(&self, peer: &DeviceId) -> String {
        self.sessions
            .get(peer)
            .map(|session| session.info.name.clone())
            .or_else(|| self.peers.get(peer).map(|record| record.name.clone()))
            .unwrap_or_else(|| peer.short().to_string())
    }
}

/// Streams the accepted files into the connection without blocking the engine.
fn spawn_sender(
    id: TransferId,
    files: Vec<OutgoingFile>,
    connection: Arc<Connection>,
    events: mpsc::UnboundedSender<Event>,
) {
    tokio::spawn(async move {
        let mut buffer = vec![0u8; CHUNK_BYTES];
        let mut failure: Option<String> = None;
        for queued in files.iter() {
            let Some(source) = &queued.source else {
                continue;
            };
            let entry = &queued.entry;
            let mut file = match tokio::fs::File::open(source).await {
                Ok(file) => file,
                Err(err) => {
                    failure = Some(format!("could not open {}: {err}", source.display()));
                    break;
                }
            };
            let mut offset = 0u64;
            loop {
                match file.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => {
                        let chunk = FileChunk {
                            transfer: id,
                            index: entry.index,
                            offset,
                            data: buffer[..read].to_vec(),
                        };
                        if let Err(err) = connection.send_chunk(chunk).await {
                            failure = Some(err.to_string());
                            break;
                        }
                        offset += read as u64;
                        let _ = events.send(Event::TransferProgress {
                            id,
                            bytes: read as u64,
                        });
                    }
                    Err(err) => {
                        failure = Some(err.to_string());
                        break;
                    }
                }
            }
            if failure.is_some() {
                break;
            }
        }
        if failure.is_none() {
            let _ = connection
                .send(Message::FileFinished(FileFinished {
                    transfer: id,
                    failed: Vec::new(),
                }))
                .await;
        }
        let _ = events.send(Event::TransferDone {
            id,
            error: failure,
        });
    });
}

fn average_rate(started_at: u64, done: u64) -> u64 {
    let elapsed = ud_core::now_unix().saturating_sub(started_at).max(1);
    done / elapsed
}

fn summarize_displays(session: &Session) -> Option<String> {
    let bounds = desktop_bounds(&session.displays);
    if bounds.is_empty() {
        return None;
    }
    Some(format!(
        "{}x{} across {} display(s)",
        bounds.width as i64,
        bounds.height as i64,
        session.displays.len()
    ))
}

fn pairing_view(session: &Session) -> Option<PairingView> {
    let pairing = session.pairing.as_ref()?;
    Some(PairingView {
        code_to_share: pairing.code_to_share.clone(),
        awaiting_code: pairing.awaiting_code,
        remote_fingerprint: session.info.fingerprint.clone(),
    })
}

fn base64_key(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_digit()).collect()
}

fn open_in_file_manager(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

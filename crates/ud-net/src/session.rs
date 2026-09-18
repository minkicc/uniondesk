//! Encrypted peer sessions.
//!
//! The handshake is Noise `XX`, which authenticates both static keys but still
//! lets two strangers complete a connection — the trust decision is then made by
//! the engine, using the pinned key store and, for new peers, a pairing code.

use std::sync::Arc;

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use ud_core::identity::DeviceIdentity;
use ud_core::protocol::{noise_params, DeviceInfo, FileChunk, Message};

use crate::codec::{decode_inner, encode_inner, read_record, write_record, Incoming, Outgoing};
use crate::error::{Error, Result};

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
const URGENT_QUEUE: usize = 2048;
const NORMAL_QUEUE: usize = 256;

/// Result of the cryptographic handshake.
#[derive(Debug, Clone)]
pub struct HandshakeOutcome {
    /// Static public key of the peer, as proven by the handshake.
    pub remote_static: Vec<u8>,
    pub hello: DeviceInfo,
    pub initiator: bool,
}

/// A live, encrypted connection to one peer.
pub struct Connection {
    pub peer: DeviceInfo,
    pub remote_static: Vec<u8>,
    pub initiator: bool,
    urgent: mpsc::Sender<Outgoing>,
    normal: mpsc::Sender<Outgoing>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl Connection {
    /// Queues a latency critical message. Returns false when the queue is full or
    /// the connection is gone; input relaying must never block on the network.
    pub fn try_send_urgent(&self, message: Message) -> bool {
        match self.urgent.try_send(Outgoing::Message(message)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                trace!("urgent queue is full, dropping an input event");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Queues a message with backpressure, used for clipboard and control.
    pub async fn send(&self, message: Message) -> Result<()> {
        self.normal
            .send(Outgoing::Message(message))
            .await
            .map_err(|_| Error::Closed("connection is gone".into()))
    }

    pub async fn send_chunk(&self, chunk: FileChunk) -> Result<()> {
        self.normal
            .send(Outgoing::Chunk(chunk))
            .await
            .map_err(|_| Error::Closed("connection is gone".into()))
    }

    pub fn is_closed(&self) -> bool {
        self.cancel.is_cancelled() || self.normal.is_closed()
    }

    /// Returns a token that fires when the connection dies.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn close(&self) {
        self.cancel.cancel();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Dials a peer and completes the handshake as the initiator.
pub async fn dial(
    stream: TcpStream,
    identity: &DeviceIdentity,
    hello: DeviceInfo,
) -> Result<(Connection, mpsc::Receiver<Incoming>)> {
    stream.set_nodelay(true)?;
    let (outcome, read, write, state) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        run_handshake(stream, identity, hello, Role::Initiator),
    )
    .await
    .map_err(|_| Error::protocol("handshake timed out"))??;
    Ok(build(outcome, read, write, state))
}

/// Accepts an inbound peer and completes the handshake as the responder.
pub async fn accept(
    stream: TcpStream,
    identity: &DeviceIdentity,
    hello: DeviceInfo,
) -> Result<(Connection, mpsc::Receiver<Incoming>)> {
    stream.set_nodelay(true)?;
    let (outcome, read, write, state) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        run_handshake(stream, identity, hello, Role::Responder),
    )
    .await
    .map_err(|_| Error::protocol("handshake timed out"))??;
    Ok(build(outcome, read, write, state))
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Initiator,
    Responder,
}

async fn run_handshake(
    mut stream: TcpStream,
    identity: &DeviceIdentity,
    hello: DeviceInfo,
    role: Role,
) -> Result<(HandshakeOutcome, OwnedReadHalf, OwnedWriteHalf, snow::StatelessTransportState)> {
    let params = noise_params()?;
    let private = identity.private_key()?;
    let builder = snow::Builder::new(params).local_private_key(&private);
    let mut handshake = match role {
        Role::Initiator => builder.build_initiator(),
        Role::Responder => builder.build_responder(),
    }
    .map_err(Error::from)?;

    let payload = serde_json::to_vec(&hello)?;
    let mut buf = vec![0u8; crate::codec::MAX_RECORD];
    let mut plain = vec![0u8; crate::codec::MAX_RECORD];
    let mut record = Vec::new();

    let remote_hello: DeviceInfo = match role {
        Role::Initiator => {
            // -> e
            let len = handshake.write_message(&[], &mut buf)?;
            write_record(&mut stream, &buf[..len]).await?;

            // <- e, ee, s, es  (the responder's identity travels encrypted here)
            read_record(&mut stream, &mut record).await?;
            let len = handshake.read_message(&record, &mut plain)?;
            let peer_hello: DeviceInfo = serde_json::from_slice(&plain[..len])?;

            // -> s, se  (our identity, also encrypted)
            let len = handshake.write_message(&payload, &mut buf)?;
            write_record(&mut stream, &buf[..len]).await?;
            peer_hello
        }
        Role::Responder => {
            // <- e
            read_record(&mut stream, &mut record).await?;
            handshake.read_message(&record, &mut plain)?;

            // -> e, ee, s, es
            let len = handshake.write_message(&payload, &mut buf)?;
            write_record(&mut stream, &buf[..len]).await?;

            // <- s, se
            read_record(&mut stream, &mut record).await?;
            let len = handshake.read_message(&record, &mut plain)?;
            serde_json::from_slice(&plain[..len])?
        }
    };

    let remote_static = handshake
        .get_remote_static()
        .ok_or_else(|| Error::crypto("peer never presented a static key"))?
        .to_vec();

    debug!(
        peer = %remote_hello.name,
        initiator = role == Role::Initiator,
        "handshake complete"
    );

    let state = handshake.into_stateless_transport_mode()?;
    let outcome = HandshakeOutcome {
        remote_static,
        hello: remote_hello,
        initiator: role == Role::Initiator,
    };
    let (read, write) = stream.into_split();
    Ok((outcome, read, write, state))
}

fn build(
    outcome: HandshakeOutcome,
    read: OwnedReadHalf,
    write: OwnedWriteHalf,
    state: snow::StatelessTransportState,
) -> (Connection, mpsc::Receiver<Incoming>) {
    let (urgent_tx, urgent_rx) = mpsc::channel(URGENT_QUEUE);
    let (normal_tx, normal_rx) = mpsc::channel(NORMAL_QUEUE);
    let (incoming_tx, incoming_rx) = mpsc::channel(NORMAL_QUEUE);
    let cancel = CancellationToken::new();

    let state = Arc::new(parking_lot::Mutex::new(state));

    let reader = tokio::spawn(reader_loop(
        read,
        state.clone(),
        incoming_tx,
        cancel.clone(),
        outcome.hello.name.clone(),
    ));
    let writer = tokio::spawn(writer_loop(
        write,
        state,
        urgent_rx,
        normal_rx,
        cancel.clone(),
        outcome.hello.name.clone(),
    ));

    let connection = Connection {
        peer: outcome.hello,
        remote_static: outcome.remote_static,
        initiator: outcome.initiator,
        urgent: urgent_tx,
        normal: normal_tx,
        cancel,
        tasks: vec![reader, writer],
    };
    (connection, incoming_rx)
}

/// Reads records, decrypts them, and forwards the decoded items.
async fn reader_loop(
    mut read: OwnedReadHalf,
    state: Arc<parking_lot::Mutex<snow::StatelessTransportState>>,
    outgoing: mpsc::Sender<Incoming>,
    cancel: CancellationToken,
    peer: String,
) {
    let mut nonce: u64 = 0;
    let mut record = Vec::new();
    let mut plain = vec![0u8; crate::codec::MAX_RECORD];
    loop {
        let read_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            result = read_record(&mut read, &mut record) => result,
        };
        if let Err(err) = read_result {
            if !cancel.is_cancelled() {
                debug!(peer = %peer, error = %err, "peer connection ended");
            }
            break;
        }
        let decoded = {
            let state = state.lock();
            state
                .read_message(nonce, &record, &mut plain)
                .map_err(Error::from)
                .and_then(|len| decode_inner(&plain[..len]))
        };
        match decoded {
            Ok(item) => {
                nonce = nonce.wrapping_add(1);
                if outgoing.send(item).await.is_err() {
                    break;
                }
            }
            Err(err) => {
                warn!(peer = %peer, error = %err, "dropping an unreadable record");
                break;
            }
        }
    }
    cancel.cancel();
}

/// Serializes writes so that nonces always hit the wire in the order they were
/// produced, while letting input events overtake bulk transfers.
async fn writer_loop(
    mut write: OwnedWriteHalf,
    state: Arc<parking_lot::Mutex<snow::StatelessTransportState>>,
    mut urgent: mpsc::Receiver<Outgoing>,
    mut normal: mpsc::Receiver<Outgoing>,
    cancel: CancellationToken,
    peer: String,
) {
    let mut nonce: u64 = 0;
    let mut buf = vec![0u8; crate::codec::MAX_RECORD];
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            item = urgent.recv() => item,
            item = normal.recv() => item,
        };
        let Some(item) = next else { break };
        let result = encode_inner(&item).and_then(|plain| {
            let len = {
                let state = state.lock();
                state.write_message(nonce, &plain, &mut buf).map_err(Error::from)?
            };
            Ok(len)
        });
        match result {
            Ok(len) => {
                nonce = nonce.wrapping_add(1);
                if let Err(err) = write_record(&mut write, &buf[..len]).await {
                    debug!(peer = %peer, error = %err, "write failed");
                    break;
                }
            }
            Err(err) => {
                warn!(peer = %peer, error = %err, "could not encode outbound item");
                break;
            }
        }
    }
    cancel.cancel();
}

/// Test helper: runs a full handshake over an in-memory duplex stream.
#[cfg(test)]
async fn in_memory_pair(
    a: DeviceIdentity,
    a_hello: DeviceInfo,
    b: DeviceIdentity,
    b_hello: DeviceInfo,
) -> Result<((Connection, mpsc::Receiver<Incoming>), (Connection, mpsc::Receiver<Incoming>))> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        accept(stream, &b, b_hello).await
    });
    let client = dial(TcpStream::connect(addr).await?, &a, a_hello).await?;
    let server = server.await.unwrap()?;
    Ok((client, server))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ud_core::identity::DeviceId;
    use ud_core::protocol::Message;

    fn hello(identity: &DeviceIdentity, name: &str) -> DeviceInfo {
        DeviceInfo::new(
            identity.device_id.clone(),
            name,
            Vec::new(),
            &identity.public_key_raw().unwrap(),
        )
    }

    #[tokio::test]
    async fn handshake_authenticates_both_static_keys() {
        let a = DeviceIdentity::generate(DeviceId::new()).unwrap();
        let b = DeviceIdentity::generate(DeviceId::new()).unwrap();
        let a_pub = a.public_key_raw().unwrap();
        let b_pub = b.public_key_raw().unwrap();
        let (client, server) = in_memory_pair(
            a.clone(),
            hello(&a, "alpha"),
            b.clone(),
            hello(&b, "beta"),
        )
        .await
        .unwrap();

        assert_eq!(client.0.remote_static, b_pub);
        assert_eq!(server.0.remote_static, a_pub);
        assert_eq!(client.0.peer.name, "beta");
        assert_eq!(server.0.peer.name, "alpha");
        assert!(client.0.initiator);
        assert!(!server.0.initiator);
    }

    #[tokio::test]
    async fn messages_survive_the_round_trip() {
        let a = DeviceIdentity::generate(DeviceId::new()).unwrap();
        let b = DeviceIdentity::generate(DeviceId::new()).unwrap();
        let (client, server) = in_memory_pair(
            a.clone(),
            hello(&a, "alpha"),
            b.clone(),
            hello(&b, "beta"),
        )
        .await
        .unwrap();
        // Both halves must stay alive: dropping a Connection cancels its tasks.
        let (client, _client_rx) = client;
        let (_server, mut server_rx) = server;

        client.send(Message::Ping { nonce: 99 }).await.unwrap();
        let received = server_rx.recv().await.unwrap();
        assert_eq!(received, Incoming::Message(Message::Ping { nonce: 99 }));
    }
}

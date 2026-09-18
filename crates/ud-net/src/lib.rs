//! Transport, discovery and file transfer plumbing for UnionDesk.
//!
//! Every peer connection is a TCP stream wrapped in a Noise `XX` handshake, so
//! the channel is encrypted, integrity protected and mutually authenticated by
//! static keypairs. Two independent discovery paths find peers on the local
//! network: multicast DNS, and a UDP broadcast beacon for networks where mDNS is
//! filtered.

pub mod codec;
pub mod discovery;
pub mod error;
pub mod session;

pub use error::{Error, Result};

pub use codec::{Incoming, Outgoing};
pub use discovery::{Discovered, Discovery, DiscoveryEvent, DiscoverSource};
pub use session::{Connection, HandshakeOutcome};

/// Noise caps a single message at 65535 bytes, and the record carries a 16 byte
/// authentication tag plus a one byte inner tag. Chunk sizes stay comfortably
/// below that so a chunk always fits in one encrypted record.
pub const MAX_CHUNK: usize = 60 * 1024;

//! Shared domain types for UnionDesk.
//!
//! This crate is deliberately free of platform code and of any I/O beyond
//! reading and writing the on-disk configuration, so that it can be reused by
//! the daemon, the desktop app and tests.

pub mod config;
pub mod error;
pub mod geom;
pub mod identity;
pub mod input;
pub mod layout;
pub mod paths;
pub mod protocol;

pub use error::{Error, Result};

/// Protocol revision understood by this build. Peers negotiate on the lower of
/// the two revisions and refuse to talk when the gap is too wide.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default TCP port used for control, input, clipboard and file traffic.
pub const DEFAULT_PORT: u16 = 47_823;

/// UDP port used by the broadcast beacon fallback when multicast DNS is blocked.
pub const BEACON_PORT: u16 = 47_824;

/// Service type advertised over multicast DNS.
pub const MDNS_SERVICE_TYPE: &str = "_uniondesk._tcp.local.";

/// Human readable product name, used in window titles and mDNS records.
pub const PRODUCT_NAME: &str = "UnionDesk";

/// Seconds since the unix epoch, saturating at the epoch on platforms with a
/// clock before 1970.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

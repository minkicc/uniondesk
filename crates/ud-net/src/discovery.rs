//! Finding peers on the local network.
//!
//! Two independent mechanisms run in parallel:
//!
//! * multicast DNS (`_uniondesk._tcp.local.`) which is the polite, standards
//!   based path and also gives us hostnames;
//! * a UDP broadcast beacon, because plenty of consumer routers and corporate
//!   access points silently drop multicast.
//!
//! Whichever answers first wins; duplicates are merged by device id.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use ud_core::identity::DeviceId;
use ud_core::protocol::OsKind;
use ud_core::{BEACON_PORT, DEFAULT_PORT, MDNS_SERVICE_TYPE};

use crate::error::Result;

const BEACON_INTERVAL: Duration = Duration::from_secs(3);
const BEACON_MAGIC: &str = "uniondesk/1";

/// How the peer was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverSource {
    Mdns,
    Beacon,
    Manual,
}

impl DiscoverSource {
    pub fn label(self) -> &'static str {
        match self {
            DiscoverSource::Mdns => "Bonjour",
            DiscoverSource::Beacon => "Broadcast",
            DiscoverSource::Manual => "Manual",
        }
    }
}

/// One machine seen on the local network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discovered {
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub fingerprint: String,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    pub source: DiscoverSource,
    pub last_seen: u64,
}

impl Discovered {
    /// Best address to dial, preferring IPv4 for the widest compatibility.
    pub fn best_address(&self) -> Option<SocketAddr> {
        if let Some(v4) = self.addresses.iter().find(|a| a.is_ipv4()) {
            return Some(SocketAddr::new(*v4, self.port));
        }
        self.addresses
            .first()
            .map(|a| SocketAddr::new(*a, self.port))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiscoveryEvent {
    Found(Box<Discovered>),
    Lost(DeviceId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Beacon {
    magic: String,
    device_id: String,
    name: String,
    os: OsKind,
    fingerprint: String,
    port: u16,
    #[serde(default)]
    inquiry: bool,
    #[serde(default)]
    goodbye: bool,
}

/// Information this machine advertises.
#[derive(Debug, Clone)]
pub struct Advertise {
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub port: u16,
    pub fingerprint: String,
    pub enabled: bool,
}

/// Owns the discovery services. Dropping it stops advertising and browsing.
pub struct Discovery {
    cancel: CancellationToken,
    daemon: Option<mdns_sd::ServiceDaemon>,
    registered: Option<String>,
    beacon: Option<Arc<UdpSocket>>,
    advertise: Advertise,
    tasks: Vec<JoinHandle<()>>,
}

impl Discovery {
    /// Starts advertising this machine and listening for peers.
    pub async fn start(advertise: Advertise) -> Result<(Self, mpsc::Receiver<DiscoveryEvent>)> {
        let (tx, rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();

        let mut daemon = None;
        let mut registered = None;
        let mut tasks = Vec::new();

        if advertise.enabled {
            match Self::start_mdns(&advertise) {
                Ok((d, fullname, receiver)) => {
                    daemon = Some(d);
                    registered = Some(fullname);
                    tasks.push(tokio::spawn(mdns_loop(receiver, tx.clone(), cancel.clone())));
                }
                Err(err) => {
                    warn!(error = %err, "multicast DNS is unavailable, falling back to broadcast only");
                }
            }
        }

        let beacon = match Self::beacon_socket() {
            Ok(socket) => {
                let socket = Arc::new(socket);
                tasks.push(tokio::spawn(beacon_loop(
                    socket.clone(),
                    advertise.clone(),
                    tx.clone(),
                    cancel.clone(),
                )));
                Some(socket)
            }
            Err(err) => {
                warn!(error = %err, "could not open the broadcast beacon socket");
                None
            }
        };

        Ok((
            Discovery {
                cancel,
                daemon,
                registered,
                beacon,
                advertise,
                tasks,
            },
            rx,
        ))
    }

    fn start_mdns(
        advertise: &Advertise,
    ) -> Result<(mdns_sd::ServiceDaemon, String, mdns_sd::Receiver<mdns_sd::ServiceEvent>)> {
        let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| crate::Error::Other(e.to_string()))?;
        let host = format!("{}-uniondesk.local.", sanitize(&advertise.name));
        let mut properties = HashMap::new();
        properties.insert("id".to_string(), advertise.device_id.0.clone());
        properties.insert("name".to_string(), advertise.name.clone());
        properties.insert("os".to_string(), os_label(advertise.os).to_string());
        properties.insert("fp".to_string(), advertise.fingerprint.clone());
        properties.insert("v".to_string(), ud_core::PROTOCOL_VERSION.to_string());

        let service = mdns_sd::ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &advertise.name,
            &host,
            (),
            advertise.port,
            properties,
        )
        .map_err(|e| crate::Error::Other(e.to_string()))?
        .enable_addr_auto();

        let fullname = service.get_fullname().to_string();
        daemon
            .register(service)
            .map_err(|e| crate::Error::Other(e.to_string()))?;
        let receiver = daemon
            .browse(MDNS_SERVICE_TYPE)
            .map_err(|e| crate::Error::Other(e.to_string()))?;
        Ok((daemon, fullname, receiver))
    }

    fn beacon_socket() -> Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.set_broadcast(true)?;
        socket.set_nonblocking(true)?;
        let addr: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, BEACON_PORT));
        socket.bind(&addr.into())?;
        Ok(UdpSocket::from_std(socket.into())?)
    }

    /// Sends an immediate broadcast so the UI refreshes without waiting for the
    /// next scheduled beacon.
    pub async fn probe(&self) {
        let Some(socket) = &self.beacon else { return };
        if let Some(bytes) = our_beacon(&self.advertise, true, false) {
            let target = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, BEACON_PORT));
            let _ = socket.send_to(&bytes, target).await;
        }
    }

    /// Announces departure so peers can grey this machine out immediately.
    pub async fn say_goodbye(&self) {
        let Some(socket) = &self.beacon else { return };
        if let Some(bytes) = our_beacon(&self.advertise, false, true) {
            let target = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, BEACON_PORT));
            let _ = socket.send_to(&bytes, target).await;
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
        if let (Some(daemon), Some(fullname)) = (&self.daemon, &self.registered) {
            let _ = daemon.unregister(fullname);
        }
        if let Some(daemon) = &self.daemon {
            let _ = daemon.shutdown();
        }
    }
}

async fn beacon_loop(
    socket: Arc<UdpSocket>,
    advertise: Advertise,
    events: mpsc::Sender<DiscoveryEvent>,
    cancel: CancellationToken,
) {
    let target = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, BEACON_PORT));
    let mut buf = vec![0u8; 2048];
    let mut ticker = tokio::time::interval(BEACON_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                if advertise.enabled {
                    if let Some(bytes) = our_beacon(&advertise, false, false) {
                        let _ = socket.send_to(&bytes, target).await;
                    }
                }
            }
            received = socket.recv_from(&mut buf) => {
                let Ok((len, from)) = received else { continue };
                let Ok(beacon) = serde_json::from_slice::<Beacon>(&buf[..len]) else { continue };
                if beacon.magic != BEACON_MAGIC || beacon.device_id == advertise.device_id.0 {
                    continue;
                }
                let device_id = DeviceId(beacon.device_id.clone());
                if beacon.goodbye {
                    let _ = events.send(DiscoveryEvent::Lost(device_id)).await;
                    continue;
                }
                if beacon.inquiry && advertise.enabled {
                    // Answer the probe directly so the asker sees us at once.
                    // The reply describes *us*, not the machine that asked.
                    if let Some(bytes) = our_beacon(&advertise, false, false) {
                        let _ = socket.send_to(&bytes, from).await;
                    }
                }
                let found = Discovered {
                    device_id,
                    name: beacon.name,
                    os: beacon.os,
                    fingerprint: beacon.fingerprint,
                    addresses: vec![from.ip()],
                    port: if beacon.port == 0 { DEFAULT_PORT } else { beacon.port },
                    source: DiscoverSource::Beacon,
                    last_seen: ud_core::now_unix(),
                };
                trace!(peer = %found.name, "beacon");
                let _ = events.send(DiscoveryEvent::Found(Box::new(found))).await;
            }
        }
    }
}

async fn mdns_loop(
    receiver: mdns_sd::Receiver<mdns_sd::ServiceEvent>,
    events: mpsc::Sender<DiscoveryEvent>,
    cancel: CancellationToken,
) {
    // The resolver re-resolves a service every time the network answers, which
    // on a busy network means many events a second for a machine that has not
    // changed at all. Everything downstream reacts to a Found event — the peer
    // table, a snapshot, a UI redraw — so only forward an actual change, and at
    // most a refresh every so often even then.
    let mut seen: HashMap<DeviceId, (Vec<IpAddr>, String, Instant)> = HashMap::new();
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            event = receiver.recv_async() => event,
        };
        let Ok(event) = event else { break };
        match event {
            mdns_sd::ServiceEvent::ServiceResolved(info) => {
                let Some(device_id) = info.get_property_val_str("id") else {
                    continue;
                };
                // Prefer IPv4, which is what the connection actually uses. A
                // set that mixes in IPv6 records which arrive, expire and
                // reorder looks different on every resolution even though the
                // machine has not moved, which defeats the change detection
                // below.
                let mut addresses: Vec<IpAddr> = info
                    .get_addresses()
                    .iter()
                    .copied()
                    .filter(|a| a.is_ipv4() && !a.is_loopback())
                    .collect();
                if addresses.is_empty() {
                    addresses = info
                        .get_addresses()
                        .iter()
                        .copied()
                        .filter(|a| !a.is_loopback())
                        .collect();
                }
                addresses.sort();
                addresses.dedup();
                if addresses.is_empty() {
                    continue;
                }
                let found = Discovered {
                    device_id: DeviceId(device_id.to_string()),
                    name: info
                        .get_property_val_str("name")
                        .unwrap_or_else(|| info.get_fullname())
                        .to_string(),
                    os: parse_os(info.get_property_val_str("os").unwrap_or_default()),
                    fingerprint: info.get_property_val_str("fp").unwrap_or_default().to_string(),
                    addresses,
                    port: info.get_port(),
                    source: DiscoverSource::Mdns,
                    last_seen: ud_core::now_unix(),
                };
                let unchanged = seen
                    .get(&found.device_id)
                    .map(|(addresses, name, at)| {
                        *addresses == found.addresses
                            && *name == found.name
                            && at.elapsed() < Duration::from_secs(300)
                    })
                    .unwrap_or(false);
                // Any sighting that is not a change is dropped, and even a real
                // change is not worth forwarding more than once every few
                // seconds: the only consumer is a device list, and the chatter
                // otherwise reaches the UI as a redraw.
                let too_soon = seen
                    .get(&found.device_id)
                    .map(|(_, _, at)| at.elapsed() < Duration::from_secs(5))
                    .unwrap_or(false);
                if unchanged || too_soon {
                    continue;
                }
                seen.insert(
                    found.device_id.clone(),
                    (
                        found.addresses.clone(),
                        found.name.clone(),
                        Instant::now(),
                    ),
                );
                debug!(peer = %found.name, "multicast DNS");
                let _ = events.send(DiscoveryEvent::Found(Box::new(found))).await;
            }
            mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                trace!(service = %fullname, "service removed");
            }
            _ => {}
        }
    }
}

/// Merges repeated sightings of the same device, keeping the freshest address
/// list while remembering where it was first seen.
#[derive(Debug, Default)]
pub struct DiscoveryTable {
    order: Vec<DeviceId>,
    entries: HashMap<DeviceId, Discovered>,
}

impl DiscoveryTable {
    pub fn upsert(&mut self, found: Discovered) -> bool {
        use std::collections::hash_map::Entry;
        match self.entries.entry(found.device_id.clone()) {
            Entry::Occupied(mut slot) => {
                let previous = slot.get();
                let changed = previous.best_address() != found.best_address()
                    || previous.name != found.name;
                let mut merged = found;
                if merged.addresses.is_empty() {
                    merged.addresses = previous.addresses.clone();
                }
                merged.source = if previous.source == DiscoverSource::Mdns {
                    DiscoverSource::Mdns
                } else {
                    merged.source
                };
                slot.insert(merged);
                changed
            }
            Entry::Vacant(slot) => {
                self.order.push(found.device_id.clone());
                slot.insert(found);
                true
            }
        }
    }

    pub fn remove(&mut self, id: &DeviceId) -> bool {
        self.order.retain(|existing| existing != id);
        self.entries.remove(id).is_some()
    }

    pub fn get(&self, id: &DeviceId) -> Option<&Discovered> {
        self.entries.get(id)
    }

    pub fn list(&self) -> Vec<Discovered> {
        self.order
            .iter()
            .filter_map(|id| self.entries.get(id).cloned())
            .collect()
    }

    pub fn prune(&mut self, older_than_secs: u64) {
        let now = ud_core::now_unix();
        let stale: Vec<DeviceId> = self
            .entries
            .iter()
            .filter(|(_, d)| now.saturating_sub(d.last_seen) > older_than_secs)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.remove(&id);
        }
    }
}

fn os_label(os: OsKind) -> &'static str {
    match os {
        OsKind::Windows => "windows",
        OsKind::Macos => "macos",
        OsKind::Linux => "linux",
        OsKind::Unknown => "unknown",
    }
}

fn parse_os(value: &str) -> OsKind {
    match value {
        "windows" => OsKind::Windows,
        "macos" => OsKind::Macos,
        "linux" => OsKind::Linux,
        _ => OsKind::Unknown,
    }
}

/// mDNS instance names must not contain dots.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect()
}

/// Serializes the beacon that describes *this* machine.
fn our_beacon(advertise: &Advertise, inquiry: bool, goodbye: bool) -> Option<Vec<u8>> {
    let beacon = Beacon {
        magic: BEACON_MAGIC.to_string(),
        device_id: advertise.device_id.0.clone(),
        name: advertise.name.clone(),
        os: advertise.os,
        fingerprint: advertise.fingerprint.clone(),
        port: advertise.port,
        inquiry,
        goodbye,
    };
    serde_json::to_vec(&beacon).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discovered(id: &str, addr: &str) -> Discovered {
        Discovered {
            device_id: DeviceId(id.into()),
            name: "peer".into(),
            os: OsKind::Windows,
            fingerprint: "AAAA-BBBB-CCCC-DDDD".into(),
            addresses: vec![addr.parse().unwrap()],
            port: 47823,
            source: DiscoverSource::Beacon,
            last_seen: ud_core::now_unix(),
        }
    }

    #[test]
    fn the_table_keeps_one_entry_per_device() {
        let mut table = DiscoveryTable::default();
        assert!(table.upsert(discovered("a", "192.168.1.5")));
        assert!(!table.upsert(discovered("a", "192.168.1.5")));
        assert!(table.upsert(discovered("a", "192.168.1.9")));
        assert_eq!(table.list().len(), 1);
        assert_eq!(
            table.list()[0].best_address().unwrap().ip().to_string(),
            "192.168.1.9"
        );
    }

    #[test]
    fn stale_entries_are_pruned() {
        let mut table = DiscoveryTable::default();
        let mut old = discovered("a", "192.168.1.5");
        old.last_seen = 1;
        table.upsert(old);
        table.prune(60);
        assert!(table.list().is_empty());
    }

    #[test]
    fn sanitize_removes_dots() {
        assert_eq!(sanitize("Ruiqi's MacBook Pro"), "Ruiqi-s-MacBook-Pro");
    }
}

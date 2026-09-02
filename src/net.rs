//! The machinery that gets a blob from this screen onto someone else's.
//!
//! `wire.rs` is the format; this is the plumbing. Two transports:
//!
//! - **UDP multicast** on `239.255.71.11:47811` for presence. Each node shouts a
//!   `Beacon` once a second and listens for everyone else's. The socket sets
//!   `SO_REUSEADDR` and joins the group, which is what lets *two instances on one
//!   machine* discover each other — the only way to test any of this without a
//!   second computer in the room. A copy of each beacon also goes to the subnet
//!   broadcast address, because some networks drop multicast.
//! - **TCP** for the throw itself, on an ephemeral port announced in the beacon.
//!
//! # Why TCP, and why an acknowledgement
//!
//! A dropped UDP packet would mean a blob that left one screen and arrived on none.
//! The blob must never be in two places and never in no place, so a throw is a
//! *transfer with a receipt*: the sender keeps simulating the blob, sliding off the
//! edge, until the peer says it has it. `Landed` deletes it here. `Refused`, `Lost`,
//! or silence past the deadline bounces it back onto this screen. **The wall is the
//! fallback** — every failure mode ends with the blob still yours.
//!
//! # Threads, and why the render loop never blocks
//!
//! Sockets are slow and the frame loop is not allowed to wait for one. Every
//! blocking operation lives on its own thread and reports back through an mpsc
//! channel that `poll()` drains once a frame:
//!
//! - one discovery thread owning the UDP socket,
//! - one accept thread owning the listener, plus a thread per live connection,
//! - one short-lived thread per outbound throw.
//!
//! Throws are human-paced, so a thread each is far cheaper than the complexity of
//! non-blocking connect state machines.
//!
//! # This is not authenticated
//!
//! Anyone on your network who speaks the protocol can throw a blob at you or read
//! your beacon. That is why none of it runs unless you pass `--net`. `--net-group`
//! scopes discovery so two pairs of machines can play independently; it is a name,
//! not a password, and it is not a security boundary.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::wire::{self, Beacon, DISCOVERY_PORT, Edge, Message, NackReason, SatState, Throw};

// ---------------------------------------------------------------------------
// TUNABLES
// ---------------------------------------------------------------------------

/// Administratively-scoped multicast group. Stays on the local network.
pub const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 71, 11);

/// How often a node announces itself.
const BEACON_INTERVAL: Duration = Duration::from_millis(1000);
/// A peer that has not been heard from for this long is gone.
const PEER_TIMEOUT: Duration = Duration::from_millis(4500);
/// How long to wait for a TCP connection to the peer.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(700);
/// How long to wait for the receipt before giving up and bouncing the blob back.
const ACK_TIMEOUT: Duration = Duration::from_millis(900);
/// Blocking recv timeout on the discovery socket, so the thread can still tick.
const UDP_POLL: Duration = Duration::from_millis(250);
/// How many recently-accepted throw ids to remember, so a re-sent throw is
/// acknowledged a second time rather than landing as a second blob.
const SEEN_RING: usize = 64;

// ---------------------------------------------------------------------------

/// How this node presents itself, and who it is willing to talk to.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Discovery scope. Only peers announcing the same group are considered.
    pub group: String,
    /// Human-readable, for logs and for the peer list. Defaults to the hostname.
    pub name: String,
    /// How many blobs this screen will hold before it starts refusing throws.
    pub capacity: u16,
    /// Announce ourselves and listen for others. `--peer` works without it.
    pub discovery: bool,
    /// Peers named on the command line, each optionally pinned to one edge.
    pub pinned: Vec<(Option<Edge>, SocketAddr)>,
}

/// A peer we can throw to.
#[derive(Clone, Debug)]
pub struct Peer {
    pub node_id: u64,
    pub addr: SocketAddr,
    pub name: String,
    pub screen: (u32, u32),
    pub blobs: u16,
    pub capacity: u16,
    pub last_seen: Instant,
    /// Set when the peer came from `--peer <edge>=<host>` rather than a beacon.
    pub pinned_edge: Option<Edge>,
}

impl Peer {
    /// How this peer is named in the log: its own name plus the address, or just
    /// the address if it never told us one.
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.addr.to_string()
        } else {
            format!("{} ({})", self.name, self.addr)
        }
    }

    pub fn looks_full(&self) -> bool {
        self.capacity > 0 && self.blobs >= self.capacity
    }
}

/// Something that happened on the network, delivered to the frame loop by `poll`.
#[derive(Debug)]
pub enum NetEvent {
    /// A blob arrived and has been acknowledged. The frame loop must spawn it.
    Arrived { throw: Throw, from: SocketAddr },
    /// The peer has our blob. Delete the local copy.
    Landed { throw_id: u64, peer: String },
    /// The peer said no. Bounce the blob back in.
    Refused { throw_id: u64, peer: String, why: NackReason },
    /// Nobody answered, or the connection failed. Bounce the blob back in.
    Lost { throw_id: u64, peer: String, why: String },
    PeerUp { name: String, addr: SocketAddr, screen: (u32, u32) },
    PeerDown { name: String },
    /// Something worth printing that is nobody's fault in particular.
    Note(String),
}

/// Shared counters the socket threads read without taking a lock.
struct Shared {
    /// Blobs currently on this screen, published by the frame loop each frame.
    resident: AtomicU32,
    /// Slots promised to throws we have acknowledged but not yet spawned. Without
    /// this, two throws arriving in the same millisecond both see room for one.
    reserved: AtomicU32,
    capacity: u32,
    node_id: u64,
    /// Our own listening port, so an outbound throw can tell the peer where to
    /// throw back to.
    tcp_port: AtomicU64,
}

impl Shared {
    fn has_room(&self) -> bool {
        let used = self.resident.load(Ordering::Relaxed) + self.reserved.load(Ordering::Relaxed);
        used < self.capacity
    }
}

pub struct Net {
    cfg: NetConfig,
    shared: Arc<Shared>,
    peers: Arc<Mutex<HashMap<u64, Peer>>>,
    events: mpsc::Receiver<NetEvent>,
    tx: mpsc::Sender<NetEvent>,
    /// Local address the listener bound to.
    pub local_addr: SocketAddr,
    next_throw: u64,
    /// Peers learned from an inbound throw rather than a beacon. Keyed by node id.
    /// This is what makes catch work when discovery is off on one side.
    learned: Arc<Mutex<HashMap<u64, Peer>>>,
}

impl Net {
    /// Bind the sockets and start the background threads.
    pub fn start(cfg: NetConfig) -> Result<Net, String> {
        let node_id = random_node_id();

        let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
            .map_err(|e| format!("could not bind a TCP port for incoming throws: {e}"))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| format!("could not read back the listening address: {e}"))?;

        let shared = Arc::new(Shared {
            resident: AtomicU32::new(0),
            reserved: AtomicU32::new(0),
            capacity: cfg.capacity.max(1) as u32,
            node_id,
            tcp_port: AtomicU64::new(local_addr.port() as u64),
        });
        let peers: Arc<Mutex<HashMap<u64, Peer>>> = Arc::new(Mutex::new(HashMap::new()));
        let learned: Arc<Mutex<HashMap<u64, Peer>>> = Arc::new(Mutex::new(HashMap::new()));
        let (tx, events) = mpsc::channel();

        spawn_accept_thread(listener, shared.clone(), tx.clone(), learned.clone());

        if cfg.discovery {
            match open_discovery_socket() {
                Ok(sock) => spawn_discovery_thread(
                    sock,
                    cfg.clone(),
                    shared.clone(),
                    peers.clone(),
                    tx.clone(),
                ),
                Err(e) => {
                    // Not fatal: `--peer` still works, and so does receiving. Say so
                    // rather than pretending discovery is running.
                    let _ = tx.send(NetEvent::Note(format!(
                        "discovery is off ({e}); peers named with --peer still work, \
                         and this node can still be thrown to"
                    )));
                }
            }
        }

        Ok(Net {
            cfg,
            shared,
            peers,
            events,
            tx,
            local_addr,
            next_throw: 1,
            learned,
        })
    }

    /// Tell the network layer how many blobs are on screen. Called once a frame;
    /// it feeds both the beacon and the capacity check on inbound throws.
    pub fn set_resident(&self, n: usize) {
        self.shared.resident.store(n as u32, Ordering::Relaxed);
    }

    /// Consume one reserved slot: the frame loop has now actually spawned the blob
    /// it acknowledged.
    pub fn commit_arrival(&self) {
        // `fetch_update` rather than `fetch_sub`, so a bookkeeping slip can never
        // wrap the counter to four billion and wedge the capacity check forever.
        let _ = self.shared.reserved.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |v| Some(v.saturating_sub(1)),
        );
    }

    /// Everything that happened since the last call.
    pub fn poll(&self) -> Vec<NetEvent> {
        self.events.try_iter().collect()
    }

    /// Every peer we could throw to right now, beacons and `--peer` alike.
    pub fn peers(&self) -> Vec<Peer> {
        let mut out: Vec<Peer> = Vec::new();
        if let Ok(m) = self.peers.lock() {
            out.extend(m.values().cloned());
        }
        if let Ok(m) = self.learned.lock() {
            for p in m.values() {
                if !out.iter().any(|q| q.addr == p.addr) {
                    out.push(p.clone());
                }
            }
        }
        for (edge, addr) in &self.cfg.pinned {
            if out.iter().any(|q| q.addr == *addr) {
                continue;
            }
            out.push(Peer {
                node_id: 0,
                addr: *addr,
                name: addr.to_string(),
                screen: (0, 0),
                blobs: 0,
                capacity: 0,
                last_seen: Instant::now(),
                pinned_edge: *edge,
            });
        }
        // Stable order, so edge assignment does not shuffle as beacons arrive.
        out.sort_by_key(|p| (p.node_id, p.addr.to_string()));
        out
    }

    /// Which peer a blob leaving `edge` should be thrown to.
    ///
    /// The rules, in order:
    /// 1. A peer pinned to this edge with `--peer <edge>=<host>` wins outright.
    /// 2. With exactly one peer, **every** edge leads to it. Two machines side by
    ///    side is the case worth optimising for, and it needs no configuration at
    ///    all: throw the blob off any edge and it turns up on the other screen.
    /// 3. With several, peers are ordered by node id and dealt onto the edges
    ///    right, left, bottom, top. Deterministic, and both ends agree on it, but
    ///    it is arbitrary — pin them with `--peer` if you care which is which.
    pub fn peer_for_edge(&self, edge: Edge) -> Option<Peer> {
        let all = self.peers();
        if let Some(p) = all.iter().find(|p| p.pinned_edge == Some(edge)) {
            return Some(p.clone());
        }
        let mut free: Vec<&Peer> = all.iter().filter(|p| p.pinned_edge.is_none()).collect();
        // A peer whose last beacon said it was full will refuse the throw and the
        // blob will bounce. That is a correct outcome, but a pointless one when
        // somebody else has room, so full peers go to the back of the queue rather
        // than out of it — their beacon may simply be a second stale.
        if free.iter().any(|p| !p.looks_full()) {
            free.retain(|p| !p.looks_full());
        }
        match free.len() {
            0 => None,
            1 => Some(free[0].clone()),
            _ => {
                const ORDER: [Edge; 4] = [Edge::Right, Edge::Left, Edge::Bottom, Edge::Top];
                let slot = ORDER.iter().position(|e| *e == edge)?;
                free.get(slot % free.len()).map(|p| (*p).clone())
            }
        }
    }

    /// Bitmask of edges that currently lead somewhere, for the physics to treat as
    /// doors rather than walls.
    pub fn portal_edges(&self) -> u8 {
        let mut mask = 0;
        for e in Edge::ALL {
            if self.peer_for_edge(e).is_some() {
                mask |= e.bit();
            }
        }
        mask
    }

    /// Send a blob to the peer on `edge`. Returns the throw id to watch for, or
    /// `None` if there is nobody there after all (a peer can time out between the
    /// physics deciding to leave and this call).
    ///
    /// Never blocks: the connect, the write and the wait for the receipt all happen
    /// on a thread that reports back through `poll`.
    pub fn throw(
        &mut self,
        edge: Edge,
        along: f32,
        vel: (f32, f32),
        sats: Vec<SatState>,
    ) -> Option<(u64, Peer)> {
        let peer = self.peer_for_edge(edge)?;
        let throw_id = (self.shared.node_id << 16) ^ self.next_throw;
        self.next_throw = self.next_throw.wrapping_add(1);

        let msg = Throw {
            throw_id,
            from_node: self.shared.node_id,
            from_port: self.shared.tcp_port.load(Ordering::Relaxed) as u16,
            edge,
            along,
            vel_x: vel.0,
            vel_y: vel.1,
            sats,
        };
        let bytes = msg.encode();
        let tx = self.tx.clone();
        let target = peer.clone();
        let label = peer_label(&peer);
        std::thread::spawn(move || {
            let ev = match deliver(target.addr, &bytes, throw_id) {
                Ok(Delivery::Ack) => NetEvent::Landed { throw_id, peer: label },
                Ok(Delivery::Nack(why)) => NetEvent::Refused { throw_id, peer: label, why },
                Err(e) => NetEvent::Lost { throw_id, peer: label, why: e },
            };
            let _ = tx.send(ev);
        });
        Some((throw_id, peer))
    }
}

fn peer_label(p: &Peer) -> String {
    p.label()
}

enum Delivery {
    Ack,
    Nack(NackReason),
}

/// Connect, send one throw, wait for the receipt. Runs on its own thread.
fn deliver(addr: SocketAddr, bytes: &[u8], throw_id: u64) -> Result<Delivery, String> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("could not reach {addr}: {e}"))?;
    // Nagle would sit on a throw this small waiting for company it will never get.
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(ACK_TIMEOUT))
        .map_err(|e| format!("could not set a read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(CONNECT_TIMEOUT))
        .map_err(|e| format!("could not set a write timeout: {e}"))?;
    stream.write_all(bytes).map_err(|e| format!("could not send the throw: {e}"))?;
    stream.flush().ok();

    let deadline = Instant::now() + ACK_TIMEOUT;
    let mut buf = Vec::with_capacity(64);
    let mut chunk = [0u8; 256];
    loop {
        if Instant::now() >= deadline {
            return Err("no receipt within the deadline".into());
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err("the peer closed the connection without a receipt".into()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("no receipt within the deadline".into());
            }
            Err(e) => return Err(format!("reading the receipt failed: {e}")),
        }
        loop {
            match wire::decode(&buf) {
                Ok((Message::Ack(id), used)) => {
                    buf.drain(..used);
                    if id == throw_id {
                        return Ok(Delivery::Ack);
                    }
                }
                Ok((Message::Nack(id, why), used)) => {
                    buf.drain(..used);
                    if id == throw_id {
                        return Ok(Delivery::Nack(why));
                    }
                }
                // Anything else on this socket is not ours to act on; skip it.
                Ok((_, used)) => {
                    buf.drain(..used);
                }
                Err(wire::DecodeError::Truncated) => break,
                Err(e) => return Err(format!("the receipt did not parse: {e}")),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Inbound
// ---------------------------------------------------------------------------

fn spawn_accept_thread(
    listener: TcpListener,
    shared: Arc<Shared>,
    tx: mpsc::Sender<NetEvent>,
    learned: Arc<Mutex<HashMap<u64, Peer>>>,
) {
    let seen: Arc<Mutex<VecDeque<u64>>> = Arc::new(Mutex::new(VecDeque::with_capacity(SEEN_RING)));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let shared = shared.clone();
                    let tx = tx.clone();
                    let seen = seen.clone();
                    let learned = learned.clone();
                    std::thread::spawn(move || {
                        serve_connection(stream, shared, tx, seen, learned);
                    });
                }
                Err(e) => {
                    let _ = tx.send(NetEvent::Note(format!("could not accept a connection: {e}")));
                    // A listener that is broken stays broken; do not spin on it.
                    if e.kind() != std::io::ErrorKind::Interrupted {
                        break;
                    }
                }
            }
        }
    });
}

/// Read throws off one connection until it closes, acknowledging each.
///
/// The decision to accept or refuse is taken *here*, not on the frame loop, so the
/// peer gets its receipt at network speed rather than at frame rate. The reserved
/// counter is what keeps that honest.
fn serve_connection(
    mut stream: TcpStream,
    shared: Arc<Shared>,
    tx: mpsc::Sender<NetEvent>,
    seen: Arc<Mutex<VecDeque<u64>>>,
    learned: Arc<Mutex<HashMap<u64, Peer>>>,
) {
    let from = match stream.peer_addr() {
        Ok(a) => a,
        Err(_) => return,
    };
    let _ = stream.set_nodelay(true);
    // Long enough that an idle link stays up between throws, short enough that a
    // peer that vanished does not hold a thread forever.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));

    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return,
        }
        loop {
            match wire::decode(&buf) {
                Ok((Message::Throw(t), used)) => {
                    buf.drain(..used);
                    // Remember where this came from, so we can throw back even if
                    // discovery never told us about them.
                    if t.from_port != 0 {
                        let addr = SocketAddr::new(from.ip(), t.from_port);
                        if let Ok(mut m) = learned.lock() {
                            m.entry(t.from_node)
                                .and_modify(|p| {
                                    p.addr = addr;
                                    p.last_seen = Instant::now();
                                })
                                .or_insert(Peer {
                                    node_id: t.from_node,
                                    addr,
                                    name: addr.to_string(),
                                    screen: (0, 0),
                                    blobs: 0,
                                    capacity: 0,
                                    last_seen: Instant::now(),
                                    pinned_edge: None,
                                });
                        }
                    }

                    let already = {
                        let mut s = match seen.lock() {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                        if s.contains(&t.throw_id) {
                            true
                        } else {
                            if s.len() == SEEN_RING {
                                s.pop_front();
                            }
                            s.push_back(t.throw_id);
                            false
                        }
                    };

                    let reply = if already {
                        // A re-sent throw. Acknowledge it again; do NOT spawn a
                        // second blob.
                        wire::encode_ack(t.throw_id)
                    } else if shared.has_room() {
                        shared.reserved.fetch_add(1, Ordering::Relaxed);
                        let id = t.throw_id;
                        if tx.send(NetEvent::Arrived { throw: t, from }).is_err() {
                            // The frame loop is gone; give the slot back and refuse.
                            shared.reserved.fetch_sub(1, Ordering::Relaxed);
                            wire::encode_nack(id, NackReason::Full)
                        } else {
                            wire::encode_ack(id)
                        }
                    } else {
                        wire::encode_nack(t.throw_id, NackReason::Full)
                    };
                    if stream.write_all(&reply).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                }
                Ok((_, used)) => {
                    buf.drain(..used);
                }
                Err(wire::DecodeError::Truncated) => break,
                Err(e) => {
                    let _ = tx.send(NetEvent::Note(format!("dropped a frame from {from}: {e}")));
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// A UDP socket bound to the discovery port with `SO_REUSEADDR`, joined to the
/// multicast group, with loopback on.
///
/// `SO_REUSEADDR` before `bind` is the whole reason `socket2` is a dependency:
/// `std::net::UdpSocket` gives no way to set it, and without it a second instance
/// on the same machine cannot bind the port — which would make it impossible to try
/// any of this out without two computers.
fn open_discovery_socket() -> Result<std::net::UdpSocket, String> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| format!("could not create the discovery socket: {e}"))?;
    // Linux delivers a multicast or broadcast datagram to *every* socket bound to
    // the port with this set, which is exactly what two instances on one machine
    // need. (Unicast would go to only one of them, but nothing here is unicast.)
    sock.set_reuse_address(true)
        .map_err(|e| format!("could not set SO_REUSEADDR: {e}"))?;
    // The BSDs, macOS included, want SO_REUSEPORT as well before they will let a
    // second socket bind the same port — SO_REUSEADDR alone is enough on Linux but
    // not there. Best-effort: a platform that refuses it can still run one instance,
    // which is the normal case anyway.
    #[cfg(unix)]
    {
        let _ = sock.set_reuse_port(true);
    }
    let bind: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT).into();
    sock.bind(&bind.into())
        .map_err(|e| format!("could not bind UDP {DISCOVERY_PORT}: {e}"))?;
    sock.join_multicast_v4(&MULTICAST_GROUP, &Ipv4Addr::UNSPECIFIED)
        .map_err(|e| format!("could not join {MULTICAST_GROUP}: {e}"))?;
    // Without loopback we would not hear the other instance on this machine.
    let _ = sock.set_multicast_loop_v4(true);
    let _ = sock.set_broadcast(true);
    sock.set_read_timeout(Some(UDP_POLL))
        .map_err(|e| format!("could not set a read timeout: {e}"))?;
    Ok(sock.into())
}

fn spawn_discovery_thread(
    sock: std::net::UdpSocket,
    cfg: NetConfig,
    shared: Arc<Shared>,
    peers: Arc<Mutex<HashMap<u64, Peer>>>,
    tx: mpsc::Sender<NetEvent>,
) {
    std::thread::spawn(move || {
        let mcast: SocketAddr = SocketAddrV4::new(MULTICAST_GROUP, DISCOVERY_PORT).into();
        let bcast: SocketAddr =
            SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT).into();
        let mut last_beacon = Instant::now() - BEACON_INTERVAL;
        let mut buf = [0u8; 2048];

        loop {
            if last_beacon.elapsed() >= BEACON_INTERVAL {
                last_beacon = Instant::now();
                let b = Beacon {
                    node_id: shared.node_id,
                    tcp_port: shared.tcp_port.load(Ordering::Relaxed) as u16,
                    // Filled in by the frame loop through `resident`; the screen size
                    // is stamped once at start-up and republished here.
                    screen_w: SCREEN_W.load(Ordering::Relaxed) as u32,
                    screen_h: SCREEN_H.load(Ordering::Relaxed) as u32,
                    blobs: shared.resident.load(Ordering::Relaxed).min(u16::MAX as u32) as u16,
                    capacity: shared.capacity.min(u16::MAX as u32) as u16,
                    group: cfg.group.clone(),
                    name: cfg.name.clone(),
                }
                .encode();
                let _ = sock.send_to(&b, mcast);
                // Multicast is dropped by some switches and some VPN interfaces;
                // the broadcast copy is the fallback. Duplicates are harmless —
                // beacons are idempotent and keyed by node id.
                let _ = sock.send_to(&b, bcast);

                // Prune the departed.
                if let Ok(mut m) = peers.lock() {
                    let gone: Vec<u64> = m
                        .iter()
                        .filter(|(_, p)| p.last_seen.elapsed() > PEER_TIMEOUT)
                        .map(|(k, _)| *k)
                        .collect();
                    for k in gone {
                        if let Some(p) = m.remove(&k) {
                            let _ = tx.send(NetEvent::PeerDown { name: peer_label(&p) });
                        }
                    }
                }
            }

            let (n, from) = match sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => continue,
            };
            let Ok((msg, _)) = wire::decode(&buf[..n]) else { continue };
            let Message::Beacon(b) = msg else { continue };
            if b.node_id == shared.node_id || b.group != cfg.group {
                continue;
            }
            let addr = SocketAddr::new(from.ip(), b.tcp_port);
            let peer = Peer {
                node_id: b.node_id,
                addr,
                name: b.name.clone(),
                screen: (b.screen_w, b.screen_h),
                blobs: b.blobs,
                capacity: b.capacity,
                last_seen: Instant::now(),
                pinned_edge: None,
            };
            if let Ok(mut m) = peers.lock() {
                let fresh = !m.contains_key(&b.node_id);
                m.insert(b.node_id, peer.clone());
                if fresh {
                    let _ = tx.send(NetEvent::PeerUp {
                        name: peer_label(&peer),
                        addr,
                        screen: peer.screen,
                    });
                }
            }
        }
    });
}

/// The screen size the beacon advertises. Stamped once at start-up by the frame
/// loop; globals rather than plumbing because the discovery thread is the only
/// reader and it wants the freshest value without holding a lock.
static SCREEN_W: AtomicU64 = AtomicU64::new(0);
static SCREEN_H: AtomicU64 = AtomicU64::new(0);

pub fn publish_screen_size(w: u32, h: u32) {
    SCREEN_W.store(w as u64, Ordering::Relaxed);
    SCREEN_H.store(h as u64, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------

/// A per-process identifier that does not need a UUID crate to be unique enough:
/// process id, plus the wall clock, plus the address of a heap allocation.
fn random_node_id() -> u64 {
    let pid = std::process::id() as u64;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let boxed = Box::new(0u8);
    let addr = (&*boxed as *const u8) as u64;
    pid.rotate_left(32) ^ now.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ addr.rotate_left(17)
}

/// This machine's name, for the peer list. Best-effort: a missing hostname is not
/// worth failing to start over.
///
/// Via the `gethostname` crate rather than `/proc/sys/kernel/hostname`, because the
/// peer at the other end of a throw is expected to be a Mac and that path does not
/// exist there.
pub fn hostname() -> String {
    let name = gethostname::gethostname();
    let name = name.to_string_lossy();
    let name = name.trim();
    if name.is_empty() { "liquidmetal".into() } else { name.to_string() }
}

/// Parse a `--peer` value: `host`, `host:port`, or `<edge>=host[:port]`.
///
/// A bare host gets the discovery port, which is wrong on purpose: there is no
/// fixed TCP port to guess, so a peer named without one is only usable once its
/// beacon has been heard. Naming the port explicitly is what makes `--peer` work
/// with discovery switched off entirely.
pub fn parse_peer(spec: &str) -> Result<(Option<Edge>, SocketAddr), String> {
    let (edge, rest) = match spec.split_once('=') {
        Some((e, r)) => {
            let edge = match e.trim().to_ascii_lowercase().as_str() {
                "left" => Edge::Left,
                "right" => Edge::Right,
                "top" | "up" => Edge::Top,
                "bottom" | "down" => Edge::Bottom,
                other => {
                    return Err(format!(
                        "{other:?} is not an edge; use left, right, top or bottom"
                    ));
                }
            };
            (Some(edge), r)
        }
        None => (None, spec),
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Err("no host given".into());
    }
    let with_port =
        if rest.contains(':') { rest.to_string() } else { format!("{rest}:{DISCOVERY_PORT}") };
    let mut addrs = with_port
        .to_socket_addrs()
        .map_err(|e| format!("could not resolve {rest:?}: {e}"))?;
    // IPv4 first: the discovery half of this is v4-only, so preferring v4 keeps a
    // dual-stack host's two halves talking about the same address.
    let list: Vec<SocketAddr> = addrs.by_ref().collect();
    let addr = list
        .iter()
        .find(|a| matches!(a.ip(), IpAddr::V4(_)))
        .or_else(|| list.first())
        .copied()
        .ok_or_else(|| format!("{rest:?} resolved to no addresses"))?;
    Ok((edge, addr))
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test here binds real sockets and speaks the real protocol over them —
    /// that is the point, since the encode/decode half is already covered in
    /// `wire.rs` and what is left to get wrong is the plumbing.
    ///
    /// Discovery is off throughout: a `cargo test` run must not start broadcasting
    /// on whatever network the machine happens to be on.
    fn node(capacity: u16, peers: &[SocketAddr]) -> Net {
        Net::start(NetConfig {
            group: "test".into(),
            name: "test-node".into(),
            capacity,
            discovery: false,
            pinned: peers.iter().map(|a| (None, *a)).collect(),
        })
        .expect("a node starts")
    }

    /// Poll until `f` yields something, or give up. Threads and sockets mean this
    /// cannot be instantaneous, but on loopback it is a millisecond or two.
    fn wait_for<T>(net: &Net, mut f: impl FnMut(NetEvent) -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            for ev in net.poll() {
                if let Some(v) = f(ev) {
                    return Some(v);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    fn a_throw() -> (f32, (f32, f32), Vec<SatState>) {
        (
            0.4,
            (1.25, -0.3),
            vec![SatState { off_x: 1.1, off_y: -0.2, vel_x: 0.4, vel_y: 0.1 }; 8],
        )
    }

    /// The whole feature in one test: a blob leaves one process and turns up in
    /// another, over TCP, and the sender is told it landed.
    #[test]
    fn a_throw_crosses_two_real_sockets() {
        let catcher = node(4, &[]);
        let mut thrower = node(4, &[catcher.local_addr]);

        let (along, vel, sats) = a_throw();
        let (throw_id, _peer) =
            thrower.throw(Edge::Right, along, vel, sats.clone()).expect("a peer to throw to");

        let got = wait_for(&catcher, |ev| match ev {
            NetEvent::Arrived { throw, .. } => Some(throw),
            _ => None,
        })
        .expect("the blob arrives");

        assert_eq!(got.throw_id, throw_id);
        assert_eq!(got.edge, Edge::Right, "the edge it left by survives the trip");
        assert!((got.along - along).abs() < 1e-6);
        assert!((got.vel_x - vel.0).abs() < 1e-6 && (got.vel_y - vel.1).abs() < 1e-6);
        assert_eq!(got.sats, sats, "the thrown shape survives the trip");
        assert_eq!(got.from_port, thrower.local_addr.port(), "a return address came with it");

        let landed = wait_for(&thrower, |ev| match ev {
            NetEvent::Landed { throw_id, .. } => Some(throw_id),
            _ => None,
        })
        .expect("the sender is told it landed");
        assert_eq!(landed, throw_id);
    }

    /// Catching a throw teaches the catcher where the thrower lives, so it can
    /// throw back without ever having heard a beacon. This is what makes a game of
    /// catch work with discovery switched off on both ends.
    #[test]
    fn catching_a_blob_teaches_you_where_to_throw_it_back() {
        let catcher = node(4, &[]);
        let mut thrower = node(4, &[catcher.local_addr]);
        assert!(catcher.peers().is_empty(), "the catcher starts knowing nobody");

        let (along, vel, sats) = a_throw();
        thrower.throw(Edge::Right, along, vel, sats).expect("a peer");
        wait_for(&catcher, |ev| match ev {
            NetEvent::Arrived { .. } => Some(()),
            _ => None,
        })
        .expect("the blob arrives");

        let known = catcher.peers();
        assert_eq!(known.len(), 1, "the catcher learned exactly one peer");
        assert_eq!(known[0].addr.port(), thrower.local_addr.port());
    }

    /// A screen that is full says so, and says why, rather than dropping the blob
    /// on the floor.
    #[test]
    fn a_full_screen_refuses_the_throw_and_says_why() {
        let catcher = node(1, &[]);
        catcher.set_resident(1); // its one slot is taken
        let mut thrower = node(4, &[catcher.local_addr]);

        let (along, vel, sats) = a_throw();
        let (id, _) = thrower.throw(Edge::Right, along, vel, sats).expect("a peer");

        let why = wait_for(&thrower, |ev| match ev {
            NetEvent::Refused { throw_id, why, .. } if throw_id == id => Some(why),
            _ => None,
        })
        .expect("a refusal comes back");
        assert_eq!(why, NackReason::Full);
    }

    /// Nobody listening. The sender has to find out promptly, because a blob is
    /// sitting off the edge of its screen waiting to be told.
    #[test]
    fn a_throw_into_the_void_is_reported_lost() {
        // Port 1 is privileged and nothing will be listening on it.
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let mut thrower = node(4, &[dead]);

        let (along, vel, sats) = a_throw();
        let started = Instant::now();
        let (id, _) = thrower.throw(Edge::Left, along, vel, sats).expect("a pinned peer");

        let why = wait_for(&thrower, |ev| match ev {
            NetEvent::Lost { throw_id, why, .. } if throw_id == id => Some(why),
            _ => None,
        })
        .expect("the loss is reported");
        assert!(!why.is_empty(), "a loss has to come with a reason");
        assert!(
            started.elapsed() < CONNECT_TIMEOUT + Duration::from_secs(1),
            "took {:?}; the blob would be stuck off-screen that whole time",
            started.elapsed()
        );
    }

    /// A throw that is somehow delivered twice — a retry, a duplicated frame — must
    /// be acknowledged twice but land only once. Otherwise one blob becomes two.
    #[test]
    fn a_throw_delivered_twice_lands_once() {
        let catcher = node(4, &[]);

        let msg = Throw {
            throw_id: 0xabcd,
            from_node: 1,
            from_port: 9999,
            edge: Edge::Top,
            along: 0.5,
            vel_x: 0.0,
            vel_y: 1.0,
            sats: Vec::new(),
        }
        .encode();

        let mut s = TcpStream::connect(catcher.local_addr).expect("connects");
        s.write_all(&msg).unwrap();
        s.write_all(&msg).unwrap();
        s.flush().unwrap();

        // Both copies are acknowledged...
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 128];
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut acks = 0;
        while acks < 2 && Instant::now() < deadline {
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
            while let Ok((m, used)) = wire::decode(&buf) {
                buf.drain(..used);
                if matches!(m, Message::Ack(0xabcd)) {
                    acks += 1;
                }
            }
        }
        assert_eq!(acks, 2, "both copies should be acknowledged");

        // ...but only one blob is handed to the frame loop.
        std::thread::sleep(Duration::from_millis(200));
        let arrivals = catcher
            .poll()
            .into_iter()
            .filter(|e| matches!(e, NetEvent::Arrived { .. }))
            .count();
        assert_eq!(arrivals, 1, "the duplicate became a second blob");
    }

    /// Nonsense on the socket must not take the node down, and must not be mistaken
    /// for a blob.
    #[test]
    fn garbage_on_the_wire_is_ignored_and_the_node_survives() {
        let catcher = node(4, &[]);

        let mut s = TcpStream::connect(catcher.local_addr).expect("connects");
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        s.flush().unwrap();
        drop(s);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !catcher.poll().iter().any(|e| matches!(e, NetEvent::Arrived { .. })),
            "garbage should never look like a blob"
        );

        // And the node still works afterwards.
        let mut thrower = node(4, &[catcher.local_addr]);
        let (along, vel, sats) = a_throw();
        thrower.throw(Edge::Right, along, vel, sats).expect("a peer");
        assert!(
            wait_for(&catcher, |ev| match ev {
                NetEvent::Arrived { .. } => Some(()),
                _ => None,
            })
            .is_some(),
            "the node stopped working after being fed garbage"
        );
    }

    /// With one peer, every edge leads to it — the two-machines case, which is the
    /// one that should need no configuration at all.
    #[test]
    fn with_a_single_peer_every_edge_is_a_door() {
        let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let n = node(4, &[peer]);
        assert_eq!(n.portal_edges(), 0b1111);
        for e in Edge::ALL {
            assert_eq!(n.peer_for_edge(e).map(|p| p.addr), Some(peer));
        }
    }

    /// With nobody there, nothing is a door and the walls are ordinary walls.
    #[test]
    fn with_no_peers_nothing_is_a_door() {
        let n = node(4, &[]);
        assert_eq!(n.portal_edges(), 0);
        for e in Edge::ALL {
            assert!(n.peer_for_edge(e).is_none());
        }
    }

    /// A pinned peer owns its edge and nothing else.
    #[test]
    fn a_pinned_peer_owns_only_its_own_edge() {
        let left: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let n = Net::start(NetConfig {
            group: "test".into(),
            name: "t".into(),
            capacity: 4,
            discovery: false,
            pinned: vec![(Some(Edge::Left), left)],
        })
        .unwrap();
        assert_eq!(n.peer_for_edge(Edge::Left).map(|p| p.addr), Some(left));
        assert!(n.peer_for_edge(Edge::Right).is_none());
        assert_eq!(n.portal_edges(), Edge::Left.bit());
    }

    #[test]
    fn peer_specs_parse() {
        let (e, a) = parse_peer("right=127.0.0.1:5000").unwrap();
        assert_eq!(e, Some(Edge::Right));
        assert_eq!(a, "127.0.0.1:5000".parse::<SocketAddr>().unwrap());

        let (e, a) = parse_peer("127.0.0.1:5000").unwrap();
        assert_eq!(e, None);
        assert_eq!(a.port(), 5000);

        // A bare host gets the discovery port, which is only useful once a beacon
        // has been heard — documented on `parse_peer`.
        let (_, a) = parse_peer("127.0.0.1").unwrap();
        assert_eq!(a.port(), DISCOVERY_PORT);

        assert!(parse_peer("sideways=127.0.0.1:1").is_err());
        assert!(parse_peer("").is_err());
        assert!(parse_peer("=127.0.0.1:1").is_err());
    }

    /// Two node ids colliding would mean two machines that cannot tell each other
    /// apart, and a beacon each would ignore as its own.
    #[test]
    fn node_ids_do_not_collide() {
        let ids: std::collections::HashSet<u64> = (0..1000).map(|_| random_node_id()).collect();
        assert_eq!(ids.len(), 1000);
    }
}

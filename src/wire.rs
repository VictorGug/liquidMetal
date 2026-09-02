//! The over-the-wire contract for throwing a blob between machines.
//!
//! This module is the *specification*, not an implementation detail. The peer on the
//! other end of a throw is expected to be a different program on a different
//! operating system, so everything here is defined in absolute terms — byte order,
//! field widths, and units — and none of it may drift without a version bump.
//!
//! Like `physics.rs`, this has no SDL / GL / X11 / socket types in it at all: it is
//! pure bytes in, values out, so the whole format is exercised by unit tests.
//!
//! # Framing
//!
//! Every message, on both transports, is one frame:
//!
//! ```text
//! magic    [4]  b"LQMB"
//! version  u16  protocol version; PROTOCOL_VERSION below
//! kind     u16  message kind
//! length   u32  payload length in bytes
//! payload  [length]
//! ```
//!
//! **All integers are little-endian. All floats are IEEE-754 binary32,
//! little-endian.** A frame whose magic or version does not match is dropped
//! without a reply — an old peer shouting on the same port must not be able to
//! provoke a response.
//!
//! # Units, and why they are not pixels
//!
//! The two machines have different screens. Sending raw pixels would mean a throw
//! that drifts gently across a 1080p screen arrives as a bullet on a 5K one, and a
//! blob that left two-thirds of the way up the edge would arrive somewhere else
//! entirely. So the wire carries *proportions*, and each end scales into its own
//! pixels:
//!
//! - **Position along an edge** is `along`, a fraction in `0..=1`.
//! - **Velocity** is in **screen heights per second**, and *both* components are
//!   scaled by the receiver's screen height. Scaling uniformly (rather than x by
//!   width and y by height) is what preserves the angle of the throw: a 45° fling
//!   arrives at 45° even between screens of different aspect ratios.
//! - **Satellite geometry** is in units of the sender's satellite orbit radius, so
//!   the arriving blob keeps the *shape* it was thrown with even if the peer draws
//!   a blob of a different size.
//!
//! The receiver's blob keeps its own local size. What crosses the wire is the
//! gesture, not the geometry.

/// Bumped whenever any layout below changes. Peers ignore frames that disagree.
pub const PROTOCOL_VERSION: u16 = 1;

/// Frame magic, first four bytes of every message on both transports.
pub const MAGIC: [u8; 4] = *b"LQMB";

/// Bytes before the payload: magic(4) + version(2) + kind(2) + length(4).
pub const HEADER_LEN: usize = 12;

/// Refuse to allocate for an absurd `length` field. The largest legitimate frame is
/// a THROW with a few hundred satellites; 64 KiB is far past anything real.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// UDP port the presence beacon is broadcast to. Fixed, because it has to be known
/// before any peer has been discovered. The TCP port is *not* fixed — each node
/// binds an ephemeral one and announces it in its beacon.
pub const DISCOVERY_PORT: u16 = 47811;

// ---------------------------------------------------------------------------

/// Message kinds. Unknown kinds are ignored rather than treated as an error, so a
/// later protocol version can add messages without breaking this one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Beacon = 1,
    Throw = 2,
    Ack = 3,
    Nack = 4,
}

impl Kind {
    pub fn from_u16(v: u16) -> Option<Kind> {
        match v {
            1 => Some(Kind::Beacon),
            2 => Some(Kind::Throw),
            3 => Some(Kind::Ack),
            4 => Some(Kind::Nack),
            _ => None,
        }
    }
}

/// Which edge of a screen a blob crossed.
///
/// Always expressed from the point of view of the machine the blob is *leaving*.
/// The receiver flips it: something that left the sender's right edge enters the
/// receiver's left edge, still travelling in the same direction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Edge {
    Left = 0,
    Right = 1,
    Top = 2,
    Bottom = 3,
}

impl Edge {
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    pub fn from_u8(v: u8) -> Option<Edge> {
        match v {
            0 => Some(Edge::Left),
            1 => Some(Edge::Right),
            2 => Some(Edge::Top),
            3 => Some(Edge::Bottom),
            _ => None,
        }
    }

    /// The edge a blob leaving `self` arrives at on the other screen.
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    /// Index into a 4-bit edge mask.
    pub fn bit(self) -> u8 {
        1 << (self as u8)
    }

    pub fn name(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }
}

/// Why a throw was refused. The sender turns any of these back into a bounce.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NackReason {
    /// The receiver is already holding as many blobs as it will take.
    Full = 0,
    /// The frame did not parse.
    Malformed = 1,
    /// The sender is not in this node's group.
    WrongGroup = 2,
}

impl NackReason {
    pub fn from_u8(v: u8) -> Option<NackReason> {
        match v {
            0 => Some(NackReason::Full),
            1 => Some(NackReason::Malformed),
            2 => Some(NackReason::WrongGroup),
            _ => None,
        }
    }

    pub fn explain(self) -> &'static str {
        match self {
            NackReason::Full => "the other screen is full",
            NackReason::Malformed => "the other end could not read the throw",
            NackReason::WrongGroup => "the other end is in a different group",
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Broadcast once a second so peers can find each other without configuration.
///
/// ```text
/// node_id     u64   random per process, stable for its lifetime
/// tcp_port    u16   where to send a THROW
/// screen_w    u32   logical pixels
/// screen_h    u32   logical pixels; the unit velocity is denominated in
/// blobs       u16   how many blobs are resident right now
/// capacity    u16   how many it will hold in total
/// group_len   u16
/// name_len    u16
/// group       utf8[group_len]
/// name        utf8[name_len]   human-readable, for logs only
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Beacon {
    pub node_id: u64,
    pub tcp_port: u16,
    pub screen_w: u32,
    pub screen_h: u32,
    pub blobs: u16,
    pub capacity: u16,
    pub group: String,
    pub name: String,
}

/// One satellite's state, relative to the core, in units of the sender's orbit
/// radius. This is what makes an arriving blob still look thrown: it turns up
/// stretched and wobbling exactly as it left, rather than as a fresh round ball.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct SatState {
    pub off_x: f32,
    pub off_y: f32,
    pub vel_x: f32,
    pub vel_y: f32,
}

/// A blob leaving one screen for another.
///
/// ```text
/// throw_id    u64   unique per sender; echoed in the ACK/NACK, and used to
///                   discard a duplicate delivery
/// from_node   u64
/// from_port   u16   the sender's own TCP listening port
/// edge        u8    Edge, from the SENDER's point of view
/// pad         u8
/// along       f32   0..=1 along that edge, where the core crossed
/// vel_x       f32   sender screen heights per second
/// vel_y       f32
/// sat_count   u16
/// pad2        u16
/// sats        SatState[sat_count]   16 bytes each
/// ```
///
/// A receiver whose blob has a different satellite count is explicitly allowed to
/// ignore `sats` and start the arriving blob at rest around its core: the throw
/// still lands, it just does not carry the wobble across. That is a graceful
/// degradation, not an error, and it is why `sat_count` is on the wire at all.
#[derive(Clone, Debug, PartialEq)]
pub struct Throw {
    pub throw_id: u64,
    pub from_node: u64,
    /// The sender's own listening port. A throw therefore *teaches* the receiver a
    /// complete return address — combined with the connection's source IP, that is
    /// everything needed to throw back. Discovery is then a convenience for finding
    /// a peer in the first place, not a prerequisite for playing catch.
    pub from_port: u16,
    pub edge: Edge,
    pub along: f32,
    pub vel_x: f32,
    pub vel_y: f32,
    pub sats: Vec<SatState>,
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn put_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_f32(b: &mut Vec<u8>, v: f32) {
    b.extend_from_slice(&v.to_le_bytes());
}

/// Wrap a payload in the standard frame header.
pub fn frame(kind: Kind, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    put_u16(&mut out, PROTOCOL_VERSION);
    put_u16(&mut out, kind as u16);
    put_u32(&mut out, payload.len() as u32);
    out.extend_from_slice(payload);
    out
}

impl Beacon {
    pub fn encode(&self) -> Vec<u8> {
        let g = self.group.as_bytes();
        let n = self.name.as_bytes();
        let mut p = Vec::with_capacity(28 + g.len() + n.len());
        put_u64(&mut p, self.node_id);
        put_u16(&mut p, self.tcp_port);
        put_u32(&mut p, self.screen_w);
        put_u32(&mut p, self.screen_h);
        put_u16(&mut p, self.blobs);
        put_u16(&mut p, self.capacity);
        put_u16(&mut p, g.len() as u16);
        put_u16(&mut p, n.len() as u16);
        p.extend_from_slice(g);
        p.extend_from_slice(n);
        frame(Kind::Beacon, &p)
    }
}

impl Throw {
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(36 + self.sats.len() * 16);
        put_u64(&mut p, self.throw_id);
        put_u64(&mut p, self.from_node);
        put_u16(&mut p, self.from_port);
        p.push(self.edge as u8);
        p.push(0);
        put_f32(&mut p, self.along);
        put_f32(&mut p, self.vel_x);
        put_f32(&mut p, self.vel_y);
        put_u16(&mut p, self.sats.len() as u16);
        put_u16(&mut p, 0);
        for s in &self.sats {
            put_f32(&mut p, s.off_x);
            put_f32(&mut p, s.off_y);
            put_f32(&mut p, s.vel_x);
            put_f32(&mut p, s.vel_y);
        }
        frame(Kind::Throw, &p)
    }
}

/// `ACK` is just the id being acknowledged.
pub fn encode_ack(throw_id: u64) -> Vec<u8> {
    let mut p = Vec::with_capacity(8);
    put_u64(&mut p, throw_id);
    frame(Kind::Ack, &p)
}

/// `NACK` is the id plus one byte saying why, so the sender can tell the user
/// something better than "it did not work".
pub fn encode_nack(throw_id: u64, reason: NackReason) -> Vec<u8> {
    let mut p = Vec::with_capacity(9);
    put_u64(&mut p, throw_id);
    p.push(reason as u8);
    frame(Kind::Nack, &p)
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// A cursor that can only fail, never panic, on a short or malformed buffer.
struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn new(b: &'a [u8]) -> Cursor<'a> {
        Cursor { b, i: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.i.checked_add(n).ok_or(DecodeError::Truncated)?;
        if end > self.b.len() {
            return Err(DecodeError::Truncated);
        }
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, DecodeError> {
        let v = f32::from_le_bytes(self.take(4)?.try_into().unwrap());
        // A NaN or infinity here would poison the receiving simulation forever, and
        // it can only arrive from a buggy or hostile peer. Reject at the boundary.
        if v.is_finite() { Ok(v) } else { Err(DecodeError::NotFinite) }
    }
    fn utf8(&mut self, n: usize) -> Result<String, DecodeError> {
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).map_err(|_| DecodeError::BadUtf8)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer bytes than the format requires. On TCP this means "wait for more".
    Truncated,
    /// Not one of our frames at all.
    BadMagic,
    /// Our magic, a version we do not speak.
    BadVersion(u16),
    /// A kind this version does not know. Ignorable, not fatal.
    UnknownKind(u16),
    /// `length` exceeds `MAX_FRAME_LEN`.
    TooLong(usize),
    BadUtf8,
    BadEnum,
    /// A float that was NaN or infinite.
    NotFinite,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "frame is short"),
            DecodeError::BadMagic => write!(f, "not a liquidMetal frame"),
            DecodeError::BadVersion(v) => {
                write!(f, "protocol version {v}, we speak {PROTOCOL_VERSION}")
            }
            DecodeError::UnknownKind(k) => write!(f, "unknown message kind {k}"),
            DecodeError::TooLong(n) => write!(f, "frame claims {n} bytes"),
            DecodeError::BadUtf8 => write!(f, "a string field was not UTF-8"),
            DecodeError::BadEnum => write!(f, "an enum field was out of range"),
            DecodeError::NotFinite => write!(f, "a float field was NaN or infinite"),
        }
    }
}

/// A parsed frame header plus the payload bounds inside the input buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: u16,
    pub payload: std::ops::Range<usize>,
}

impl Header {
    /// Total bytes this frame occupies.
    pub fn total_len(&self) -> usize {
        self.payload.end
    }
}

/// Parse just the header. Returns `Truncated` when `buf` does not yet hold the whole
/// frame, which on a stream transport means "read more and try again".
pub fn parse_header(buf: &[u8]) -> Result<Header, DecodeError> {
    if buf.len() < HEADER_LEN {
        return Err(DecodeError::Truncated);
    }
    if buf[0..4] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let version = u16::from_le_bytes(buf[4..6].try_into().unwrap());
    if version != PROTOCOL_VERSION {
        return Err(DecodeError::BadVersion(version));
    }
    let kind = u16::from_le_bytes(buf[6..8].try_into().unwrap());
    let len = u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize;
    if len > MAX_FRAME_LEN {
        return Err(DecodeError::TooLong(len));
    }
    if buf.len() < HEADER_LEN + len {
        return Err(DecodeError::Truncated);
    }
    Ok(Header { kind, payload: HEADER_LEN..HEADER_LEN + len })
}

/// Every message this version can carry, already parsed.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Beacon(Beacon),
    Throw(Throw),
    Ack(u64),
    Nack(u64, NackReason),
}

/// Parse one whole frame from the front of `buf`, returning the message and how many
/// bytes it consumed.
pub fn decode(buf: &[u8]) -> Result<(Message, usize), DecodeError> {
    let h = parse_header(buf)?;
    let total = h.total_len();
    let p = &buf[h.payload.clone()];
    let kind = Kind::from_u16(h.kind).ok_or(DecodeError::UnknownKind(h.kind))?;
    let mut c = Cursor::new(p);
    let msg = match kind {
        Kind::Beacon => {
            let node_id = c.u64()?;
            let tcp_port = c.u16()?;
            let screen_w = c.u32()?;
            let screen_h = c.u32()?;
            let blobs = c.u16()?;
            let capacity = c.u16()?;
            let gl = c.u16()? as usize;
            let nl = c.u16()? as usize;
            let group = c.utf8(gl)?;
            let name = c.utf8(nl)?;
            Message::Beacon(Beacon {
                node_id,
                tcp_port,
                screen_w,
                screen_h,
                blobs,
                capacity,
                group,
                name,
            })
        }
        Kind::Throw => {
            let throw_id = c.u64()?;
            let from_node = c.u64()?;
            let from_port = c.u16()?;
            let edge = Edge::from_u8(c.u8()?).ok_or(DecodeError::BadEnum)?;
            c.take(1)?;
            let along = c.f32()?;
            let vel_x = c.f32()?;
            let vel_y = c.f32()?;
            let n = c.u16()? as usize;
            c.u16()?;
            let mut sats = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                sats.push(SatState {
                    off_x: c.f32()?,
                    off_y: c.f32()?,
                    vel_x: c.f32()?,
                    vel_y: c.f32()?,
                });
            }
            Message::Throw(Throw {
                throw_id,
                from_node,
                from_port,
                edge,
                // Clamped rather than rejected: a peer that computes the crossing a
                // hair outside its own edge is not wrong enough to drop a throw for.
                along: along.clamp(0.0, 1.0),
                vel_x,
                vel_y,
                sats,
            })
        }
        Kind::Ack => Message::Ack(c.u64()?),
        Kind::Nack => {
            let id = c.u64()?;
            let r = NackReason::from_u8(c.u8()?).ok_or(DecodeError::BadEnum)?;
            Message::Nack(id, r)
        }
    };
    Ok((msg, total))
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_throw() -> Throw {
        Throw {
            throw_id: 0x0123_4567_89ab_cdef,
            from_node: 0xfedc_ba98_7654_3210,
            from_port: 51234,
            edge: Edge::Right,
            along: 0.375,
            vel_x: 1.75,
            vel_y: -0.25,
            sats: vec![
                SatState { off_x: 1.0, off_y: 0.0, vel_x: -0.5, vel_y: 0.25 },
                SatState { off_x: 0.0, off_y: -1.0, vel_x: 0.125, vel_y: 0.0 },
            ],
        }
    }

    fn sample_beacon() -> Beacon {
        Beacon {
            node_id: 42,
            tcp_port: 51234,
            screen_w: 3840,
            screen_h: 1080,
            blobs: 1,
            capacity: 4,
            group: "default".into(),
            name: "workshop".into(),
        }
    }

    #[test]
    fn throw_round_trips() {
        let t = sample_throw();
        let bytes = t.encode();
        let (msg, used) = decode(&bytes).expect("decodes");
        assert_eq!(used, bytes.len());
        assert_eq!(msg, Message::Throw(t));
    }

    #[test]
    fn beacon_round_trips() {
        let b = sample_beacon();
        let bytes = b.encode();
        let (msg, used) = decode(&bytes).expect("decodes");
        assert_eq!(used, bytes.len());
        assert_eq!(msg, Message::Beacon(b));
    }

    #[test]
    fn ack_and_nack_round_trip() {
        let (m, _) = decode(&encode_ack(9)).unwrap();
        assert_eq!(m, Message::Ack(9));
        let (m, _) = decode(&encode_nack(9, NackReason::Full)).unwrap();
        assert_eq!(m, Message::Nack(9, NackReason::Full));
    }

    /// The layout is a contract with a program someone else writes. If a field width
    /// or an offset moves, this test fails and `PROTOCOL_VERSION` has to move too.
    #[test]
    fn wire_layout_is_pinned() {
        let bytes = sample_throw().encode();
        assert_eq!(&bytes[0..4], b"LQMB", "magic");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 1, "version");
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), Kind::Throw as u16);
        // 8 + 8 + 2 + 1 + 1 + 4 + 4 + 4 + 2 + 2 = 36 bytes fixed, then 16 per sat.
        assert_eq!(bytes.len(), HEADER_LEN + 36 + 2 * 16);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize, 36 + 32);
        assert_eq!(bytes[HEADER_LEN + 18], Edge::Right as u8, "edge byte offset");

        let b = sample_beacon().encode();
        assert_eq!(b.len(), HEADER_LEN + 26 + "default".len() + "workshop".len());
    }

    /// Every truncation of a valid frame must report `Truncated` and never panic —
    /// this is exactly what a TCP read of a partial frame looks like.
    #[test]
    fn every_prefix_is_truncated_not_a_panic() {
        for msg in [sample_throw().encode(), sample_beacon().encode()] {
            for n in 0..msg.len() {
                match decode(&msg[..n]) {
                    Err(DecodeError::Truncated) => {}
                    other => panic!("prefix of {n} bytes gave {other:?}, wanted Truncated"),
                }
            }
            assert!(decode(&msg).is_ok(), "the whole frame still decodes");
        }
    }

    /// Trailing bytes are the *next* frame, not an error: `decode` reports how much
    /// it used so a stream can be drained frame by frame.
    #[test]
    fn decode_reports_its_own_length_so_streams_can_be_drained() {
        let mut buf = sample_throw().encode();
        let first_len = buf.len();
        buf.extend_from_slice(&encode_ack(7));
        let (m1, used) = decode(&buf).unwrap();
        assert_eq!(used, first_len);
        assert!(matches!(m1, Message::Throw(_)));
        let (m2, used2) = decode(&buf[used..]).unwrap();
        assert_eq!(m2, Message::Ack(7));
        assert_eq!(used + used2, buf.len());
    }

    #[test]
    fn a_foreign_frame_is_rejected_by_magic() {
        let mut b = sample_beacon().encode();
        b[0] = b'X';
        assert_eq!(decode(&b), Err(DecodeError::BadMagic));
    }

    #[test]
    fn another_version_is_rejected_without_parsing_the_body() {
        let mut b = sample_beacon().encode();
        b[4] = 99;
        assert_eq!(decode(&b), Err(DecodeError::BadVersion(99)));
    }

    #[test]
    fn an_absurd_length_is_refused_before_allocating() {
        let mut b = sample_beacon().encode();
        b[8..12].copy_from_slice(&(MAX_FRAME_LEN as u32 + 1).to_le_bytes());
        assert_eq!(decode(&b), Err(DecodeError::TooLong(MAX_FRAME_LEN + 1)));
    }

    /// A NaN velocity would spread through the receiving simulation and never leave.
    /// It has to die at the parse boundary.
    #[test]
    fn nan_and_infinity_are_refused() {
        for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut t = sample_throw();
            t.vel_x = poison;
            assert_eq!(decode(&t.encode()), Err(DecodeError::NotFinite), "{poison}");
        }
    }

    #[test]
    fn an_out_of_range_edge_is_refused() {
        let mut b = sample_throw().encode();
        b[HEADER_LEN + 18] = 7;
        assert_eq!(decode(&b), Err(DecodeError::BadEnum));
    }

    #[test]
    fn along_is_clamped_rather_than_rejected() {
        let mut t = sample_throw();
        t.along = 1.0001;
        let (m, _) = decode(&t.encode()).unwrap();
        match m {
            Message::Throw(got) => assert_eq!(got.along, 1.0),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_lying_satellite_count_is_truncation_not_a_huge_allocation() {
        let mut b = sample_throw().encode();
        // sat_count sits at payload offset 32.
        b[HEADER_LEN + 32..HEADER_LEN + 34].copy_from_slice(&60000u16.to_le_bytes());
        assert_eq!(decode(&b), Err(DecodeError::Truncated));
    }

    #[test]
    fn unknown_kinds_are_reported_not_fatal() {
        let f = frame(Kind::Beacon, &[]);
        let mut f2 = f.clone();
        f2[6..8].copy_from_slice(&999u16.to_le_bytes());
        assert_eq!(decode(&f2), Err(DecodeError::UnknownKind(999)));
    }

    #[test]
    fn edges_are_their_own_inverse() {
        for e in Edge::ALL {
            assert_eq!(e.opposite().opposite(), e);
            assert_ne!(e.opposite(), e);
            assert_eq!(Edge::from_u8(e as u8), Some(e));
        }
    }

    /// A zero-satellite throw is legal: it is what a peer that models the blob
    /// differently would send, and it must still land.
    #[test]
    fn a_throw_with_no_satellites_is_valid() {
        let mut t = sample_throw();
        t.sats.clear();
        let (m, _) = decode(&t.encode()).unwrap();
        assert_eq!(m, Message::Throw(t));
    }
}

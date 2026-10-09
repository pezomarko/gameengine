//! gm-net: the wire protocol of gamengine, implementing `docs/PROTOCOL.md` exactly.
//!
//! - [`bits`]: MSB-first bit writer/reader with `uvar`/`svar` (section 2).
//! - [`quant`]: quantization of positions, velocities, angles and move axes (section 2).
//! - [`input`]: input datagrams (section 4).
//! - [`snapshot`]: delta-compressed snapshots (section 5).
//! - [`client`]: prediction, reconciliation and interpolation shared by client and bots (7).
//! - [`control`]: reliable messages with u16 framing, the client's and the zone's (section 8).
//! - [`transport`]: quinn configuration, certificates (section 1).
//! - [`link`]: one connection type over QUIC and WebTransport, for servers (WEB.md 2.1).
//! - [`sim`] (feature `turmoil`): quinn over turmoil's simulated UDP with loss injection.
//!
//! On `wasm32` (the browser client, WEB.md 3) only the codecs and the prediction are built:
//! the transport there is the browser's `WebTransport`.
#![forbid(unsafe_code)]

pub mod bands;
pub mod bits;
pub mod client;
pub mod control;
pub mod input;
pub mod quant;
pub mod snapshot;

#[cfg(not(target_arch = "wasm32"))]
pub mod link;
#[cfg(not(target_arch = "wasm32"))]
pub mod transport;
/// On `wasm32` only the map hash of `transport` exists.
#[cfg(target_arch = "wasm32")]
pub mod transport {
    pub use crate::fnv1a64;
}

#[cfg(all(feature = "turmoil", not(target_arch = "wasm32")))]
pub mod sim;

/// Protocol version byte (PROTOCOL.md header). Bumped on any wire change.
pub const PROTOCOL_VERSION: u8 = 18;

/// Largest datagram payload we ever send (PROTOCOL.md 1): well under the 1,200-byte initial
/// QUIC MTU minus framing, so nothing depends on MTU discovery.
pub const MAX_DATAGRAM_PAYLOAD: usize = 1100;

/// Datagram kinds (PROTOCOL.md 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Input = 0,
    Snapshot = 1,
    Ping = 2,
    Pong = 3,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        match v {
            0 => Some(Kind::Input),
            1 => Some(Kind::Snapshot),
            2 => Some(Kind::Ping),
            3 => Some(Kind::Pong),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NetError {
    #[error("read past the end of the datagram")]
    Overrun,
    #[error("malformed datagram: {0}")]
    Malformed(&'static str),
    #[error("unsupported protocol version {0}")]
    Version(u8),
    #[error("unknown datagram kind {0}")]
    Kind(u8),
    #[error("unknown snapshot baseline tick {0}")]
    UnknownBaseline(u32),
}

/// Write the two-byte datagram header (PROTOCOL.md 3).
pub fn write_header(w: &mut bits::BitWriter, kind: Kind) {
    w.write_bits(PROTOCOL_VERSION as u64, 8);
    w.write_bits(kind as u64, 8);
}

/// Read and validate the datagram header, returning the kind.
pub fn read_header(r: &mut bits::BitReader<'_>) -> Result<Kind, NetError> {
    let version = r.read_bits(8)? as u8;
    if version != PROTOCOL_VERSION {
        return Err(NetError::Version(version));
    }
    let kind = r.read_bits(8)? as u8;
    Kind::from_u8(kind).ok_or(NetError::Kind(kind))
}

/// Peek at the kind of a datagram without decoding it.
pub fn peek_kind(bytes: &[u8]) -> Result<Kind, NetError> {
    let mut r = bits::BitReader::new(bytes);
    read_header(&mut r)
}

/// FNV-1a 64 of a byte string; the map hash in `Welcome` (PROTOCOL.md 8).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

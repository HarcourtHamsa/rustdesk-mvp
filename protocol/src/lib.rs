//! Shared wire protocol for the RustDesk-MVP remote desktop tool.
//!
//! Framing: every message is [u32 big-endian length][bincode-encoded payload].
//! This crate has NO transport-layer encryption. If you need confidentiality
//! (you almost always do, since this carries your screen + keystrokes),
//! tunnel the TCP connection over SSH or a VPN. See the top-level README.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};

/// Default port the host listens on.
pub const DEFAULT_PORT: u16 = 5900;

/// Max frame size we'll accept from the network (32 MB) - prevents a
/// misbehaving/malicious peer from making us allocate unbounded memory.
pub const MAX_MESSAGE_BYTES: u32 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Messages sent from the controlling client -> the host (the machine being controlled).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message on every connection. `password_hash` is sha256(password) as hex.
    Auth { password_hash: String },
    /// Mouse moved to an absolute position, in the host's screen pixel coordinates.
    MouseMove { x: i32, y: i32 },
    MouseButton { button: MouseButton, pressed: bool },
    /// Positive = scroll up, negative = scroll down.
    MouseScroll { delta: i32 },
    /// A key event. `key` is a portable key name (see `keys.rs`), not a raw scancode,
    /// so host and client don't need to agree on OS-specific keycodes.
    Key { key: String, pressed: bool },
    /// Client is asking the host to resend the current screen size (e.g. after a resize).
    RequestScreenInfo,
    /// Toggle the "blank host screen" privacy overlay: while on, anyone
    /// physically at the host machine sees a "Welcome to Onboarding"
    /// screen instead of the real desktop, while this client keeps seeing
    /// and controlling the real thing. Windows-only on the host side.
    ToggleBlankScreen { enabled: bool },
    /// Toggle whether the host applies mouse/keyboard input from this
    /// connection. While blocked, the host keeps streaming frames as
    /// normal - it's a "view only" mode, not a pause of the session -
    /// it just silently drops MouseMove/MouseButton/MouseScroll/Key
    /// until unblocked again. Lets whoever's driving the client keep an
    /// eye on the host without risking an accidental click/keystroke
    /// landing on it (e.g. while just watching, or resting a hand on the
    /// mouse/keyboard).
    BlockGuestInput { blocked: bool },
    /// Toggle whether the host forwards its own physical keystrokes to
    /// this client, for a "what's being typed on the host" log. This is
    /// the reverse direction from - and independent of - the normal `Key`
    /// messages above (those are keys the client sends *to* the host).
    /// Windows-only on the host side, same as `ToggleBlankScreen`.
    ToggleKeystrokeLog { enabled: bool },
    Disconnect,
}

/// Messages sent from the host -> the controlling client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMessage {
    AuthResult {
        ok: bool,
        reason: Option<String>,
        screen_width: u32,
        screen_height: u32,
    },
    /// A JPEG-encoded full frame of the host's screen.
    Frame {
        width: u32,
        height: u32,
        jpeg_bytes: Vec<u8>,
    },
    Error {
        message: String,
    },
    /// Reply to `ClientMessage::ToggleBlankScreen`, so the client's toggle
    /// reflects what actually happened (e.g. unsupported Windows version)
    /// rather than just assuming the request succeeded.
    BlankScreenResult {
        enabled: bool,
        ok: bool,
        reason: Option<String>,
    },
    /// Reply to `ClientMessage::BlockGuestInput`, confirming the new state.
    /// Unlike blank-screen this can't really fail (it's just a flag the
    /// host's input loop checks), but it's still an explicit reply rather
    /// than the client assuming success, so the toggle only flips once the
    /// host has actually applied it.
    GuestInputBlockedResult {
        blocked: bool,
    },
    /// Reply to `ClientMessage::ToggleKeystrokeLog`, confirming the new
    /// state - this genuinely can fail (non-Windows host, or the hook
    /// failing to install), so unlike `GuestInputBlockedResult` it carries
    /// an `ok`/`reason`, same shape as `BlankScreenResult`.
    KeystrokeLogResult {
        enabled: bool,
        ok: bool,
        reason: Option<String>,
    },
    /// One physical keystroke observed on the host while keystroke
    /// logging is enabled. `key` is a portable key name (see `map_key`
    /// mapping the other direction), `timestamp_ms` is milliseconds since
    /// the Unix epoch, set by the host at the moment of the keypress.
    Keystroke {
        key: String,
        timestamp_ms: u64,
    },
}

/// Hash a plaintext password the same way on both sides so the plaintext
/// itself is never what's compared/stored. NOTE: this does not encrypt the
/// connection - see the crate-level docs above.
pub fn hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Write one length-prefixed bincode message to `w`.
pub fn write_message<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let payload = bincode::serialize(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if payload.len() as u64 > MAX_MESSAGE_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "outgoing message too large",
        ));
    }
    let len = payload.len() as u32;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&payload)?;
    w.flush()?;
    Ok(())
}

/// Read one length-prefixed bincode message from `r`.
pub fn read_message<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incoming message too large - dropping connection",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Constant-time-ish comparison for hashed passwords (both are fixed-length
/// hex strings, so this mostly just avoids the trivial early-exit timing leak).
pub fn hashes_match(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------
// Relay / rendezvous protocol.
//
// This is a *separate* message set used only between an endpoint (host or
// client) and the relay server, to find each other without either side
// needing a routable IP or a forwarded port - both sides connect outbound
// to the relay. Once the relay has matched a client to a host, it stops
// parsing anything at all: it just shovels raw bytes between the two
// sockets. Everything that flows after the handshake below is exactly the
// same ClientMessage/HostMessage traffic used in direct mode, untouched.
// ---------------------------------------------------------------------

pub const DEFAULT_RELAY_PORT: u16 = 5910;

/// The first message sent on *every* new connection to the relay. It tells
/// the relay what role this connection plays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelayHello {
    /// Sent by rd-host on the long-lived control connection it keeps open.
    RegisterHost,
    /// Sent by rd-client when it wants to reach a host by its short code.
    ConnectByCode { code: String },
    /// Sent by rd-host on a *new* connection it opens after being told
    /// (via IncomingSession on the control connection) that a client wants in.
    SessionData { session_id: u64 },
}

/// Sent by the relay back to rd-host on the control connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelayToHost {
    Registered { code: String },
    IncomingSession { session_id: u64 },
}

/// Sent by the relay back to rd-client, before it starts piping (success)
/// or instead of piping at all (failure).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelayToClient {
    Connected,
    Error { message: String },
}

/// A short, human-say-able code (no 0/O/1/I to avoid ambiguity when read aloud).
pub fn generate_code() -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..6)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

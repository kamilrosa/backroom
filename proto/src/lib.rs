//! Protocol shared by the Backroom client and server.
//!
//! Transport: QUIC (one connection per client).
//! - Control: a single bidirectional stream opened by the client carrying
//!   `\n`-terminated JSON messages (easy to read in logs).
//! - Voice: QUIC datagrams (no reliability needed — a late voice packet is
//!   useless, and Opus FEC and PLC conceal losses).
//!
//! Compatibility: new fields always have `#[serde(default)]` (older versions
//! skip fields they do not know), and new message types are only sent to a
//! peer that announced the matching feature in `Hello` / `Welcome`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Protocol identifier in ALPN — client and server must agree.
pub const ALPN: &[u8] = b"backroom/1";
/// Bumped on incompatible protocol changes.
pub const PROTOCOL_VERSION: u32 = 1;
/// Name in the server certificate. The client checks the certificate
/// fingerprint (TOFU), not the host name, so a constant is enough.
pub const SERVER_NAME: &str = "backroom";
pub const DEFAULT_PORT: u16 = 7777;

pub const SAMPLE_RATE: u32 = 48_000;
/// Opus frame: 20 ms at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
pub const MAX_NAME_LEN: usize = 32;
/// Chat message length limit, in characters.
pub const MAX_CHAT_LEN: usize = 500;
/// How many recent chat messages the server keeps for people who join later.
pub const CHAT_HISTORY: usize = 50;

/// Feature: text chat (`ClientMsg::Chat`, `ServerMsg::Chat`).
pub const FEATURE_CHAT: &str = "chat";

/// Connection close code: someone logged in with the same nickname.
/// The client does not reconnect after it (otherwise two people with the
/// same nickname would keep kicking each other out).
pub const CLOSE_REPLACED: u32 = 2;

pub type UserId = u16;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserInfo {
    pub id: UserId,
    pub name: String,
    pub muted: bool,
    pub deafened: bool,
    /// "Be right back".
    #[serde(default)]
    pub away: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatLine {
    pub name: String,
    /// Server time, Unix seconds.
    pub time: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        version: u32,
        name: String,
        password: String,
        /// Optional features this client understands (`FEATURE_*`).
        #[serde(default)]
        features: Vec<String>,
    },
    State {
        muted: bool,
        deafened: bool,
        #[serde(default)]
        away: bool,
    },
    Chat {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome {
        your_id: UserId,
        server_name: String,
        users: Vec<UserInfo>,
        /// Optional features this server supports (`FEATURE_*`).
        #[serde(default)]
        features: Vec<String>,
    },
    Reject {
        reason: String,
    },
    Joined {
        user: UserInfo,
    },
    Left {
        id: UserId,
    },
    State {
        id: UserId,
        muted: bool,
        deafened: bool,
        #[serde(default)]
        away: bool,
    },
    /// Only sent to clients with `FEATURE_CHAT`. Right after `Welcome` the
    /// server replays its recent messages with `history: true`.
    Chat {
        line: ChatLine,
        #[serde(default)]
        history: bool,
    },
    /// Each user's ping (QUIC RTT) to the server, in ms. Sent every ~2 s.
    Pings {
        pings: Vec<(UserId, u32)>,
    },
}

/// Server certificate fingerprint (SHA-256, `AB:CD:…`). The server prints it
/// and the client remembers it (TOFU), so both must use this one format.
pub fn fingerprint(cert_der: &[u8]) -> String {
    Sha256::digest(cert_der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// Human-readable rejection reason when versions differ.
pub fn version_mismatch_reason(server: u32, client: u32) -> String {
    format!("Incompatible version (server: {server}, you: {client}). Please update Backroom.")
}

pub fn encode_line<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut v = serde_json::to_vec(msg).expect("message serialization");
    v.push(b'\n');
    v
}

pub fn decode_line<'a, T: Deserialize<'a>>(line: &'a [u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(line)
}

// ---------------------------------------------------------------------------
// Voice packets
// ---------------------------------------------------------------------------

/// First packet of an utterance — the receiver resets its buffer and decoder.
pub const FLAG_START: u8 = 0x01;
/// Last packet of an utterance — the receiver stops playback without
/// concealment (otherwise PLC would "drag out" the end of the voice).
pub const FLAG_END: u8 = 0x02;

const KIND_VOICE: u8 = 1;
/// client → server: [kind][flags][seq u32 BE][opus…]
pub const UP_HEADER: usize = 6;
/// server → client: [kind][flags][sender u16 BE][seq u32 BE][opus…]
pub const DOWN_HEADER: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceUp<'a> {
    pub flags: u8,
    pub seq: u32,
    pub opus: &'a [u8],
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceDown<'a> {
    pub flags: u8,
    pub sender: UserId,
    pub seq: u32,
    pub opus: &'a [u8],
}

pub fn encode_voice_up(flags: u8, seq: u32, opus: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(UP_HEADER + opus.len());
    v.push(KIND_VOICE);
    v.push(flags);
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(opus);
    v
}

pub fn decode_voice_up(b: &[u8]) -> Option<VoiceUp<'_>> {
    if b.len() <= UP_HEADER || b[0] != KIND_VOICE {
        return None;
    }
    Some(VoiceUp {
        flags: b[1],
        seq: u32::from_be_bytes([b[2], b[3], b[4], b[5]]),
        opus: &b[UP_HEADER..],
    })
}

/// The server rewrites a client packet, adding the sender id.
pub fn up_to_down(sender: UserId, up: &[u8]) -> Option<Vec<u8>> {
    let p = decode_voice_up(up)?;
    let mut v = Vec::with_capacity(DOWN_HEADER + p.opus.len());
    v.push(KIND_VOICE);
    v.push(p.flags);
    v.extend_from_slice(&sender.to_be_bytes());
    v.extend_from_slice(&p.seq.to_be_bytes());
    v.extend_from_slice(p.opus);
    Some(v)
}

pub fn decode_voice_down(b: &[u8]) -> Option<VoiceDown<'_>> {
    if b.len() <= DOWN_HEADER || b[0] != KIND_VOICE {
        return None;
    }
    Some(VoiceDown {
        flags: b[1],
        sender: u16::from_be_bytes([b[2], b[3]]),
        seq: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        opus: &b[DOWN_HEADER..],
    })
}

/// Nickname normalization: trimmed, no control characters, length limit.
pub fn sanitize_name(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_LEN)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Chat text normalization: a single line, no control characters, length limit.
pub fn sanitize_chat(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_CHAT_LEN)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_roundtrip() {
        let up = encode_voice_up(FLAG_START, 0xDEADBEEF, &[1, 2, 3]);
        let p = decode_voice_up(&up).unwrap();
        assert_eq!(p, VoiceUp { flags: FLAG_START, seq: 0xDEADBEEF, opus: &[1, 2, 3] });
        let down = up_to_down(42, &up).unwrap();
        let d = decode_voice_down(&down).unwrap();
        assert_eq!(d, VoiceDown { flags: FLAG_START, sender: 42, seq: 0xDEADBEEF, opus: &[1, 2, 3] });
    }

    #[test]
    fn voice_rejects_garbage() {
        assert!(decode_voice_up(&[]).is_none());
        assert!(decode_voice_up(&[1, 0, 0, 0, 0, 1]).is_none()); // empty opus payload
        assert!(decode_voice_up(&[9, 0, 0, 0, 0, 1, 5]).is_none());
        assert!(decode_voice_down(&[1, 0, 0, 1, 0, 0, 0]).is_none());
    }

    #[test]
    fn control_json() {
        let m = ClientMsg::Hello { version: 1, name: "Alice".into(), password: "x".into(), features: vec![FEATURE_CHAT.into()] };
        let line = encode_line(&m);
        assert_eq!(*line.last().unwrap(), b'\n');
        let back: ClientMsg = decode_line(&line[..line.len() - 1]).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn older_versions_still_decode() {
        // 0.1.x messages have no `features` / `away` fields.
        let hello: ClientMsg = decode_line(br#"{"t":"hello","version":1,"name":"A","password":"x"}"#).unwrap();
        assert!(matches!(hello, ClientMsg::Hello { features, .. } if features.is_empty()));
        let st: ServerMsg = decode_line(br#"{"t":"state","id":3,"muted":true,"deafened":false}"#).unwrap();
        assert_eq!(st, ServerMsg::State { id: 3, muted: true, deafened: false, away: false });
        let u: UserInfo = serde_json::from_str(r#"{"id":1,"name":"A","muted":false,"deafened":false}"#).unwrap();
        assert!(!u.away);
    }

    #[test]
    fn chat_text() {
        assert_eq!(sanitize_chat("  brb\nin 5\tmin \u{7} "), "brb in 5 min");
        assert_eq!(sanitize_chat(" \n "), "");
        assert_eq!(sanitize_chat(&"ż".repeat(600)).chars().count(), MAX_CHAT_LEN);
    }

    #[test]
    fn fingerprint_format() {
        // SHA-256 of an empty input.
        let fp = fingerprint(b"");
        assert!(fp.starts_with("E3:B0:C4:42:98:FC:1C:14:"), "{fp}");
        assert_eq!(fp.len(), 32 * 3 - 1);
    }

    #[test]
    fn names() {
        assert_eq!(sanitize_name("  Al\u{7}ice  "), "Alice");
        assert_eq!(sanitize_name(&"x".repeat(100)).len(), MAX_NAME_LEN);
    }
}

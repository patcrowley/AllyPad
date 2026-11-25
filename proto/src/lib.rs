//! AllyPad wire protocol.
//!
//! Everything is state-based so a lost UDP packet never loses an input:
//! buttons and keys are sent as "currently held", mouse and wheel motion as
//! running totals. All integers are little endian.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const PORT: u16 = 47810;
pub const MAGIC: [u8; 2] = *b"AP";
pub const VERSION: u8 = 1;
pub const MAX_KEYS: usize = 8;
pub const MAC_LEN: usize = 8;

pub mod kind {
    pub const DISCOVER: u8 = 1;
    pub const ANNOUNCE: u8 = 2;
    pub const HELLO: u8 = 3;
    pub const CHALLENGE: u8 = 4;
    pub const STATE: u8 = 5;
    pub const FEEDBACK: u8 = 6;
    pub const REJECT: u8 = 7;
    pub const BYE: u8 = 8;
    pub const INFO: u8 = 9;
}

/// XInput button bits (same layout as XINPUT_GAMEPAD.wButtons).
pub mod btn {
    pub const UP: u16 = 0x0001;
    pub const DOWN: u16 = 0x0002;
    pub const LEFT: u16 = 0x0004;
    pub const RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LS: u16 = 0x0040;
    pub const RS: u16 = 0x0080;
    pub const LB: u16 = 0x0100;
    pub const RB: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// Feedback flag: the laptop has a working virtual controller driver.
pub const FB_PAD_OK: u8 = 1;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct PadState {
    pub buttons: u16,
    pub lt: u8,
    pub rt: u8,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct State {
    pub seq: u32,
    pub pad: PadState,
    /// Running totals; the receiver applies the difference.
    pub mouse_x: i32,
    pub mouse_y: i32,
    /// Running totals in 1/120-notch units.
    pub wheel: i32,
    pub hwheel: i32,
    /// bit0 left, bit1 right, bit2 middle
    pub mouse_buttons: u8,
    /// Held virtual-key codes, 0 = empty slot.
    pub keys: [u16; MAX_KEYS],
    /// Sender clock in milliseconds, echoed back for latency measurement.
    pub t_ms: u32,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Feedback {
    pub seq: u32,
    pub rumble_large: u8,
    pub rumble_small: u8,
    pub echo_t_ms: u32,
    pub hold_ms: u16,
    pub flags: u8,
}

pub type Key = [u8; 32];

pub fn pin_key(pin: &str) -> Key {
    let mut h = Sha256::new();
    h.update(b"AllyPad-v1:");
    h.update(pin.trim().as_bytes());
    h.finalize().into()
}

pub fn session_key(pin_key: &Key, client_nonce: u64, server_nonce: u64) -> Key {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(pin_key).expect("hmac key");
    m.update(b"session");
    m.update(&client_nonce.to_le_bytes());
    m.update(&server_nonce.to_le_bytes());
    m.finalize().into_bytes().into()
}

fn mac(key: &Key, data: &[u8]) -> [u8; MAC_LEN] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    m.update(data);
    let full = m.finalize().into_bytes();
    let mut out = [0u8; MAC_LEN];
    out.copy_from_slice(&full[..MAC_LEN]);
    out
}

fn seal(key: &Key, mut body: Vec<u8>) -> Vec<u8> {
    let tag = mac(key, &body);
    body.extend_from_slice(&tag);
    body
}

/// Returns the body without the tag if the tag is valid.
fn open<'a>(key: &Key, buf: &'a [u8]) -> Option<&'a [u8]> {
    if buf.len() < MAC_LEN + 4 {
        return None;
    }
    let (body, tag) = buf.split_at(buf.len() - MAC_LEN);
    let want = mac(key, body);
    // constant-time compare
    let mut diff = 0u8;
    for i in 0..MAC_LEN {
        diff |= want[i] ^ tag[i];
    }
    if diff == 0 {
        Some(body)
    } else {
        None
    }
}

pub fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::getrandom(&mut b).expect("os random");
    u64::from_le_bytes(b)
}

fn header(kind: u8) -> Vec<u8> {
    vec![MAGIC[0], MAGIC[1], VERSION, kind]
}

/// Packet kind if the buffer carries a valid AllyPad header.
pub fn peek_kind(buf: &[u8]) -> Option<u8> {
    if buf.len() >= 4 && buf[0] == MAGIC[0] && buf[1] == MAGIC[1] && buf[2] == VERSION {
        Some(buf[3])
    } else {
        None
    }
}

struct Rd<'a>(&'a [u8]);
impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn i16(&mut self) -> Option<i16> {
        self.u16().map(|v| v as i16)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| {
            let mut a = [0u8; 8];
            a.copy_from_slice(b);
            u64::from_le_bytes(a)
        })
    }
}

// ---- discovery -----------------------------------------------------------

pub fn encode_discover(nonce: u32) -> Vec<u8> {
    let mut v = header(kind::DISCOVER);
    v.extend_from_slice(&nonce.to_le_bytes());
    v
}

pub fn decode_discover(buf: &[u8]) -> Option<u32> {
    let mut r = Rd(buf.get(4..)?);
    r.u32()
}

pub fn encode_announce(nonce: u32, name: &str) -> Vec<u8> {
    let mut v = header(kind::ANNOUNCE);
    v.extend_from_slice(&nonce.to_le_bytes());
    let name = name.as_bytes();
    let n = name.len().min(60);
    v.push(n as u8);
    v.extend_from_slice(&name[..n]);
    v
}

pub fn decode_announce(buf: &[u8]) -> Option<(u32, String)> {
    let mut r = Rd(buf.get(4..)?);
    let nonce = r.u32()?;
    let n = r.u8()? as usize;
    let name = String::from_utf8_lossy(r.take(n)?).into_owned();
    Some((nonce, name))
}

// ---- handshake -----------------------------------------------------------

pub fn encode_hello(client_nonce: u64) -> Vec<u8> {
    let mut v = header(kind::HELLO);
    v.extend_from_slice(&client_nonce.to_le_bytes());
    v
}

pub fn decode_hello(buf: &[u8]) -> Option<u64> {
    Rd(buf.get(4..)?).u64()
}

pub fn encode_challenge(client_nonce: u64, server_nonce: u64) -> Vec<u8> {
    let mut v = header(kind::CHALLENGE);
    v.extend_from_slice(&client_nonce.to_le_bytes());
    v.extend_from_slice(&server_nonce.to_le_bytes());
    v
}

pub fn decode_challenge(buf: &[u8]) -> Option<(u64, u64)> {
    let mut r = Rd(buf.get(4..)?);
    Some((r.u64()?, r.u64()?))
}

pub const REJECT_PIN: u8 = 0;
pub const REJECT_FULL: u8 = 1;

pub fn encode_reject(reason: u8) -> Vec<u8> {
    let mut v = header(kind::REJECT);
    v.push(reason);
    v
}

pub fn decode_reject(buf: &[u8]) -> u8 {
    buf.get(4).copied().unwrap_or(REJECT_PIN)
}

/// What the laptop tells the Ally about itself, about once a second.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Info {
    /// 0..=100, or 255 when unknown.
    pub volume: u8,
    pub muted: bool,
    /// 0..=100, or 255 when there is no battery.
    pub battery: u8,
    pub charging: bool,
    /// Which player this Ally is on the laptop, 1..=4.
    pub player: u8,
    /// Program in the foreground on the laptop, empty when it is nothing of note.
    pub app: String,
}

pub fn encode_info(key: &Key, seq: u32, i: &Info) -> Vec<u8> {
    let mut v = header(kind::INFO);
    v.extend_from_slice(&seq.to_le_bytes());
    v.push(i.volume);
    v.push(i.muted as u8 | (i.charging as u8) << 1);
    v.push(i.battery);
    v.push(i.player);
    let mut end = i.app.len().min(48);
    while !i.app.is_char_boundary(end) {
        end -= 1;
    }
    v.push(end as u8);
    v.extend_from_slice(&i.app.as_bytes()[..end]);
    seal(key, v)
}

pub fn decode_info(key: &Key, buf: &[u8]) -> Option<(u32, Info)> {
    let body = open(key, buf)?;
    let mut r = Rd(body.get(4..)?);
    let seq = r.u32()?;
    let volume = r.u8()?;
    let flags = r.u8()?;
    let battery = r.u8()?;
    let player = r.u8()?;
    let n = r.u8()? as usize;
    let app = String::from_utf8_lossy(r.take(n)?).into_owned();
    Some((seq, Info { volume, muted: flags & 1 != 0, charging: flags & 2 != 0, battery, player, app }))
}

// ---- session packets -----------------------------------------------------

pub fn encode_state(key: &Key, s: &State) -> Vec<u8> {
    let mut v = header(kind::STATE);
    v.extend_from_slice(&s.seq.to_le_bytes());
    v.extend_from_slice(&s.pad.buttons.to_le_bytes());
    v.push(s.pad.lt);
    v.push(s.pad.rt);
    for a in [s.pad.lx, s.pad.ly, s.pad.rx, s.pad.ry] {
        v.extend_from_slice(&a.to_le_bytes());
    }
    for a in [s.mouse_x, s.mouse_y, s.wheel, s.hwheel] {
        v.extend_from_slice(&a.to_le_bytes());
    }
    v.push(s.mouse_buttons);
    for k in s.keys {
        v.extend_from_slice(&k.to_le_bytes());
    }
    v.extend_from_slice(&s.t_ms.to_le_bytes());
    seal(key, v)
}

pub fn decode_state(key: &Key, buf: &[u8]) -> Option<State> {
    let body = open(key, buf)?;
    let mut r = Rd(body.get(4..)?);
    let mut s = State { seq: r.u32()?, ..Default::default() };
    s.pad.buttons = r.u16()?;
    s.pad.lt = r.u8()?;
    s.pad.rt = r.u8()?;
    s.pad.lx = r.i16()?;
    s.pad.ly = r.i16()?;
    s.pad.rx = r.i16()?;
    s.pad.ry = r.i16()?;
    s.mouse_x = r.i32()?;
    s.mouse_y = r.i32()?;
    s.wheel = r.i32()?;
    s.hwheel = r.i32()?;
    s.mouse_buttons = r.u8()?;
    for k in s.keys.iter_mut() {
        *k = r.u16()?;
    }
    s.t_ms = r.u32()?;
    Some(s)
}

pub fn encode_feedback(key: &Key, f: &Feedback) -> Vec<u8> {
    let mut v = header(kind::FEEDBACK);
    v.extend_from_slice(&f.seq.to_le_bytes());
    v.push(f.rumble_large);
    v.push(f.rumble_small);
    v.extend_from_slice(&f.echo_t_ms.to_le_bytes());
    v.extend_from_slice(&f.hold_ms.to_le_bytes());
    v.push(f.flags);
    seal(key, v)
}

pub fn decode_feedback(key: &Key, buf: &[u8]) -> Option<Feedback> {
    let body = open(key, buf)?;
    let mut r = Rd(body.get(4..)?);
    Some(Feedback {
        seq: r.u32()?,
        rumble_large: r.u8()?,
        rumble_small: r.u8()?,
        echo_t_ms: r.u32()?,
        hold_ms: r.u16()?,
        flags: r.u8()?,
    })
}

pub fn encode_bye(key: &Key, seq: u32) -> Vec<u8> {
    let mut v = header(kind::BYE);
    v.extend_from_slice(&seq.to_le_bytes());
    seal(key, v)
}

pub fn decode_bye(key: &Key, buf: &[u8]) -> Option<u32> {
    let body = open(key, buf)?;
    Rd(body.get(4..)?).u32()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip_and_tamper() {
        let k = session_key(&pin_key("123456"), 1, 2);
        let mut s = State { seq: 9, mouse_x: -40, wheel: 240, t_ms: 77, ..Default::default() };
        s.pad.buttons = btn::A | btn::LB;
        s.pad.lx = -32768;
        s.keys[0] = 0x1B;
        let mut p = encode_state(&k, &s);
        assert_eq!(decode_state(&k, &p), Some(s));
        let wrong = session_key(&pin_key("123457"), 1, 2);
        assert_eq!(decode_state(&wrong, &p), None);
        p[10] ^= 1;
        assert_eq!(decode_state(&k, &p), None);
    }

    #[test]
    fn feedback_roundtrip() {
        let k = session_key(&pin_key("1"), 5, 6);
        let f = Feedback { seq: 3, rumble_large: 200, rumble_small: 9, echo_t_ms: 1234, hold_ms: 7, flags: FB_PAD_OK };
        assert_eq!(decode_feedback(&k, &encode_feedback(&k, &f)), Some(f));
    }

    #[test]
    fn info_roundtrip() {
        let k = session_key(&pin_key("1"), 5, 6);
        let i = Info { volume: 42, muted: true, battery: 80, charging: true, player: 2, app: "Spider-Man2".into() };
        assert_eq!(decode_info(&k, &encode_info(&k, 7, &i)), Some((7, i)));
        assert_eq!(decode_reject(&encode_reject(REJECT_FULL)), REJECT_FULL);
    }

    #[test]
    fn discovery_roundtrip() {
        assert_eq!(decode_discover(&encode_discover(42)), Some(42));
        assert_eq!(decode_announce(&encode_announce(42, "LAPTOP")), Some((42, "LAPTOP".into())));
        assert_eq!(peek_kind(&encode_hello(1)), Some(kind::HELLO));
        assert_eq!(decode_challenge(&encode_challenge(7, 8)), Some((7, 8)));
    }
}

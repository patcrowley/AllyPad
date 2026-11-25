//! Platform-independent receiver logic: handshake, replay protection and
//! turning state packets into input events on a `Sink`. Up to four Allys
//! can be connected at once, each with its own virtual controller.

use allypad_proto as proto;
use proto::{kind, Feedback, Info, Key, PadState, State};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub const MAX_PLAYERS: usize = 4;

pub trait Sink {
    /// Update player `slot`'s virtual controller, plugging it in if needed.
    fn pad(&mut self, slot: usize, pad: &PadState);
    /// Unplug player `slot`'s virtual controller.
    fn pad_release(&mut self, slot: usize);
    fn mouse_move(&mut self, dx: i32, dy: i32);
    fn wheel(&mut self, v: i32, h: i32);
    fn mouse_button(&mut self, bit: u8, down: bool);
    fn key(&mut self, vk: u16, down: bool);
    /// Latest rumble request from games for player `slot` (large, small).
    fn rumble(&mut self, slot: usize) -> (u8, u8);
    fn pad_ok(&self) -> bool;
    fn keep_awake(&mut self, on: bool);
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Connected(SocketAddr, usize),
    Idle(usize),
    Resumed(usize),
    Disconnected(usize),
    Reconnected(usize),
    WrongPin(SocketAddr),
    Full(SocketAddr),
    LockedOut,
}

struct Pending {
    cn: u64,
    sn: u64,
    addr: SocketAddr,
    at: Instant,
}

struct Session {
    slot: usize,
    key: Key,
    addr: SocketAddr,
    last_seq: u32,
    st: State,
    last_rx: Instant,
    idle: bool,
    last_fb: Instant,
    last_rumble: (u8, u8),
    fb_seq: u32,
}

pub struct Peer {
    pub slot: usize,
    pub addr: SocketAddr,
    pub idle: bool,
    pub pad: PadState,
}

pub struct Core {
    pin_key: Key,
    name: String,
    pending: VecDeque<Pending>,
    sessions: Vec<Session>,
    bad: u32,
    bad_since: Instant,
    locked_until: Option<Instant>,
    info_seq: u32,
    pub events: Vec<Event>,
    pub packets: u64,
}

const IDLE_AFTER: Duration = Duration::from_millis(400);
const DROP_AFTER: Duration = Duration::from_secs(20);
const FEEDBACK_EVERY: Duration = Duration::from_millis(100);
const MAX_STEP: i32 = 4000;

pub type Out = Vec<(Vec<u8>, SocketAddr)>;

impl Core {
    pub fn new(pin: &str, name: &str, now: Instant) -> Core {
        Core {
            pin_key: proto::pin_key(pin),
            name: name.to_string(),
            pending: VecDeque::new(),
            sessions: Vec::new(),
            bad: 0,
            bad_since: now,
            locked_until: None,
            info_seq: 0,
            events: Vec::new(),
            packets: 0,
        }
    }

    #[allow(dead_code)]
    pub fn connected(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Everyone connected, in player order.
    pub fn peers(&self) -> Vec<Peer> {
        let mut v: Vec<Peer> =
            self.sessions.iter().map(|s| Peer { slot: s.slot, addr: s.addr, idle: s.idle, pad: s.st.pad }).collect();
        v.sort_by_key(|p| p.slot);
        v
    }

    /// Switch to a new PIN; everyone connected has to pair again.
    pub fn set_pin(&mut self, pin: &str, sink: &mut dyn Sink) {
        self.pin_key = proto::pin_key(pin);
        self.pending.clear();
        self.shutdown(sink);
    }

    /// Let go of everything and unplug all virtual controllers.
    pub fn shutdown(&mut self, sink: &mut dyn Sink) {
        while !self.sessions.is_empty() {
            self.end_session(0, sink);
        }
    }

    /// Queue a status packet for every connected Ally.
    pub fn send_info(&mut self, info: &Info, out: &mut Out) {
        self.info_seq = self.info_seq.wrapping_add(1);
        for s in &self.sessions {
            let mine = Info { player: s.slot as u8 + 1, ..info.clone() };
            out.push((proto::encode_info(&s.key, self.info_seq, &mine), s.addr));
        }
    }

    fn locked(&mut self, now: Instant) -> bool {
        match self.locked_until {
            Some(t) if now < t => true,
            Some(_) => {
                self.locked_until = None;
                self.bad = 0;
                false
            }
            None => false,
        }
    }

    fn count_bad(&mut self, now: Instant) {
        if now.duration_since(self.bad_since) > Duration::from_secs(60) {
            self.bad = 0;
            self.bad_since = now;
        }
        self.bad += 1;
        if self.bad >= 8 {
            self.locked_until = Some(now + Duration::from_secs(60));
            self.pending.clear();
            self.events.push(Event::LockedOut);
        }
    }

    pub fn handle(&mut self, buf: &[u8], from: SocketAddr, now: Instant, sink: &mut dyn Sink, out: &mut Out) {
        let Some(k) = proto::peek_kind(buf) else { return };
        match k {
            kind::DISCOVER => {
                if let Some(nonce) = proto::decode_discover(buf) {
                    out.push((proto::encode_announce(nonce, &self.name), from));
                }
            }
            kind::HELLO => {
                if self.locked(now) {
                    return;
                }
                let Some(cn) = proto::decode_hello(buf) else { return };
                self.pending.retain(|p| now.duration_since(p.at) < Duration::from_secs(10));
                // A repeated HELLO gets the same challenge back.
                let sn = match self.pending.iter().find(|p| p.addr == from && p.cn == cn) {
                    Some(p) => p.sn,
                    None => {
                        let sn = proto::random_u64();
                        if self.pending.len() >= 16 {
                            self.pending.pop_front();
                        }
                        self.pending.push_back(Pending { cn, sn, addr: from, at: now });
                        sn
                    }
                };
                out.push((proto::encode_challenge(cn, sn), from));
            }
            kind::STATE => self.on_state(buf, from, now, sink, out),
            kind::BYE => {
                if let Some(i) = self.sessions.iter().position(|s| s.addr == from) {
                    if proto::decode_bye(&self.sessions[i].key, buf).is_some() {
                        self.end_session(i, sink);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_state(&mut self, buf: &[u8], from: SocketAddr, now: Instant, sink: &mut dyn Sink, out: &mut Out) {
        // Existing session?
        if let Some(sess) = self.sessions.iter_mut().find(|s| s.addr == from) {
            if let Some(s) = proto::decode_state(&sess.key, buf) {
                if (s.seq.wrapping_sub(sess.last_seq) as i32) > 0 {
                    self.packets += 1;
                    if sess.idle {
                        self.events.push(Event::Resumed(sess.slot));
                    }
                    apply(sess, &s, now, sink, false);
                }
                return;
            }
        }
        if self.locked(now) {
            return;
        }
        // A fresh handshake from this address?
        let mut tried = false;
        let mut found: Option<(Key, State)> = None;
        for p in self.pending.iter().filter(|p| p.addr == from) {
            tried = true;
            let key = proto::session_key(&self.pin_key, p.cn, p.sn);
            if let Some(s) = proto::decode_state(&key, buf) {
                found = Some((key, s));
                break;
            }
        }
        let Some((key, s)) = found else {
            if tried {
                self.pending.retain(|p| p.addr != from);
                out.push((proto::encode_reject(proto::REJECT_PIN), from));
                self.events.push(Event::WrongPin(from));
                self.count_bad(now);
            }
            return;
        };
        self.pending.retain(|p| p.addr != from);

        // The same Ally reconnecting keeps its player number and its virtual
        // controller, so games do not see it unplugged and plugged back in.
        let again = self.sessions.iter().position(|x| x.addr.ip() == from.ip());
        let slot = match again {
            Some(i) => {
                let mut old = self.sessions.remove(i);
                release(&mut old, sink);
                old.slot
            }
            None => match (0..MAX_PLAYERS).find(|n| !self.sessions.iter().any(|x| x.slot == *n)) {
                Some(n) => n,
                None => {
                    out.push((proto::encode_reject(proto::REJECT_FULL), from));
                    self.events.push(Event::Full(from));
                    return;
                }
            },
        };
        let mut sess = Session {
            slot,
            key,
            addr: from,
            last_seq: s.seq,
            st: s,
            last_rx: now,
            idle: false,
            last_fb: now - FEEDBACK_EVERY,
            last_rumble: (0, 0),
            fb_seq: 0,
        };
        apply(&mut sess, &s, now, sink, true);
        self.sessions.push(sess);
        self.packets += 1;
        self.bad = 0;
        sink.keep_awake(true);
        self.events.push(if again.is_some() { Event::Reconnected(slot) } else { Event::Connected(from, slot) });
    }

    fn end_session(&mut self, index: usize, sink: &mut dyn Sink) {
        let mut sess = self.sessions.remove(index);
        release(&mut sess, sink);
        sink.pad_release(sess.slot);
        if self.sessions.is_empty() {
            sink.keep_awake(false);
        }
        self.events.push(Event::Disconnected(sess.slot));
    }

    /// Call often (every few ms): handles timeouts and sends feedback.
    pub fn tick(&mut self, now: Instant, sink: &mut dyn Sink, out: &mut Out) {
        let mut i = 0;
        while i < self.sessions.len() {
            let quiet = now.duration_since(self.sessions[i].last_rx);
            if quiet > DROP_AFTER {
                self.end_session(i, sink);
                continue;
            }
            let sess = &mut self.sessions[i];
            if quiet > IDLE_AFTER {
                if !sess.idle {
                    release(sess, sink);
                    sess.idle = true;
                    self.events.push(Event::Idle(sess.slot));
                }
            } else {
                let rumble = sink.rumble(sess.slot);
                if rumble != sess.last_rumble || now.duration_since(sess.last_fb) >= FEEDBACK_EVERY {
                    sess.fb_seq = sess.fb_seq.wrapping_add(1);
                    let f = Feedback {
                        seq: sess.fb_seq,
                        rumble_large: rumble.0,
                        rumble_small: rumble.1,
                        echo_t_ms: sess.st.t_ms,
                        hold_ms: quiet.as_millis().min(60000) as u16,
                        flags: if sink.pad_ok() { proto::FB_PAD_OK } else { 0 },
                    };
                    out.push((proto::encode_feedback(&sess.key, &f), sess.addr));
                    sess.last_fb = now;
                    sess.last_rumble = rumble;
                }
            }
            i += 1;
        }
    }
}

/// Shift, Ctrl, Alt and Windows keys.
fn is_modifier(vk: u16) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

fn step(new: i32, old: i32) -> i32 {
    new.wrapping_sub(old).clamp(-MAX_STEP, MAX_STEP)
}

fn apply(sess: &mut Session, s: &State, now: Instant, sink: &mut dyn Sink, first: bool) {
    let old = sess.st;
    if first || sess.idle || s.pad != old.pad {
        sink.pad(sess.slot, &s.pad);
    }
    if !first {
        let (dx, dy) = (step(s.mouse_x, old.mouse_x), step(s.mouse_y, old.mouse_y));
        if dx != 0 || dy != 0 {
            sink.mouse_move(dx, dy);
        }
        let (wv, wh) = (step(s.wheel, old.wheel), step(s.hwheel, old.hwheel));
        if wv != 0 || wh != 0 {
            sink.wheel(wv, wh);
        }
    }
    let (old_mb, old_keys) = if first { (0, [0u16; proto::MAX_KEYS]) } else { (old.mouse_buttons, old.keys) };
    for bit in 0..3u8 {
        let (was, is) = (old_mb >> bit & 1, s.mouse_buttons >> bit & 1);
        if was != is {
            sink.mouse_button(bit, is == 1);
        }
    }
    // Chords like Alt+Tab only work in keyboard order: modifiers go down
    // first and come up last.
    for pass in [false, true] {
        for k in old_keys {
            if k != 0 && is_modifier(k) == pass && !s.keys.contains(&k) {
                sink.key(k, false);
            }
        }
    }
    for pass in [true, false] {
        for k in s.keys {
            if k != 0 && is_modifier(k) == pass && !old_keys.contains(&k) {
                sink.key(k, true);
            }
        }
    }
    sess.st = *s;
    sess.last_seq = s.seq;
    sess.last_rx = now;
    sess.idle = false;
}

/// Let go of everything the session is holding down.
fn release(sess: &mut Session, sink: &mut dyn Sink) {
    for bit in 0..3u8 {
        if sess.st.mouse_buttons >> bit & 1 == 1 {
            sink.mouse_button(bit, false);
        }
    }
    for pass in [false, true] {
        for k in sess.st.keys {
            if k != 0 && is_modifier(k) == pass {
                sink.key(k, false);
            }
        }
    }
    sess.st.mouse_buttons = 0;
    sess.st.keys = [0; proto::MAX_KEYS];
    sess.st.pad = PadState::default();
    sink.pad(sess.slot, &sess.st.pad);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Rec {
        log: Vec<String>,
        plugged: [bool; MAX_PLAYERS],
    }
    impl Sink for Rec {
        fn pad(&mut self, slot: usize, p: &PadState) {
            self.plugged[slot] = true;
            self.log.push(format!("pad{slot} {:04x} {}", p.buttons, p.lx));
        }
        fn pad_release(&mut self, slot: usize) {
            self.plugged[slot] = false;
            self.log.push(format!("unplug{slot}"));
        }
        fn mouse_move(&mut self, dx: i32, dy: i32) {
            self.log.push(format!("move {dx} {dy}"));
        }
        fn wheel(&mut self, v: i32, h: i32) {
            self.log.push(format!("wheel {v} {h}"));
        }
        fn mouse_button(&mut self, bit: u8, down: bool) {
            self.log.push(format!("mb {bit} {down}"));
        }
        fn key(&mut self, vk: u16, down: bool) {
            self.log.push(format!("key {vk} {down}"));
        }
        fn rumble(&mut self, slot: usize) -> (u8, u8) {
            (10 + slot as u8, 20)
        }
        fn pad_ok(&self) -> bool {
            true
        }
        fn keep_awake(&mut self, _on: bool) {}
    }

    fn addr(host: u8, p: u16) -> SocketAddr {
        format!("192.168.1.{host}:{p}").parse().unwrap()
    }

    fn handshake(core: &mut Core, sink: &mut Rec, pin: &str, from: SocketAddr, now: Instant) -> Option<Key> {
        let mut out = Out::new();
        core.handle(&proto::encode_hello(11), from, now, sink, &mut out);
        let (cn, sn) = proto::decode_challenge(&out.first()?.0)?;
        Some(proto::session_key(&proto::pin_key(pin), cn, sn))
    }

    fn join(core: &mut Core, sink: &mut Rec, from: SocketAddr, now: Instant) -> Key {
        let key = handshake(core, sink, "246810", from, now).unwrap();
        let mut out = Out::new();
        core.handle(&proto::encode_state(&key, &State { seq: 1, ..Default::default() }), from, now, sink, &mut out);
        key
    }

    #[test]
    fn full_session() {
        let t0 = Instant::now();
        let mut core = Core::new("246810", "LAPTOP", t0);
        let mut sink = Rec::default();
        let key = handshake(&mut core, &mut sink, "246810", addr(5, 1), t0).unwrap();
        let mut out = Out::new();

        let mut s = State { seq: 1, mouse_x: 1000, ..Default::default() };
        s.pad.buttons = proto::btn::A;
        core.handle(&proto::encode_state(&key, &s), addr(5, 1), t0, &mut sink, &mut out);
        assert!(core.connected());
        assert_eq!(sink.log, vec!["pad0 1000 0"]); // first packet sets the baseline, no mouse jump

        s.seq = 2;
        s.mouse_x = 1012;
        s.mouse_y = -3;
        s.wheel = 120;
        s.mouse_buttons = 1;
        s.keys[0] = 27;
        core.handle(&proto::encode_state(&key, &s), addr(5, 1), t0, &mut sink, &mut out);
        assert_eq!(&sink.log[1..], ["move 12 -3", "wheel 120 0", "mb 0 true", "key 27 true"]);

        // replayed / out-of-order packet is ignored
        let n = sink.log.len();
        core.handle(&proto::encode_state(&key, &s), addr(5, 1), t0, &mut sink, &mut out);
        assert_eq!(sink.log.len(), n);

        // feedback carries rumble and the echo
        core.tick(t0 + Duration::from_millis(5), &mut sink, &mut out);
        let fb = proto::decode_feedback(&key, &out.last().unwrap().0).unwrap();
        assert_eq!((fb.rumble_large, fb.rumble_small, fb.hold_ms), (10, 20, 5));

        // silence releases held inputs
        core.tick(t0 + Duration::from_millis(600), &mut sink, &mut out);
        assert_eq!(&sink.log[n..], ["mb 0 false", "key 27 false", "pad0 0000 0"]);

        // and a long silence unplugs
        core.tick(t0 + Duration::from_secs(30), &mut sink, &mut out);
        assert!(!core.connected());
        assert!(!sink.plugged[0]);
    }

    #[test]
    fn chords_press_modifiers_first_and_release_them_last() {
        let t0 = Instant::now();
        let mut core = Core::new("246810", "LAPTOP", t0);
        let mut sink = Rec::default();
        let key = join(&mut core, &mut sink, addr(5, 1), t0);
        let mut out = Out::new();
        sink.log.clear();
        let mut s = State { seq: 2, ..Default::default() };
        s.keys[0] = 9; // Tab
        s.keys[1] = 18; // Alt
        core.handle(&proto::encode_state(&key, &s), addr(5, 1), t0, &mut sink, &mut out);
        s.seq = 3;
        s.keys = [0; proto::MAX_KEYS];
        core.handle(&proto::encode_state(&key, &s), addr(5, 1), t0, &mut sink, &mut out);
        assert_eq!(sink.log, ["key 18 true", "key 9 true", "key 9 false", "key 18 false"]);
    }

    #[test]
    fn four_players_then_full() {
        let t0 = Instant::now();
        let mut core = Core::new("246810", "LAPTOP", t0);
        let mut sink = Rec::default();
        let mut keys = Vec::new();
        for h in 1..=4u8 {
            keys.push(join(&mut core, &mut sink, addr(h, 9), t0));
        }
        assert_eq!(core.peers().iter().map(|p| p.slot).collect::<Vec<_>>(), [0, 1, 2, 3]);
        assert_eq!(sink.plugged, [true; 4]);

        // a fifth is turned away with "full", not "wrong PIN"
        let k5 = handshake(&mut core, &mut sink, "246810", addr(5, 9), t0).unwrap();
        let mut out = Out::new();
        core.handle(&proto::encode_state(&k5, &State { seq: 1, ..Default::default() }), addr(5, 9), t0, &mut sink, &mut out);
        assert_eq!(proto::decode_reject(&out[0].0), proto::REJECT_FULL);

        // each player drives only their own controller and gets their own rumble
        let mut s = State { seq: 2, ..Default::default() };
        s.pad.buttons = proto::btn::B;
        sink.log.clear();
        core.handle(&proto::encode_state(&keys[2], &s), addr(3, 9), t0, &mut sink, &mut out);
        assert_eq!(sink.log, ["pad2 2000 0"]);
        out.clear();
        core.tick(t0 + Duration::from_millis(150), &mut sink, &mut out);
        let fb = proto::decode_feedback(&keys[2], &out.iter().find(|o| o.1 == addr(3, 9)).unwrap().0).unwrap();
        assert_eq!(fb.rumble_large, 12);

        // player 2 leaves; the slot is reused by the next Ally, others keep theirs
        core.handle(&proto::encode_bye(&keys[1], 9), addr(2, 9), t0, &mut sink, &mut out);
        assert!(!sink.plugged[1]);
        join(&mut core, &mut sink, addr(6, 9), t0);
        assert_eq!(core.peers().iter().map(|p| (p.slot, p.addr.ip().to_string())).nth(1).unwrap(), (1, "192.168.1.6".to_string()));

        // status goes to everyone with their own player number
        out.clear();
        core.send_info(&Info { volume: 30, app: "Game".into(), ..Default::default() }, &mut out);
        assert_eq!(out.len(), 4);
        let (_, info) = proto::decode_info(&keys[3], &out.iter().find(|o| o.1 == addr(4, 9)).unwrap().0).unwrap();
        assert_eq!((info.player, info.volume, info.app.as_str()), (4, 30, "Game"));
    }

    #[test]
    fn reconnect_keeps_the_controller_plugged() {
        let t0 = Instant::now();
        let mut core = Core::new("246810", "LAPTOP", t0);
        let mut sink = Rec::default();
        join(&mut core, &mut sink, addr(7, 100), t0);
        sink.log.clear();
        // same Ally, new port (it restarted its link)
        join(&mut core, &mut sink, addr(7, 200), t0);
        assert!(!sink.log.iter().any(|l| l.starts_with("unplug")));
        assert_eq!(core.peers().len(), 1);
        assert!(core.events.contains(&Event::Reconnected(0)));
    }

    #[test]
    fn wrong_pin_rejected_and_locked_out() {
        let t0 = Instant::now();
        let mut core = Core::new("246810", "LAPTOP", t0);
        let mut sink = Rec::default();
        for i in 0..8 {
            let key = handshake(&mut core, &mut sink, "000000", addr(2, 2), t0).unwrap();
            let mut out = Out::new();
            let s = State { seq: 1, ..Default::default() };
            core.handle(&proto::encode_state(&key, &s), addr(2, 2), t0, &mut sink, &mut out);
            assert_eq!(proto::peek_kind(&out[0].0), Some(kind::REJECT), "attempt {i}");
            assert_eq!(proto::decode_reject(&out[0].0), proto::REJECT_PIN);
            assert!(!core.connected());
        }
        // now locked: even the right PIN gets no challenge
        assert!(handshake(&mut core, &mut sink, "246810", addr(2, 2), t0).is_none());
        // ...until the lockout passes
        let later = t0 + Duration::from_secs(61);
        assert!(handshake(&mut core, &mut sink, "246810", addr(2, 2), later).is_some());
        assert!(sink.log.is_empty());
    }

    #[test]
    fn stale_session_key_cannot_be_replayed() {
        let t0 = Instant::now();
        let mut core = Core::new("246810", "LAPTOP", t0);
        let mut sink = Rec::default();
        let key = handshake(&mut core, &mut sink, "246810", addr(5, 1), t0).unwrap();
        let mut out = Out::new();
        let pkt = proto::encode_state(&key, &State { seq: 1, ..Default::default() });
        core.handle(&pkt, addr(5, 1), t0, &mut sink, &mut out);
        core.tick(t0 + Duration::from_secs(30), &mut sink, &mut out);
        assert!(!core.connected());
        // attacker replays the captured packet after a new HELLO
        let _ = handshake(&mut core, &mut sink, "246810", addr(5, 1), t0 + Duration::from_secs(31));
        core.handle(&pkt, addr(5, 1), t0 + Duration::from_secs(31), &mut sink, &mut out);
        assert!(!core.connected());
    }
}

//! The real-time loop: read the controller and sensors, shape the input,
//! and send it to the laptop at a steady rate. Also owns the network link.

use crate::config::{self, Config, Saved};
use crate::platform::{self, Pad, Sensors};
use crate::process::{MotionIn, Processor, TouchIn};
use allypad_proto as proto;
use proto::{kind, Key};
use serde::Serialize;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub enum Cmd {
    Connect,
    Disconnect,
    Discover,
    Recenter,
    Calibrate,
    /// Touch was locked (true) or unlocked (false).
    Locked(bool),
}

#[derive(Serialize, Clone, PartialEq)]
pub struct Found {
    pub name: String,
    pub ip: String,
}

#[derive(Default)]
pub struct TouchShared {
    pub mouse: (f32, f32),
    pub wheel: (f32, f32),
    pub buttons: u8,
    pub stick: Option<(f32, f32)>,
    pub actions: Vec<String>,
}

pub struct Shared {
    pub cfg: Mutex<Config>,
    pub cfg_version: AtomicU64,
    /// Bumped when the engine itself changes the settings, so the UI reloads them.
    pub cfg_push: AtomicU64,
    pub touch: Mutex<TouchShared>,
    pub cmds: Mutex<Vec<Cmd>>,
    pub status: Mutex<String>,
    pub found: Mutex<Vec<Found>>,
    pub found_version: AtomicU64,
    pub quit: AtomicBool,
    pub ui_clients: AtomicUsize,
    pub ui_seen: AtomicBool,
}

impl Shared {
    pub fn new(cfg: Config) -> Shared {
        Shared {
            cfg: Mutex::new(cfg),
            cfg_version: AtomicU64::new(1),
            cfg_push: AtomicU64::new(0),
            touch: Mutex::new(TouchShared::default()),
            cmds: Mutex::new(Vec::new()),
            status: Mutex::new(String::from("{\"t\":\"st\",\"conn\":\"off\"}")),
            found: Mutex::new(Vec::new()),
            found_version: AtomicU64::new(0),
            quit: AtomicBool::new(false),
            ui_clients: AtomicUsize::new(0),
            ui_seen: AtomicBool::new(false),
        }
    }
}

enum Link {
    Off,
    Hello { cn: u64, next: Instant },
    Up { key: Key, seq: u32, last_fb: Instant, confirmed: bool },
    WrongPin,
    Full,
}

fn resolve(host: &str) -> Option<SocketAddr> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, proto::PORT));
    }
    (host, proto::PORT).to_socket_addrs().ok()?.find(|a| a.is_ipv4())
}

/// Every (address, netmask) this device has on a real network.
fn local_nets() -> Vec<(u32, u32)> {
    let mut nets = Vec::new();
    if let Ok(list) = if_addrs::get_if_addrs() {
        for i in list {
            if let if_addrs::IfAddr::V4(v4) = i.addr {
                if !v4.ip.is_loopback() && !v4.ip.is_link_local() {
                    nets.push((u32::from(v4.ip), u32::from(v4.netmask)));
                }
            }
        }
    }
    if nets.is_empty() {
        // Fall back to the default route's address and assume a home /24.
        if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
            if s.connect("192.0.2.1:9").is_ok() {
                if let Ok(SocketAddr::V4(a)) = s.local_addr() {
                    nets.push((u32::from(*a.ip()), 0xFFFF_FF00));
                }
            }
        }
    }
    nets
}

/// Ask every network we are on who is running the receiver. Broadcasts go
/// out per network (a single 255.255.255.255 only leaves by one adapter),
/// and `sweep` also asks each address directly, for routers that drop
/// broadcasts between Wi-Fi devices.
fn send_discover(sock: &UdpSocket, nonce: u32, sweep: bool) {
    let pkt = proto::encode_discover(nonce);
    let to = |ip: u32| {
        let _ = sock.send_to(&pkt, SocketAddr::new(IpAddr::V4(ip.into()), proto::PORT));
    };
    to(u32::MAX);
    for (ip, mask) in local_nets() {
        if mask == u32::MAX || mask == 0 {
            continue;
        }
        let (net, bcast) = (ip & mask, ip | !mask);
        to(bcast);
        if sweep && bcast - net <= 1024 {
            for host in net + 1..bcast {
                if host != ip {
                    to(host);
                }
            }
        }
    }
}

pub fn run(shared: &Shared) {
    platform::keep_awake();
    let sock = UdpSocket::bind("0.0.0.0:0").expect("udp socket");
    let _ = sock.set_nonblocking(true);
    let _ = sock.set_broadcast(true);

    let mut pad = Pad::new();
    let sensors = Sensors::start();
    let mut proc = Processor::new();

    let start = Instant::now();
    let mut cfg = shared.cfg.lock().unwrap().clone();
    let mut seen_version = 0u64;
    let mut dirty_since: Option<Instant> = None;

    let mut link = Link::Off;
    let mut target: Option<SocketAddr> = None;
    let mut was_up = false;
    let mut rtt_ms: Option<f32> = None;
    let mut laptop_pad_ok = true;
    let mut laptop: Option<proto::Info> = None;
    let mut last_app = String::new();
    let mut eco = false;
    let mut dimmed = false;
    let mut ally_battery = platform::battery();
    let mut battery_at = Instant::now();
    let mut last_profile = cfg.active.clone();
    let mut rumble = (0u8, 0u8);

    let mut discover: Option<(u32, Instant, u8)> = None;
    let mut gyro_bias = [0f32; 3];
    let mut calib: Option<(Instant, [f64; 3], u32)> = None;

    let mut prev_raw = crate::process::RawPad::default();
    let mut last = Instant::now();
    let mut next_tick = last;
    let mut last_status = last;
    let mut sent_count = 0u32;
    let mut hz = 0f32;
    let mut hz_at = last;
    let mut buf = [0u8; 512];

    while !shared.quit.load(Ordering::Relaxed) {
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f32();
        last = now;

        // ---- settings and commands from the UI ----
        let v = shared.cfg_version.load(Ordering::Relaxed);
        if v != seen_version {
            let fresh = shared.cfg.lock().unwrap().clone();
            let relink = fresh.host != cfg.host || fresh.pin != cfg.pin;
            cfg = fresh;
            if seen_version != 0 {
                dirty_since.get_or_insert(now);
            }
            if relink || seen_version == 0 {
                target = resolve(&cfg.host);
                link = match target {
                    Some(_) if cfg.pin.len() >= 4 => Link::Hello { cn: proto::random_u64(), next: now },
                    _ => Link::Off,
                };
                was_up = false;
            }
            seen_version = v;
        }
        if dirty_since.map_or(false, |t| now.duration_since(t) > Duration::from_millis(800)) {
            config::save(&cfg);
            dirty_since = None;
        }
        for cmd in shared.cmds.lock().unwrap().drain(..) {
            match cmd {
                Cmd::Connect => {
                    target = resolve(&cfg.host);
                    if target.is_some() && cfg.pin.len() >= 4 {
                        link = Link::Hello { cn: proto::random_u64(), next: now };
                    }
                }
                Cmd::Disconnect => {
                    if let (Link::Up { key, seq, .. }, Some(t)) = (&link, target) {
                        let _ = sock.send_to(&proto::encode_bye(key, seq.wrapping_add(1)), t);
                    }
                    link = Link::Off;
                    was_up = false;
                }
                Cmd::Discover => {
                    let nonce = proto::random_u64() as u32;
                    send_discover(&sock, nonce, false);
                    shared.found.lock().unwrap().clear();
                    shared.found_version.fetch_add(1, Ordering::Relaxed);
                    discover = Some((nonce, now, 0));
                }
                Cmd::Recenter => proc.request_recenter(),
                Cmd::Calibrate => calib = Some((now, [0.0; 3], 0)),
                Cmd::Locked(on) => {
                    let want = on && cfg.low_power;
                    if want != dimmed {
                        dimmed = want;
                        platform::dim(want);
                    }
                }
            }
        }

        // ---- incoming packets ----
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            let pkt = &buf[..n];
            match proto::peek_kind(pkt) {
                Some(kind::ANNOUNCE) => {
                    if let (Some((nonce, name)), Some((want, _, _))) = (proto::decode_announce(pkt), discover) {
                        if nonce == want {
                            let f = Found { name, ip: from.ip().to_string() };
                            let mut list = shared.found.lock().unwrap();
                            if !list.contains(&f) {
                                list.push(f);
                                shared.found_version.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                Some(kind::CHALLENGE) if Some(from) == target => {
                    if let (Link::Hello { cn, .. }, Some((got_cn, sn))) = (&link, proto::decode_challenge(pkt)) {
                        if *cn == got_cn {
                            crate::log("laptop answered, starting session");
                            let key = proto::session_key(&proto::pin_key(&cfg.pin), *cn, sn);
                            link = Link::Up { key, seq: 0, last_fb: now, confirmed: false };
                        }
                    }
                }
                Some(kind::REJECT) if Some(from) == target => {
                    if matches!(link, Link::Up { confirmed: false, .. }) {
                        if proto::decode_reject(pkt) == proto::REJECT_FULL {
                            crate::log("laptop already has four players");
                            link = Link::Full;
                        } else {
                            crate::log("laptop refused the PIN");
                            link = Link::WrongPin;
                        }
                    }
                }
                Some(kind::FEEDBACK) if Some(from) == target => {
                    if let Link::Up { key, last_fb, confirmed, .. } = &mut link {
                        if let Some(fb) = proto::decode_feedback(key, pkt) {
                            *last_fb = now;
                            if !*confirmed {
                                // Remember this computer for one-tap switching later.
                                let entry = Saved {
                                    name: if cfg.host_name.is_empty() { cfg.host.clone() } else { cfg.host_name.clone() },
                                    host: cfg.host.clone(),
                                    pin: cfg.pin.clone(),
                                };
                                let mut c = shared.cfg.lock().unwrap();
                                if c.saved.first() != Some(&entry) {
                                    c.saved.retain(|s| s.host != entry.host);
                                    c.saved.insert(0, entry);
                                    c.saved.truncate(8);
                                    shared.cfg_version.fetch_add(1, Ordering::Relaxed);
                                    shared.cfg_push.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            *confirmed = true;
                            was_up = true;
                            rumble = (fb.rumble_large, fb.rumble_small);
                            laptop_pad_ok = fb.flags & proto::FB_PAD_OK != 0;
                            let now_ms = start.elapsed().as_millis() as u32;
                            let sample = now_ms.wrapping_sub(fb.echo_t_ms).wrapping_sub(fb.hold_ms as u32) as f32;
                            if sample < 2000.0 {
                                rtt_ms = Some(rtt_ms.map_or(sample, |r| r + (sample - r) * 0.15));
                            }
                        }
                    }
                }
                Some(kind::INFO) if Some(from) == target => {
                    if let Link::Up { key, .. } = &link {
                        if let Some((_, info)) = proto::decode_info(key, pkt) {
                            // Per-game profiles: when a mapped game comes to
                            // the front on the laptop, switch to its game type.
                            if info.app != last_app {
                                last_app = info.app.clone();
                                if cfg.auto_switch {
                                    if let Some(id) = cfg.game_map.get(&info.app.to_lowercase()) {
                                        let mut c = shared.cfg.lock().unwrap();
                                        if c.active != *id && c.profiles.iter().any(|p| p.id == *id) {
                                            crate::log(&format!("{} is running: switching to {id}", info.app));
                                            c.active = id.clone();
                                            shared.cfg_version.fetch_add(1, Ordering::Relaxed);
                                            shared.cfg_push.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                            laptop = Some(info);
                        }
                    }
                }
                _ => {}
            }
        }

        // ---- link upkeep ----
        match &mut link {
            Link::Hello { cn, next } => {
                if now >= *next {
                    if let Some(t) = target {
                        let _ = sock.send_to(&proto::encode_hello(*cn), t);
                    }
                    *next = now + Duration::from_millis(400);
                }
            }
            Link::Up { last_fb, .. } => {
                if now.duration_since(*last_fb) > Duration::from_secs(3) {
                    crate::log(&format!("no reply from the laptop for 3 s at {} s, reconnecting", start.elapsed().as_secs()));
                    link = Link::Hello { cn: proto::random_u64(), next: now };
                    rumble = (0, 0);
                    rtt_ms = None;
                    laptop = None;
                }
            }
            _ => {}
        }
        if let Some((nonce, t0, step)) = &mut discover {
            let age = now.duration_since(*t0);
            if *step == 0 && age > Duration::from_millis(400) {
                send_discover(&sock, *nonce, true);
                *step = 1;
            } else if *step == 1 && age > Duration::from_millis(1500) {
                send_discover(&sock, *nonce, false);
                *step = 2;
            } else if age > Duration::from_secs(4) {
                discover = None;
            }
        }

        // ---- inputs ----
        let raw = pad.read(&cfg.back_keys);
        if cfg.active != last_profile {
            last_profile = cfg.active.clone();
            proc.reset_latches();
        }
        let mr = sensors.take();
        if let Some((t0, sum, n)) = &mut calib {
            for k in 0..3 {
                sum[k] += mr.gyro[k] as f64;
            }
            *n += 1;
            if now.duration_since(*t0) > Duration::from_secs(2) {
                for k in 0..3 {
                    gyro_bias[k] = (sum[k] / *n as f64) as f32;
                }
                calib = None;
            }
        }
        let mut motion = MotionIn { have_gyro: mr.have_gyro, have_accel: mr.have_accel, gyro: mr.gyro, accel: mr.accel, dangle: mr.dangle };
        for k in 0..3 {
            motion.gyro[k] -= gyro_bias[k];
            motion.dangle[k] -= gyro_bias[k] * mr.dangle_dt;
        }
        let touch = {
            let mut t = shared.touch.lock().unwrap();
            let out = TouchIn { mouse: t.mouse, wheel: t.wheel, buttons: t.buttons, stick: t.stick, actions: t.actions.clone() };
            t.mouse = (0.0, 0.0);
            t.wheel = (0.0, 0.0);
            out
        };

        let (mut state, viz) = proc.step(cfg.active_profile(), &raw, &motion, &touch, dt);

        // Diagnostics: a trigger cannot physically travel from well pressed
        // to fully released between two reads a few milliseconds apart, and
        // the loop itself should never pause for long.
        if (prev_raw.lt > 0.6 && raw.lt == 0.0) || (prev_raw.rt > 0.6 && raw.rt == 0.0) {
            crate::log(&format!("trigger reading snapped to zero at {} s (LT {:.2}->{:.2}, RT {:.2}->{:.2})", start.elapsed().as_secs(), prev_raw.lt, raw.lt, prev_raw.rt, raw.rt));
        }
        if dt > 0.1 {
            crate::log(&format!("loop paused for {:.0} ms at {} s", dt * 1000.0, start.elapsed().as_secs()));
        }
        prev_raw = raw;

        // ---- send ----
        let connected = matches!(link, Link::Up { confirmed: true, .. });
        if let (Link::Up { key, seq, .. }, Some(t)) = (&mut link, target) {
            *seq = seq.wrapping_add(1);
            state.seq = *seq;
            state.t_ms = start.elapsed().as_millis() as u32;
            let _ = sock.send_to(&proto::encode_state(key, &state), t);
            sent_count += 1;
        }
        // Low power while it is only being a controller.
        let want_eco = connected && cfg.low_power;
        if want_eco != eco {
            eco = want_eco;
            platform::low_power(eco);
        }
        if dimmed && !cfg.low_power {
            dimmed = false;
            platform::dim(false);
        }
        if now.duration_since(battery_at) >= Duration::from_secs(5) {
            battery_at = now;
            ally_battery = platform::battery();
        }
        if connected {
            pad.rumble(rumble.0 as f32 / 255.0 * cfg.rumble, rumble.1 as f32 / 255.0 * cfg.rumble);
        } else {
            pad.rumble(0.0, 0.0);
        }
        if now.duration_since(hz_at) >= Duration::from_secs(1) {
            hz = sent_count as f32 / now.duration_since(hz_at).as_secs_f32();
            sent_count = 0;
            hz_at = now;
        }

        // ---- status for the UI ----
        if now.duration_since(last_status) >= Duration::from_millis(33) {
            last_status = now;
            let conn = match &link {
                Link::Off => "off",
                Link::WrongPin => "wrongpin",
                Link::Full => "full",
                Link::Up { confirmed: true, .. } => "on",
                _ if was_up => "lost",
                _ => "connecting",
            };
            let r3 = |v: f32| (v * 1000.0).round() / 1000.0;
            let js = serde_json::json!({
                "t": "st",
                "conn": conn,
                // Half the round trip is the one-way delay to the laptop.
                "rtt": rtt_ms.map(|r| (r.max(0.0) * 5.0).round() / 10.0),
                "hz": hz.round(),
                "pad": raw.connected,
                "drv": laptop_pad_ok,
                "gy": mr.have_gyro,
                "ac": mr.have_accel,
                "raw": viz.raw.map(r3),
                "out": viz.out.map(r3),
                "ob": viz.out_buttons,
                "roll": r3(viz.roll),
                "pitch": r3(viz.pitch),
                "yaw": r3(viz.yaw),
                "mo": viz.motion_active,
                "cal": calib.is_some(),
                "extra": raw.extra,
                "bat": ally_battery.map(|b| b.0),
                "chg": ally_battery.map_or(false, |b| b.1),
                "eco": eco,
                "lap": if connected { laptop.as_ref().map(|l| serde_json::json!({
                    "vol": if l.volume <= 100 { Some(l.volume) } else { None },
                    "muted": l.muted,
                    "bat": if l.battery <= 100 { Some(l.battery) } else { None },
                    "chg": l.charging,
                    "player": l.player,
                    "app": l.app,
                })) } else { None },
            });
            *shared.status.lock().unwrap() = js.to_string();
        }

        // ---- pacing ----
        let period = Duration::from_micros(1_000_000 / cfg.rate.max(60) as u64);
        next_tick += period;
        let after = Instant::now();
        if next_tick > after {
            std::thread::sleep(next_tick - after);
        } else if after.duration_since(next_tick) > Duration::from_millis(50) {
            next_tick = after;
        }
    }

    // Tell the laptop we are leaving so it releases everything at once.
    if let (Link::Up { key, seq, .. }, Some(t)) = (&link, target) {
        let _ = sock.send_to(&proto::encode_bye(key, seq.wrapping_add(1)), t);
    }
    pad.rumble(0.0, 0.0);
    // Put the Ally back the way it was.
    if eco {
        platform::low_power(false);
    }
    if dimmed {
        platform::dim(false);
        std::thread::sleep(Duration::from_millis(1500));
    }
    if dirty_since.is_some() {
        config::save(&cfg);
    }
}

//! AllyPad Receiver — run this on the laptop. It turns packets from the
//! AllyPad Sender into a virtual Xbox controller, mouse and keyboard, and
//! shows its state in a small window.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod core;
mod info;
#[cfg(not(windows))]
mod sink_stub;
#[cfg(windows)]
mod sink_win;
mod tray;
mod ui;

use crate::core::{Core, Event, Out, Sink};
use allypad_proto as proto;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::net::{TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const UI_PORT: u16 = 47812;
const DRIVER_URL: &str = "https://github.com/nefarius/ViGEmBus/releases";

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Config {
    pin: String,
    /// Start with Windows and keep running when the window is closed.
    autostart: bool,
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("AllyPad")
}

/// Append a line to AllyPad\receiver.log (there is no console window).
fn log(msg: &str) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("receiver.log")) {
        let _ = writeln!(f, "{msg}");
    }
    #[cfg(not(windows))]
    println!("{msg}");
}

fn new_pin() -> String {
    format!("{:06}", proto::random_u64() % 1_000_000)
}

fn save_config(cfg: &Config) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("receiver.json"), serde_json::to_string_pretty(cfg).unwrap_or_default());
}

fn load_config(force_new: bool, given: Option<String>) -> Config {
    let mut cfg: Config = std::fs::read_to_string(config_dir().join("receiver.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let pin = match given {
        Some(p) => p,
        None if force_new || cfg.pin.len() < 4 => new_pin(),
        None => cfg.pin.clone(),
    };
    if pin != cfg.pin {
        cfg.pin = pin;
        save_config(&cfg);
    }
    cfg
}

/// Register or remove the "run at sign-in" entry for this user. Returns
/// whether the change took effect.
#[cfg(windows)]
fn set_autostart(on: bool) -> bool {
    use std::os::windows::process::CommandExt;
    const RUN: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let mut cmd = std::process::Command::new("reg");
    if on {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        cmd.args(["add", RUN, "/v", "AllyPadReceiver", "/t", "REG_SZ", "/d", &format!("\"{exe}\" --background"), "/f"]);
    } else {
        cmd.args(["delete", RUN, "/v", "AllyPadReceiver", "/f"]);
    }
    cmd.creation_flags(0x0800_0000).status().map(|s| s.success()).unwrap_or(false) || !on
}
#[cfg(not(windows))]
fn set_autostart(_on: bool) -> bool {
    true
}

/// This machine's addresses, the one on the default route first.
fn local_ips() -> Vec<String> {
    let mut ips: Vec<String> = Vec::new();
    // No packet is sent; this only asks the OS which interface it would use.
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("192.0.2.1:9").is_ok() {
            if let Ok(a) = s.local_addr() {
                ips.push(a.ip().to_string());
            }
        }
    }
    if let Ok(list) = if_addrs::get_if_addrs() {
        for i in list {
            if let if_addrs::IfAddr::V4(v4) = i.addr {
                let ip = v4.ip.to_string();
                if !v4.ip.is_loopback() && !v4.ip.is_link_local() && !ips.contains(&ip) {
                    ips.push(ip);
                }
            }
        }
    }
    ips
}

fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Laptop".into())
}

#[cfg(windows)]
mod firewall {
    //! Windows Firewall blocks incoming UDP unless a rule allows it, and the
    //! one-time Windows prompt is easy to miss or to answer for the wrong
    //! network type. So make sure an explicit rule exists.
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn rule_name(port: u16) -> String {
        format!("AllyPad Receiver UDP {port}")
    }

    pub fn rule_exists(port: u16) -> bool {
        Command::new("netsh")
            .args(["advfirewall", "firewall", "show", "rule", &format!("name={}", rule_name(port))])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Adds the rule through one administrator prompt. Returns whether the
    /// rule is in place afterwards.
    pub fn add_rule(port: u16) -> bool {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let script = std::env::temp_dir().join("allypad-firewall.cmd");
        // Windows adds "block" rules for a program when its own prompt is
        // dismissed, and a block beats an allow, so clear those first.
        let body = format!(
            "@echo off\r\nchcp 65001 >nul\r\nnetsh advfirewall firewall delete rule name=all program=\"{exe}\" >nul 2>&1\r\nnetsh advfirewall firewall add rule name=\"{}\" dir=in action=allow protocol=UDP localport={port} profile=any >nul\r\n",
            rule_name(port)
        );
        if std::fs::write(&script, body).is_err() {
            return false;
        }
        let path = script.display().to_string().replace('\'', "''");
        let _ = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("Start-Process -FilePath '{path}' -Verb RunAs -WindowStyle Hidden -Wait"),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        let _ = std::fs::remove_file(&script);
        rule_exists(port)
    }
}

// Firewall states shown in the window.
const FW_NA: u8 = 0;
const FW_OK: u8 = 1;
const FW_ASKING: u8 = 2;
const FW_MISSING: u8 = 3;

#[cfg(windows)]
fn firewall_task(state: Arc<AtomicU8>, port: u16, ask: bool) {
    if state.swap(FW_ASKING, Ordering::Relaxed) == FW_ASKING {
        return; // a prompt is already open
    }
    std::thread::spawn(move || {
        let ok = firewall::rule_exists(port) || (ask && firewall::add_rule(port));
        state.store(if ok { FW_OK } else { FW_MISSING }, Ordering::Relaxed);
    });
}
#[cfg(not(windows))]
fn firewall_task(state: Arc<AtomicU8>, _port: u16, _ask: bool) {
    state.store(FW_NA, Ordering::Relaxed);
}

#[cfg(windows)]
fn platform_init() {
    use windows::Win32::System::Threading::*;
    unsafe {
        // 1 ms timer resolution so socket timeouts and sleeps are precise.
        windows::Win32::Media::timeBeginPeriod(1);
        // No window of its own, so Windows 11 would treat this as background
        // work (slower cores, coarse timers). Opt out.
        let me = GetCurrentProcess();
        let _ = SetPriorityClass(me, HIGH_PRIORITY_CLASS);
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        let _ = SetProcessInformation(
            me,
            ProcessPowerThrottling,
            &state as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}
#[cfg(not(windows))]
fn platform_init() {}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut given = None;
    let mut force_new = false;
    let mut skip_firewall = false;
    let mut no_ui = false;
    let mut background = false;
    let mut port = proto::PORT;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pin" => {
                given = args.get(i + 1).cloned();
                i += 1;
            }
            "--new-pin" => force_new = true,
            "--no-firewall" => skip_firewall = true,
            "--no-ui" => no_ui = true,
            "--background" => background = true,
            "--port" => {
                port = args.get(i + 1).and_then(|p| p.parse().ok()).unwrap_or(proto::PORT);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    let ui_port = std::env::var("ALLYPAD_RX_UI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(UI_PORT);
    let url = format!("http://127.0.0.1:{ui_port}/");
    let listener = match TcpListener::bind(("127.0.0.1", ui_port)) {
        Ok(l) => l,
        Err(_) => {
            // Already running: just bring up its window.
            if !no_ui && !background {
                let _ = ui::open_window(&url);
            }
            return;
        }
    };

    let _ = std::fs::remove_file(config_dir().join("receiver.log"));
    log("AllyPad Receiver starting");
    platform_init();
    let mut cfg = load_config(force_new, given);
    let mut pin = cfg.pin.clone();
    let name = host_name();

    let shared = Arc::new(ui::Shared::new());
    {
        let shared = shared.clone();
        std::thread::spawn(move || ui::serve(shared, listener, ui_port));
    }

    let (sock, sock_err) = match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(s) => {
            let _ = s.set_read_timeout(Some(Duration::from_millis(4)));
            (Some(s), None)
        }
        Err(e) => {
            log(&format!("could not open UDP port {port}: {e}"));
            (None, Some(format!("UDP port {port} is in use by another program.")))
        }
    };

    #[cfg(windows)]
    let mut sink = sink_win::WinSink::new();
    #[cfg(not(windows))]
    let mut sink = sink_stub::StubSink::new();
    if let Some(e) = &sink.pad_error {
        log(&format!("virtual controller driver not available: {e}"));
    }

    let fw = Arc::new(AtomicU8::new(FW_NA));
    if !skip_firewall {
        firewall_task(fw.clone(), port, true);
    }

    let mut window = if no_ui || background { None } else { ui::open_window(&url) };
    // With a tray icon the receiver outlives its window.
    let in_tray = !no_ui && tray::start();
    if !in_tray && !no_ui {
        log("tray icon not available; the app will stop when its window closes");
    }
    let laptop = info::start();
    let mut info_at = Instant::now();

    let start = Instant::now();
    let mut core = Core::new(&pin, &name, start);
    let mut buf = [0u8; 512];
    let mut out = Out::new();
    let mut events: VecDeque<(Instant, String)> = VecDeque::new();
    let note = |events: &mut VecDeque<(Instant, String)>, text: String| {
        log(&text);
        events.push_front((Instant::now(), text));
        events.truncate(4);
    };
    let ips = local_ips();
    let mut last_status = start;
    let mut rate_at = start;
    let mut rate_packets = 0u64;
    let mut rate = 0f64;
    let mut alone_since: Option<Instant> = None;

    while !shared.quit.load(Ordering::Relaxed) {
        match &sock {
            Some(s) => {
                if let Ok((n, from)) = s.recv_from(&mut buf) {
                    core.handle(&buf[..n], from, Instant::now(), &mut sink, &mut out);
                }
                // An error is a timeout, or a harmless ICMP echo on Windows.
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
        let now = Instant::now();
        core.tick(now, &mut sink, &mut out);
        if let Some(s) = &sock {
            for (pkt, to) in out.drain(..) {
                let _ = s.send_to(&pkt, to);
            }
        }
        for ev in core.events.drain(..) {
            let text = match ev {
                Event::Connected(a, n) => format!("Player {} connected from {}", n + 1, a.ip()),
                Event::Idle(n) => format!("Player {} signal lost, inputs released", n + 1),
                Event::Resumed(n) => format!("Player {} signal back", n + 1),
                Event::Disconnected(n) => format!("Player {} disconnected", n + 1),
                Event::Reconnected(n) => format!("Player {} reconnected", n + 1),
                Event::Full(a) => format!("{} turned away: four players already", a.ip()),
                Event::WrongPin(a) => format!("{} tried a wrong PIN", a.ip()),
                Event::LockedOut => "Too many wrong PINs, pausing new connections for a minute".to_string(),
            };
            note(&mut events, text);
        }
        match tray::take_action() {
            tray::OPEN => {
                // One window is enough: only open it if none is showing.
                if shared.clients.load(Ordering::Relaxed) == 0 {
                    if let Some(mut old) = window.take() {
                        let _ = old.kill();
                    }
                    window = ui::open_window(&url);
                }
            }
            tray::QUIT => break,
            _ => {}
        }
        for cmd in shared.cmds.lock().unwrap().drain(..) {
            match cmd {
                ui::Cmd::NewPin => {
                    pin = new_pin();
                    cfg.pin = pin.clone();
                    save_config(&cfg);
                    core.set_pin(&pin, &mut sink);
                    note(&mut events, "New PIN set".to_string());
                }
                ui::Cmd::Firewall => firewall_task(fw.clone(), port, true),
                ui::Cmd::Driver => ui::install_driver(DRIVER_URL),
                ui::Cmd::Autostart(on) => {
                    if set_autostart(on) {
                        cfg.autostart = on;
                        save_config(&cfg);
                        note(&mut events, if on { "Will start with Windows".to_string() } else { "Will not start with Windows".to_string() });
                    } else {
                        note(&mut events, "Could not change the start-up setting".to_string());
                    }
                }
            }
        }

        if now.duration_since(info_at) >= Duration::from_secs(1) {
            info_at = now;
            if !sink.pad_ok() && start.elapsed().as_secs() % 4 == 0 {
                sink.retry_driver();
            }
            let li = laptop.lock().unwrap().clone();
            core.send_info(&li, &mut out);
        }
        if now.duration_since(rate_at) >= Duration::from_secs(1) {
            rate = (core.packets - rate_packets) as f64 / now.duration_since(rate_at).as_secs_f64();
            rate_packets = core.packets;
            rate_at = now;
        }
        if now.duration_since(last_status) >= Duration::from_millis(50) {
            last_status = now;
            let peers = core.peers();
            let li = laptop.lock().unwrap().clone();
            let js = serde_json::json!({
                "t": "st",
                "name": name,
                "ips": ips,
                "port": port,
                "pin": pin,
                "driver": sink.pad_ok(),
                "fw": match fw.load(Ordering::Relaxed) { FW_OK => "ok", FW_ASKING => "asking", FW_MISSING => "missing", _ => "na" },
                "err": sock_err,
                "players": peers.iter().map(|p| serde_json::json!({
                    "n": p.slot + 1,
                    "ip": p.addr.ip().to_string(),
                    "idle": p.idle,
                    "pad": [p.pad.buttons as i32, p.pad.lt as i32, p.pad.rt as i32, p.pad.lx as i32, p.pad.ly as i32, p.pad.rx as i32, p.pad.ry as i32],
                })).collect::<Vec<_>>(),
                "rate": rate.round(),
                "autostart": cfg.autostart,
                "tray": in_tray,
                "app": li.app,
                "log": events.iter().map(|(t, s)| serde_json::json!({"ago": now.duration_since(*t).as_secs(), "text": s})).collect::<Vec<_>>(),
            });
            *shared.status.lock().unwrap() = js.to_string();

            // The app lives as long as its window, unless it is set to stay
            // in the background.
            if !no_ui && !background && !cfg.autostart && !in_tray {
                if shared.clients.load(Ordering::Relaxed) > 0 {
                    alone_since = None;
                } else if shared.seen.load(Ordering::Relaxed) {
                    if alone_since.get_or_insert(now).elapsed() > Duration::from_secs(3) {
                        log("window closed, stopping");
                        break;
                    }
                } else if start.elapsed() > Duration::from_secs(180) {
                    log("window never opened, stopping");
                    break;
                }
            }
        }
    }

    // Let go of everything and unplug the virtual controllers.
    core.shutdown(&mut sink);
    sink.keep_awake(false);
    if let Some(w) = window.as_mut() {
        let _ = w.kill();
    }
    tray::stop();
}

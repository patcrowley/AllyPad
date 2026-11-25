//! AllyPad Sender — run this on the ROG Ally. It reads the built-in
//! controller, motion sensors and a touchscreen trackpad, and streams them
//! to AllyPad Receiver on the laptop.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod engine;
mod process;
mod server;
#[cfg(windows)]
#[path = "platform_win.rs"]
mod platform;
#[cfg(not(windows))]
#[path = "platform_stub.rs"]
mod platform;

use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const UI_PORT: u16 = 47811;

/// Append a line to AllyPad\sender.log (there is no console window).
pub fn log(msg: &str) {
    let dir = config::dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("sender.log")) {
        let _ = writeln!(f, "{msg}");
    }
}

fn main() {
    let no_ui = std::env::args().any(|a| a == "--no-ui");
    let port = std::env::var("ALLYPAD_UI_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(UI_PORT);
    let url = format!("http://127.0.0.1:{port}/");

    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(_) => {
            // Already running: just bring up its window.
            if !no_ui {
                let _ = platform::open_ui(&url);
            }
            return;
        }
    };

    let _ = std::fs::remove_file(config::dir().join("sender.log"));
    log("AllyPad Sender starting");
    platform::init();

    let shared = Arc::new(engine::Shared::new(config::load()));
    {
        let shared = shared.clone();
        std::thread::spawn(move || server::run(shared, listener, port));
    }
    let engine_thread = {
        let shared = shared.clone();
        std::thread::spawn(move || engine::run(&shared))
    };
    let mut window = if no_ui { None } else { platform::open_ui(&url) };

    // The app lives as long as its window: stop shortly after the UI goes
    // away, or if it never appears at all.
    let started = Instant::now();
    let mut alone_since: Option<Instant> = None;
    while !shared.quit.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        if no_ui {
            continue;
        }
        let seen = shared.ui_seen.load(Ordering::Relaxed);
        if shared.ui_clients.load(Ordering::Relaxed) > 0 {
            alone_since = None;
        } else if seen {
            let since = *alone_since.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_secs(3) {
                log("window closed, stopping");
                shared.quit.store(true, Ordering::Relaxed);
            }
        } else if started.elapsed() > Duration::from_secs(180) {
            log("window never opened, stopping");
            shared.quit.store(true, Ordering::Relaxed);
        }
    }
    let _ = engine_thread.join();
    // Take the window down with the app.
    if let Some(w) = window.as_mut() {
        let _ = w.kill();
    }
}

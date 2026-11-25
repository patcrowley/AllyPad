//! Serves the touch UI to a local browser window and talks to it over a
//! WebSocket. Bound to 127.0.0.1 only.

use crate::config::{self, Config};
use crate::engine::{Cmd, Shared};
use serde_json::Value;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tungstenite::Message;

const INDEX: &str = include_str!("../ui/index.html");
const ICON: &str = include_str!("../ui/icon.svg");
const FONTS: [(&str, &[u8]); 6] = [
    ("barlow-semi-condensed-500.woff2", include_bytes!("../ui/fonts/barlow-semi-condensed-500.woff2")),
    ("barlow-semi-condensed-700.woff2", include_bytes!("../ui/fonts/barlow-semi-condensed-700.woff2")),
    ("bricolage-grotesque-500.woff2", include_bytes!("../ui/fonts/bricolage-grotesque-500.woff2")),
    ("bricolage-grotesque-700.woff2", include_bytes!("../ui/fonts/bricolage-grotesque-700.woff2")),
    ("chakra-petch-500.woff2", include_bytes!("../ui/fonts/chakra-petch-500.woff2")),
    ("chakra-petch-700.woff2", include_bytes!("../ui/fonts/chakra-petch-700.woff2")),
];

pub fn run(shared: Arc<Shared>, listener: TcpListener, port: u16) {
    for stream in listener.incoming().flatten() {
        let shared = shared.clone();
        std::thread::spawn(move || handle(shared, stream, port));
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn respond(mut s: TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
    let _ = s.flush();
}

fn handle(shared: Arc<Shared>, stream: TcpStream, port: u16) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    // Look at the request without consuming it, so a WebSocket upgrade can
    // still be handed to the WebSocket library intact.
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(3);
    let head = loop {
        match stream.peek(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if let Some(end) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
                    break String::from_utf8_lossy(&buf[..end]).into_owned();
                }
                if n == buf.len() || Instant::now() > deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return,
        }
    };
    let mut first = head.lines().next().unwrap_or("").split_whitespace();
    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let path = path.split('?').next().unwrap_or("");

    // Only this machine's own UI may talk to us: refuse other Host and
    // Origin values so a web page open on the Ally cannot drive the laptop.
    let hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let host_ok = header(&head, "host").map_or(false, |h| hosts.iter().any(|x| x == h));
    let origin_ok = header(&head, "origin").map_or(true, |o| hosts.iter().any(|x| o == format!("http://{x}")));
    if method != "GET" || !host_ok || !origin_ok {
        drain(&stream);
        return respond(stream, "403 Forbidden", "text/plain", b"forbidden");
    }

    if path == "/ws" {
        if header(&head, "origin").is_none() {
            drain(&stream);
            return respond(stream, "403 Forbidden", "text/plain", b"forbidden");
        }
        return websocket(shared, stream);
    }
    drain(&stream);
    if path == "/" || path == "/index.html" {
        return respond(stream, "200 OK", "text/html; charset=utf-8", INDEX.as_bytes());
    }
    if path == "/icon.svg" {
        return respond(stream, "200 OK", "image/svg+xml", ICON.as_bytes());
    }
    if let Some(name) = path.strip_prefix("/fonts/") {
        if let Some((_, data)) = FONTS.iter().find(|(n, _)| *n == name) {
            return respond(stream, "200 OK", "font/woff2", data);
        }
    }
    respond(stream, "404 Not Found", "text/plain", b"not found");
}

fn drain(mut s: &TcpStream) {
    let mut buf = [0u8; 4096];
    let _ = s.read(&mut buf);
}

struct ClientGuard<'a>(&'a Shared);
impl Drop for ClientGuard<'_> {
    fn drop(&mut self) {
        // Never leave a key or button held because the window went away.
        let mut t = self.0.touch.lock().unwrap();
        t.buttons = 0;
        t.stick = None;
        t.actions.clear();
        drop(t);
        // The window is gone, so nothing can still be locked or dimmed.
        self.0.cmds.lock().unwrap().push(Cmd::Locked(false));
        self.0.ui_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

fn websocket(shared: Arc<Shared>, stream: TcpStream) {
    let Ok(mut ws) = tungstenite::accept(stream) else { return };
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(15)));
    shared.ui_clients.fetch_add(1, Ordering::Relaxed);
    shared.ui_seen.store(true, Ordering::Relaxed);
    let _guard = ClientGuard(&shared);

    let hello = serde_json::json!({
        "t": "hello",
        "cfg": *shared.cfg.lock().unwrap(),
        "defaults": config::default_profiles(),
    });
    if ws.send(Message::Text(hello.to_string())).is_err() {
        return;
    }

    let mut last_status = Instant::now();
    let mut found_seen = u64::MAX;
    let mut push_seen = shared.cfg_push.load(Ordering::Relaxed);
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    on_message(&shared, &v);
                }
            }
            Ok(Message::Close(_)) => return,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return,
        }
        if shared.quit.load(Ordering::Relaxed) {
            return;
        }
        if last_status.elapsed() >= Duration::from_millis(33) {
            last_status = Instant::now();
            let s = shared.status.lock().unwrap().clone();
            if ws.send(Message::Text(s)).is_err() {
                return;
            }
            let pv = shared.cfg_push.load(Ordering::Relaxed);
            if pv != push_seen {
                push_seen = pv;
                let msg = serde_json::json!({"t": "cfg", "cfg": *shared.cfg.lock().unwrap()});
                if ws.send(Message::Text(msg.to_string())).is_err() {
                    return;
                }
            }
            let fv = shared.found_version.load(Ordering::Relaxed);
            if fv != found_seen {
                found_seen = fv;
                let msg = serde_json::json!({"t": "found", "list": *shared.found.lock().unwrap()});
                if ws.send(Message::Text(msg.to_string())).is_err() {
                    return;
                }
            }
        }
    }
}

fn num(v: &Value, k: &str) -> f32 {
    let n = v.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
    if n.is_finite() {
        n
    } else {
        0.0
    }
}

fn on_message(shared: &Shared, v: &Value) {
    let cmd = |c: Cmd| shared.cmds.lock().unwrap().push(c);
    match v.get("t").and_then(Value::as_str).unwrap_or("") {
        "tp" => {
            let mut t = shared.touch.lock().unwrap();
            t.mouse.0 += num(v, "x").clamp(-3000.0, 3000.0);
            t.mouse.1 += num(v, "y").clamp(-3000.0, 3000.0);
        }
        "wh" => {
            let mut t = shared.touch.lock().unwrap();
            t.wheel.0 += num(v, "v").clamp(-3000.0, 3000.0);
            t.wheel.1 += num(v, "h").clamp(-3000.0, 3000.0);
        }
        "mb" => shared.touch.lock().unwrap().buttons = (num(v, "b") as u8) & 7,
        "vs" => {
            let up = v.get("up").and_then(Value::as_bool).unwrap_or(false);
            shared.touch.lock().unwrap().stick =
                if up { None } else { Some((num(v, "x").clamp(-1.0, 1.0), num(v, "y").clamp(-1.0, 1.0))) };
        }
        "tk" => {
            let list: Vec<String> = v
                .get("a")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).filter(|s| s.len() <= 16).take(8).map(String::from).collect())
                .unwrap_or_default();
            shared.touch.lock().unwrap().actions = list;
        }
        "cfg" => {
            if let Some(c) = v.get("cfg") {
                if let Ok(mut new) = serde_json::from_value::<Config>(c.clone()) {
                    new.sanitize();
                    let mut cur = shared.cfg.lock().unwrap();
                    if *cur != new {
                        *cur = new;
                        shared.cfg_version.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        "connect" => cmd(Cmd::Connect),
        "disconnect" => cmd(Cmd::Disconnect),
        "discover" => cmd(Cmd::Discover),
        "recenter" => cmd(Cmd::Recenter),
        "calibrate" => cmd(Cmd::Calibrate),
        "locked" => cmd(Cmd::Locked(v.get("on").and_then(Value::as_bool).unwrap_or(false))),
        "quit" => shared.quit.store(true, Ordering::Relaxed),
        _ => {}
    }
}

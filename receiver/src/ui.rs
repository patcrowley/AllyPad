//! The receiver's window: a small local page shown in an Edge app window,
//! fed over a WebSocket. Bound to 127.0.0.1 only.

use serde_json::Value;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::Message;

const INDEX: &str = include_str!("../ui/index.html");
const ICON: &str = include_str!("../ui/icon.svg");
const FONTS: [(&str, &[u8]); 2] = [
    ("bricolage-grotesque-500.woff2", include_bytes!("../../sender/ui/fonts/bricolage-grotesque-500.woff2")),
    ("bricolage-grotesque-700.woff2", include_bytes!("../../sender/ui/fonts/bricolage-grotesque-700.woff2")),
];

pub enum Cmd {
    NewPin,
    Firewall,
    Driver,
    Autostart(bool),
}

pub struct Shared {
    pub status: Mutex<String>,
    pub cmds: Mutex<Vec<Cmd>>,
    pub quit: AtomicBool,
    pub clients: AtomicUsize,
    pub seen: AtomicBool,
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            status: Mutex::new(String::from("{\"t\":\"st\"}")),
            cmds: Mutex::new(Vec::new()),
            quit: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            seen: AtomicBool::new(false),
        }
    }
}

pub fn serve(shared: Arc<Shared>, listener: TcpListener, port: u16) {
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

fn drain(mut s: &TcpStream) {
    let mut buf = [0u8; 4096];
    let _ = s.read(&mut buf);
}

fn handle(shared: Arc<Shared>, stream: TcpStream, port: u16) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
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

    // Only this machine's own window may talk to us.
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

struct Guard<'a>(&'a Shared);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

fn websocket(shared: Arc<Shared>, stream: TcpStream) {
    let Ok(mut ws) = tungstenite::accept(stream) else { return };
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(40)));
    shared.clients.fetch_add(1, Ordering::Relaxed);
    shared.seen.store(true, Ordering::Relaxed);
    let _guard = Guard(&shared);
    let mut last = Instant::now() - Duration::from_secs(1);
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    let cmd = |c: Cmd| shared.cmds.lock().unwrap().push(c);
                    match v.get("t").and_then(Value::as_str).unwrap_or("") {
                        "newpin" => cmd(Cmd::NewPin),
                        "firewall" => cmd(Cmd::Firewall),
                        "driver" => cmd(Cmd::Driver),
                        "autostart" => cmd(Cmd::Autostart(v.get("on").and_then(Value::as_bool).unwrap_or(false))),
                        "quit" => shared.quit.store(true, Ordering::Relaxed),
                        _ => {}
                    }
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
        if last.elapsed() >= Duration::from_millis(50) {
            last = Instant::now();
            let s = shared.status.lock().unwrap().clone();
            if ws.send(Message::Text(s)).is_err() {
                return;
            }
        }
    }
}

/// Opens the window; returns the Edge process when it was started directly
/// so it can be closed again on exit.
#[cfg(windows)]
pub fn open_window(url: &str) -> Option<std::process::Child> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut candidates = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            candidates.push(std::path::PathBuf::from(base).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
    }
    if let Some(edge) = candidates.into_iter().find(|p| p.exists()) {
        let profile = crate::config_dir().join("edge-profile-receiver");
        let _ = std::fs::create_dir_all(&profile);
        let child = Command::new(edge)
            .arg(format!("--app={url}"))
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--window-size=460,800",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-mode",
                "--hide-crash-restore-bubble",
            ])
            .spawn();
        if let Ok(c) = child {
            return Some(c);
        }
    }
    let app = Command::new("cmd")
        .args(["/C", "start", "", "msedge", &format!("--app={url}"), "--window-size=460,800"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !app {
        let _ = Command::new("cmd").args(["/C", "start", "", url]).creation_flags(CREATE_NO_WINDOW).status();
    }
    None
}

#[cfg(not(windows))]
pub fn open_window(url: &str) -> Option<std::process::Child> {
    println!("AllyPad Receiver UI: {url}");
    None
}

/// Install the controller driver the easiest way available: a bundled
/// installer next to this program, else Windows' own package manager,
/// else the download page.
pub fn install_driver(page: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let bundled = std::env::current_exe().ok().and_then(|exe| {
            std::fs::read_dir(exe.parent()?).ok()?.flatten().map(|e| e.path()).find(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
                name.starts_with("vigembus") && (name.ends_with(".exe") || name.ends_with(".msi"))
            })
        });
        if let Some(installer) = bundled {
            // "start" goes through the shell, which shows the administrator prompt.
            let _ = Command::new("cmd").args(["/C", "start", ""]).arg(installer).creation_flags(CREATE_NO_WINDOW).status();
            return;
        }
        let has_winget = Command::new("where").arg("winget").creation_flags(CREATE_NO_WINDOW).output().map(|o| o.status.success()).unwrap_or(false);
        if has_winget {
            // In a visible console so progress and prompts can be seen.
            let _ = Command::new("cmd")
                .args(["/C", "start", "Installing the controller driver", "cmd", "/C", "winget install --id ViGEm.ViGEmBus -e --accept-source-agreements --accept-package-agreements & pause"])
                .creation_flags(CREATE_NO_WINDOW)
                .status();
            return;
        }
    }
    open_link(page);
}

/// Opens a web page in the default browser.
pub fn open_link(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).creation_flags(0x0800_0000).status();
    }
    #[cfg(not(windows))]
    println!("open {url}");
}

//! Phone remote, two ways to reach the phone:
//!
//! * Internet (relay): the app connects OUT to
//!   wss://tele.johorstudio.com/relay/mac/<session>; the phone opens
//!   https://tele.johorstudio.com/r/<session> (from the QR). No Tailscale,
//!   works on any network, HTTPS so wake-lock/fullscreen work.
//! * Local (fallback, no internet): http://<mac-ip>:8140 serves the page,
//!   ws://<mac-ip>:8141 carries the data. Tailscale (100.x) or same Wi-Fi.
//!
//! Messages app → phone: {"t":"layout",…} (screen-2 rows, same line breaks)
//! and {"t":"state",…}. Phone → app: {"cmd":…} (see `Cmd`).

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::Deserialize;
use tungstenite::Message;
use tungstenite::stream::MaybeTlsStream;

pub const HTTP_PORT: u16 = 8140;
pub const WS_PORT: u16 = 8141;

const PAGE: &str = include_str!("remote.html");
const MANIFEST: &str = r##"{"name":"JohorStudio Remote","short_name":"Remote","display":"fullscreen","orientation":"portrait","background_color":"#0d0d0f","theme_color":"#0d0d0f","start_url":"/"}"##;

/// A command sent by the phone.
#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Cmd {
    Toggle,
    Slower,
    Faster,
    Stop,
    Top,
    LineUp,
    LineDown,
    DefaultSpeed { v: i32 },
    Font { delta: f32 },
    /// Finger drag on the preview: absolute prompter y on the reading line.
    ScrollTo { y: f32 },
    /// Tap on the preview: glide the row at prompter y to the reading line.
    Jump { y: f32 },
}

#[derive(Default)]
pub struct Shared {
    pub state: String,
    pub layout: String,
    pub layout_ver: u64,
    pub cmds: Vec<Cmd>,
    pub local_clients: usize,
    pub relay_clients: usize,
    pub relay_ok: bool,
    /// wss://…/relay/mac/<session>; empty = relay off.
    pub relay_url: String,
    /// Bumped when the session changes, so the relay reconnects.
    pub relay_gen: u64,
    pub ctx: Option<egui::Context>,
    pub error: Option<String>,
}

pub type Handle = Arc<Mutex<Shared>>;

pub fn start() -> Handle {
    let h: Handle = Arc::new(Mutex::new(Shared::default()));
    for f in [http_loop as fn(Handle), ws_loop, relay_loop] {
        let h = h.clone();
        thread::spawn(move || f(h));
    }
    h
}

fn repaint(h: &Handle) {
    if let Some(c) = h.lock().unwrap().ctx.clone() {
        c.request_repaint();
    }
}

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if io.kind() == ErrorKind::WouldBlock || io.kind() == ErrorKind::TimedOut)
}

fn handle_text(h: &Handle, t: &str) {
    if let Ok(c) = serde_json::from_str::<Cmd>(t) {
        h.lock().unwrap().cmds.push(c);
        repaint(h);
    } else if t.starts_with("{\"t\":\"clients\"") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            h.lock().unwrap().relay_clients = v["n"].as_u64().unwrap_or(0) as usize;
            repaint(h);
        }
    }
}

/// What still has to go out on one connection: (layout if newer, state if changed).
fn pending(h: &Handle, ver: &mut u64, state: &mut String) -> (Option<String>, Option<String>) {
    let g = h.lock().unwrap();
    let layout = (g.layout_ver != *ver && !g.layout.is_empty()).then(|| {
        *ver = g.layout_ver;
        g.layout.clone()
    });
    let st = (g.state != *state && !g.state.is_empty()).then(|| {
        *state = g.state.clone();
        g.state.clone()
    });
    (layout, st)
}

// ------------------------------------------------------------------ local

fn header(k: &str, v: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn http_loop(h: Handle) {
    let server = match tiny_http::Server::http(("0.0.0.0", HTTP_PORT)) {
        Ok(s) => s,
        Err(e) => {
            h.lock().unwrap().error = Some(format!("Port {HTTP_PORT}: {e}"));
            return;
        }
    };
    for req in server.incoming_requests() {
        let path = req.url().split('?').next().unwrap_or("/").to_string();
        let (body, ctype) = match path.as_str() {
            "/" | "/index.html" => (PAGE, "text/html; charset=utf-8"),
            "/manifest.json" => (MANIFEST, "application/manifest+json"),
            _ => {
                let _ = req.respond(tiny_http::Response::from_string("not found").with_status_code(404));
                continue;
            }
        };
        let resp = tiny_http::Response::from_string(body)
            .with_header(header("Content-Type", ctype))
            .with_header(header("Cache-Control", "no-store"));
        let _ = req.respond(resp);
    }
}

fn ws_loop(h: Handle) {
    let l = match TcpListener::bind(("0.0.0.0", WS_PORT)) {
        Ok(l) => l,
        Err(e) => {
            h.lock().unwrap().error = Some(format!("Port {WS_PORT}: {e}"));
            return;
        }
    };
    for s in l.incoming().flatten() {
        let h = h.clone();
        thread::spawn(move || ws_client(s, h));
    }
}

fn ws_client(stream: TcpStream, h: Handle) {
    let _ = stream.set_nodelay(true);
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(33)));
    h.lock().unwrap().local_clients += 1;
    repaint(&h);
    let (mut ver, mut state) = (u64::MAX, String::new());
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => handle_text(&h, t.as_str()),
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) if is_timeout(&e) => {}
            Err(_) => break,
        }
        let (layout, st) = pending(&h, &mut ver, &mut state);
        let ok = layout.map_or(true, |l| ws.send(Message::text(l)).is_ok())
            && st.map_or(true, |s| ws.send(Message::text(s)).is_ok());
        if !ok {
            break;
        }
    }
    h.lock().unwrap().local_clients -= 1;
    repaint(&h);
}

// ------------------------------------------------------------------ relay

fn relay_loop(h: Handle) {
    loop {
        let (url, generation) = {
            let g = h.lock().unwrap();
            (g.relay_url.clone(), g.relay_gen)
        };
        if url.is_empty() {
            thread::sleep(Duration::from_secs(1));
            continue;
        }
        if let Ok((mut ws, _)) = tungstenite::connect(url.as_str()) {
            let t = Some(Duration::from_millis(33));
            let _ = match ws.get_mut() {
                MaybeTlsStream::Plain(s) => s.set_read_timeout(t),
                MaybeTlsStream::NativeTls(s) => s.get_ref().set_read_timeout(t),
                _ => Ok(()),
            };
            h.lock().unwrap().relay_ok = true;
            repaint(&h);
            let (mut ver, mut state) = (u64::MAX, String::new());
            let mut last_ping = Instant::now();
            loop {
                if h.lock().unwrap().relay_gen != generation {
                    break;
                }
                match ws.read() {
                    Ok(Message::Text(t)) => handle_text(&h, t.as_str()),
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(e) if is_timeout(&e) => {}
                    Err(_) => break,
                }
                let (layout, st) = pending(&h, &mut ver, &mut state);
                let mut ok = layout.map_or(true, |l| ws.send(Message::text(l)).is_ok())
                    && st.map_or(true, |s| ws.send(Message::text(s)).is_ok());
                if ok && last_ping.elapsed() > Duration::from_secs(20) {
                    last_ping = Instant::now();
                    ok = ws.send(Message::Ping(Vec::new().into())).is_ok();
                }
                if !ok {
                    break;
                }
            }
            let _ = ws.close(None);
            let mut g = h.lock().unwrap();
            g.relay_ok = false;
            g.relay_clients = 0;
            drop(g);
            repaint(&h);
        }
        // Retry soon (also right away after a session change).
        for _ in 0..30 {
            if h.lock().unwrap().relay_gen != generation {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

// ------------------------------------------------------------------ helpers

/// Best local address for the phone: Tailscale (100.64.0.0/10) first, then LAN.
pub fn best_ip() -> Option<Ipv4Addr> {
    let addrs = if_addrs::get_if_addrs().ok()?;
    let v4: Vec<Ipv4Addr> = addrs
        .iter()
        .filter(|a| !a.is_loopback())
        .filter_map(|a| match a.ip() {
            IpAddr::V4(ip) => Some(ip),
            _ => None,
        })
        .collect();
    let tailscale = |ip: &Ipv4Addr| ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]);
    v4.iter()
        .find(|ip| tailscale(ip))
        .or_else(|| v4.iter().find(|ip| ip.is_private()))
        .copied()
}

/// New random session id (24 hex chars).
pub fn new_session() -> String {
    let mut b = [0u8; 12];
    if getrandom::fill(&mut b).is_err() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b.copy_from_slice(&n.to_le_bytes()[..12]);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// QR modules (true = dark) and width for `text`.
pub fn qr(text: &str) -> Option<(Vec<bool>, usize)> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let w = code.width();
    let cells = code
        .to_colors()
        .into_iter()
        .map(|c| c == qrcode::Color::Dark)
        .collect();
    Some((cells, w))
}

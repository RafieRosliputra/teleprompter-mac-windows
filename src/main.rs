//! JohorStudio Teleprompter — two-screen presidential teleprompter.
//!
//! Main window (operator):  left 2/3 = script (click a line to jump to it),
//!                          right 1/3 = live preview of screen 2 + controls.
//! Screen 2 (prompter):     black background, white text, mirrored for the
//!                          glass, reading line at 1/3 from the top with
//!                          arrows on the left and right.
//! Keys: ← slower / reverse, → faster, Space pause, ↑/↓ nudge, Home = top.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod i18n;
mod draw;
mod markup;
mod net;
mod remote;
mod settings;

use std::sync::{Arc, mpsc};

use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, FontFamily, Galley, Key, Pos2, Rect, Sense,
    Stroke, Vec2, ViewportBuilder, ViewportCommand, ViewportId, pos2, vec2,
};

use draw::{Flip, RowMap};
use markup::{JobParams, Span};
use net::{ListItem, NetMsg};
use settings::{Settings, TextAlign};

const PROMPT_FONT: &str = "prompt";
const ARROW: Color32 = Color32::from_rgb(255, 64, 64);
const SPEED_FACTOR: f32 = 0.12; // px/s per speed level, per px of font size

/// macOS: the user may have "Prefer tabs: Always" — that would merge screen 2
/// into the operator window as a tab. Turn automatic tabbing off for this app.
#[cfg(target_os = "macos")]
fn disable_window_tabbing() {
    use objc2::{class, msg_send, runtime::Bool};
    unsafe {
        let _: () = msg_send![class!(NSWindow), setAllowsAutomaticWindowTabbing: Bool::NO];
    }
}

fn main() -> eframe::Result {
    #[cfg(target_os = "macos")]
    disable_window_tabbing();
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("JohorStudio Teleprompter")
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "JohorStudio Teleprompter",
        options,
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))),
    )
}

fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        "/Library/Fonts/Arial Bold.ttf",
        "C:\\Windows\\Fonts\\arialbd.ttf",
        "/usr/share/fonts/truetype/msttcorefonts/Arial_Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ];
    let mut defs = egui::FontDefinitions::default();
    let mut fam: Vec<String> = Vec::new();
    for p in candidates {
        if let Ok(bytes) = std::fs::read(p) {
            defs.font_data
                .insert("prompt-bold".into(), Arc::new(egui::FontData::from_owned(bytes)));
            fam.push("prompt-bold".into());
            break;
        }
    }
    // Fallbacks (emoji etc.) from the default proportional family.
    if let Some(prop) = defs.families.get(&FontFamily::Proportional) {
        fam.extend(prop.iter().cloned());
    }
    defs.families.insert(FontFamily::Name(PROMPT_FONT.into()), fam);
    ctx.set_fonts(defs);
}

#[derive(Clone)]
struct Disp {
    label: String,
    /// Top-left in logical points (global desktop coordinates).
    pos: Pos2,
    size: Vec2,
    primary: bool,
}

fn list_displays() -> Vec<Disp> {
    match display_info::DisplayInfo::all() {
        Ok(v) if !v.is_empty() => v
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let name = if d.friendly_name.is_empty() { &d.name } else { &d.friendly_name };
                // macOS reports points; Windows/Linux report physical pixels.
                let k = if cfg!(target_os = "macos") { 1.0 } else { d.scale_factor.max(0.5) };
                let pw = if cfg!(target_os = "macos") { d.width as f32 * d.scale_factor } else { d.width as f32 };
                let ph = if cfg!(target_os = "macos") { d.height as f32 * d.scale_factor } else { d.height as f32 };
                Disp {
                    label: format!(
                        "{}: {} ({}×{}){}",
                        i + 1,
                        name,
                        pw.round(),
                        ph.round(),
                        if d.is_primary { i18n::t(" — main", " — utama") } else { "" }
                    ),
                    pos: pos2(d.x as f32 / k, d.y as f32 / k),
                    size: vec2(d.width as f32 / k, d.height as f32 / k),
                    primary: d.is_primary,
                }
            })
            .collect(),
        _ => vec![Disp {
            label: i18n::t("1: Main screen", "1: Skrin utama").into(),
            pos: Pos2::ZERO,
            size: vec2(1920.0, 1080.0),
            primary: true,
        }],
    }
}

/// Smooth ease-in/ease-out move of the scroll position (so the talent's eyes
/// can follow the text instead of it jumping).
#[derive(Clone, Copy)]
struct Glide {
    from: f32,
    to: f32,
    start: f64,
    dur: f64,
}

fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// Window placement steps when moving the prompter between screens /
/// in and out of fullscreen (macOS needs fullscreen exited before a move).
#[derive(Clone, Copy)]
struct MoveJob {
    stage: u8,
    at: f64,
    fullscreen: bool,
    monitor: usize,
}

struct App {
    s: Settings,
    saved_settings: String,
    last_save: f64,

    script: String,
    spans: Vec<Span>,
    script_dirty: bool,

    /// y position (in prompter-galley points) that sits on the reading line.
    scroll: f32,
    glide: Option<Glide>,
    speed_level: i32,
    paused: bool,

    prompter_open: bool,
    prompter_size: Vec2,
    prompter_fullscreen_applied: Option<(bool, usize)>,
    move_job: Option<MoveJob>,

    main_offset: f32,
    main_target: f32,
    main_glide: Option<Glide>,
    follow: bool,
    edit_mode: bool,

    displays: Vec<Disp>,
    code_input: String,
    list: Vec<ListItem>,
    confirm_delete: Option<String>,
    status: String,
    loaded_title: String,
    tx: mpsc::Sender<NetMsg>,
    rx: mpsc::Receiver<NetMsg>,

    // phone remote
    remote: remote::Handle,
    /// Local link (Tailscale / Wi-Fi) and its QR.
    local_url: String,
    local_qr: Option<(Vec<bool>, usize)>,
    /// Internet (relay) link and its QR.
    relay_link: String,
    relay_qr: Option<(Vec<bool>, usize)>,
    qr_internet: bool,
    layout_key: u64,
    layout_ver: u64,
    scripts_ver: u64,
    /// Code of the script loaded from the server (for "reload").
    loaded_code: Option<String>,
    /// Scroll to restore after a reload of the same script.
    keep_scroll: Option<f32>,
}

const SAMPLE_EN: &str = "Welcome to JohorStudio Teleprompter.\n\n\
Paste your script with **Edit script**, open a .txt file, \
or load a script from tele.johorstudio.com with its 6-digit code.\n\n\
[CUE: smile at the camera]\n\n\
Press the right arrow to start and go faster, the left arrow to slow down or go back. \
Press Space to play at the default speed or pause.\n\n\
Click any line on the left to bring it to the reading line on screen 2.";

const SAMPLE: &str = "Selamat datang ke JohorStudio Teleprompter.\n\n\
Tampal skrip anda dengan butang **Edit skrip**, buka fail .txt, \
atau muat skrip dari tele.johorstudio.com menggunakan kod 6 digit.\n\n\
[CUE: senyum ke kamera]\n\n\
Tekan anak panah kanan untuk mula dan laju, anak panah kiri untuk perlahan atau undur. \
Tekan Space untuk berhenti seketika.\n\n\
Klik mana-mana baris di sebelah kiri untuk lompat ke baris itu di skrin kedua.";

impl App {
    fn new(ctx: &egui::Context) -> Self {
        install_fonts(ctx);
        ctx.set_visuals(egui::Visuals::dark());
        let mut s = Settings::load();
        i18n::set_lang(&s.lang);
        let script = settings::load_script()
            .unwrap_or_else(|| (if i18n::ms() { SAMPLE } else { SAMPLE_EN }).to_string());
        let (tx, rx) = mpsc::channel();
        let displays = list_displays();
        if s.monitor >= displays.len() || (displays.len() > 1 && displays[s.monitor].primary) {
            // Default to the first screen that isn't the main one.
            s.monitor = displays.iter().position(|d| !d.primary).unwrap_or(0);
        }
        let saved_settings = serde_json::to_string(&s).unwrap_or_default();
        let psize = displays.get(s.monitor).map(|d| d.size).unwrap_or(vec2(1920.0, 1080.0));
        let local_url = format!(
            "http://{}:{}",
            remote::best_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "localhost".into()),
            remote::HTTP_PORT
        );
        let mut app = Self {
            spans: markup::parse(&script),
            script,
            script_dirty: false,
            s,
            saved_settings,
            last_save: 0.0,
            scroll: 0.0,
            glide: None,
            speed_level: 0,
            paused: false,
            prompter_open: true,
            prompter_size: psize,
            prompter_fullscreen_applied: None,
            move_job: None,
            main_offset: 0.0,
            main_target: f32::NAN,
            main_glide: None,
            follow: true,
            edit_mode: false,
            displays,
            code_input: String::new(),
            list: Vec::new(),
            confirm_delete: None,
            status: String::new(),
            loaded_title: String::new(),
            tx,
            rx,
            remote: remote::start(),
            local_qr: remote::qr(&local_url),
            local_url,
            relay_link: String::new(),
            relay_qr: None,
            qr_internet: true,
            layout_key: 0,
            layout_ver: 0,
            scripts_ver: 0,
            loaded_code: None,
            keep_scroll: None,
        };
        app.set_relay();
        app
    }

    // ------------------------------------------------------------ phone remote

    /// (Re)point the internet relay at this session.
    fn set_relay(&mut self) {
        if self.s.remote_session.len() < 16 {
            self.s.remote_session = remote::new_session();
        }
        let base = self.s.server_url.trim_end_matches('/').to_string();
        self.relay_link = format!("{base}/r/{}", self.s.remote_session);
        self.relay_qr = remote::qr(&self.relay_link);
        let ws = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        let mut g = self.remote.lock().unwrap();
        g.relay_url = format!("{ws}/relay/mac/{}", self.s.remote_session);
        g.relay_gen += 1;
    }

    fn remote_cmds(&mut self, ctx: &egui::Context, pmap: &RowMap, line_h: f32) {
        let cmds = {
            let mut g = self.remote.lock().unwrap();
            if g.ctx.is_none() {
                g.ctx = Some(ctx.clone());
            }
            std::mem::take(&mut g.cmds)
        };
        let now = ctx.input(|i| i.time);
        for c in cmds {
            let base = self.glide.map(|g| g.to).unwrap_or(self.scroll);
            match c {
                remote::Cmd::Toggle => self.toggle_play(),
                remote::Cmd::Slower => {
                    self.speed_level = (self.speed_level - 1).max(-20);
                    self.paused = false;
                }
                remote::Cmd::Faster => {
                    self.speed_level = (self.speed_level + 1).min(40);
                    self.paused = false;
                }
                remote::Cmd::Stop => self.speed_level = 0,
                remote::Cmd::Top => {
                    self.glide_to(0.0, now);
                    self.speed_level = 0;
                }
                remote::Cmd::LineUp => self.glide_to(base - line_h, now),
                remote::Cmd::LineDown => self.glide_to(base + line_h, now),
                remote::Cmd::DefaultSpeed { v } => self.s.default_speed = v.clamp(1, 20),
                remote::Cmd::Font { delta } => {
                    self.s.font_size = (self.s.font_size + delta).clamp(30.0, 200.0)
                }
                remote::Cmd::ScrollTo { y } => {
                    if y.is_finite() {
                        self.glide = None;
                        self.scroll = y;
                        self.follow = true;
                    }
                }
                remote::Cmd::Jump { y } => {
                    if y.is_finite() {
                        self.glide_to(pmap.row_center_at_y(y), now);
                        self.follow = true;
                    }
                }
                remote::Cmd::ListScripts => {
                    self.status = i18n::t("Phone asked for the script list…", "Telefon minta senarai skrip…").into();
                    net::fetch_list(
                        self.s.server_url.clone(),
                        self.s.admin_token.clone(),
                        self.tx.clone(),
                        ctx.clone(),
                    );
                }
                remote::Cmd::LoadScript { code } => {
                    if code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()) {
                        self.code_input = code.clone();
                        self.load_code(code, ctx);
                    }
                }
                remote::Cmd::ReloadScript => {
                    if let Some(code) = self.loaded_code.clone() {
                        self.keep_scroll = Some(self.scroll);
                        self.load_code(code, ctx);
                    } else {
                        self.publish_scripts(None, Some(i18n::t("No server script to reload", "Tiada skrip dari server untuk dimuat semula")));
                    }
                }
            }
        }
    }

    /// Hand the phone the script list (or an error message).
    fn publish_scripts(&mut self, _ok: Option<()>, err: Option<&str>) {
        let items: Vec<serde_json::Value> = self
            .list
            .iter()
            .map(|it| {
                serde_json::json!({"code": it.code, "title": it.title, "name": it.name,
                    "chars": it.chars, "updated": it.updated})
            })
            .collect();
        let body = serde_json::json!({"items": items, "error": err}).to_string();
        self.scripts_ver += 1;
        let msg = format!(r#"{{"t":"scripts",{}"#, &body[1..]);
        let mut g = self.remote.lock().unwrap();
        g.scripts = msg;
        g.scripts_ver = self.scripts_ver;
    }

    /// Hand the phone the screen-2 rows (when they change) and the live state.
    fn remote_publish(&mut self, pgal: &Galley, pmap: &RowMap) {
        use std::hash::{Hash, Hasher};
        let mut hs = std::collections::hash_map::DefaultHasher::new();
        self.script.hash(&mut hs);
        for v in [
            self.s.font_size,
            self.s.line_spacing,
            self.s.margin_left,
            self.s.margin_right,
            self.s.read_line,
            self.prompter_size.x,
            self.prompter_size.y,
        ] {
            v.to_bits().hash(&mut hs);
        }
        (self.s.align as u8).hash(&mut hs);
        let key = hs.finish();
        let mut layout = None;
        if key != self.layout_key {
            self.layout_key = key;
            self.layout_ver += 1;
            layout = Some(self.layout_json(pgal));
        }
        let (row, rows) = pmap.row_index(self.scroll);
        let state = format!(
            r#"{{"t":"state","ver":{},"y":{:.1},"spd":{},"paused":{},"def":{},"fs":{:.0},"row":{},"rows":{},"code":{},"title":{}}}"#,
            self.layout_ver,
            self.scroll,
            self.speed_level,
            self.paused,
            self.s.default_speed,
            self.s.font_size,
            row + 1,
            rows,
            serde_json::to_string(&self.loaded_code).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&self.loaded_title).unwrap_or_else(|_| "\"\"".into()),
        );
        let mut g = self.remote.lock().unwrap();
        if let Some(l) = layout {
            g.layout = l;
            g.layout_ver = self.layout_ver;
        }
        if g.state != state {
            g.state = state;
        }
    }

    fn layout_json(&self, g: &Galley) -> String {
        let text = &g.job.text;
        let byte_of: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        let color_at = |ci: usize| -> Color32 {
            let b = byte_of.get(ci).copied().unwrap_or(usize::MAX);
            g.job
                .sections
                .iter()
                .find(|s| b >= s.byte_range.start.0 && b < s.byte_range.end.0)
                .map(|s| s.format.color)
                .unwrap_or(Color32::WHITE)
        };
        let mut rows = Vec::with_capacity(g.rows.len());
        let mut ci = 0usize;
        for r in &g.rows {
            let mut segs: Vec<(String, Color32)> = Vec::new();
            for gl in &r.row.glyphs {
                let col = color_at(ci);
                match segs.last_mut() {
                    Some((s, c)) if *c == col => s.push(gl.chr),
                    _ => segs.push((gl.chr.to_string(), col)),
                }
                ci += 1;
            }
            if r.ends_with_newline {
                ci += 1;
            }
            let s: Vec<serde_json::Value> = segs
                .into_iter()
                .map(|(t, c)| serde_json::json!([t, format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())]))
                .collect();
            rows.push(serde_json::json!({"y": r.pos.y, "h": r.row.size.y, "s": s}));
        }
        let align = match self.s.align {
            TextAlign::Left => "left",
            TextAlign::Center => "center",
            TextAlign::Right => "right",
        };
        let body = serde_json::json!({
            "w": self.prompter_size.x, "h": self.prompter_size.y,
            "read": self.s.read_line, "ml": self.s.margin_left, "mr": self.s.margin_right,
            "align": align, "fs": self.s.font_size, "rows": rows,
        })
        .to_string();
        // "t" first: the relay recognises the message type from its start.
        format!(r#"{{"t":"layout","ver":{},{}"#, self.layout_ver, &body[1..])
    }

    /// QR card, bottom right of the main window.
    fn qr_card(&mut self, ctx: &egui::Context) {
        if !self.s.show_qr {
            return;
        }
        let (relay_ok, phones, err) = {
            let g = self.remote.lock().unwrap();
            (g.relay_ok, g.local_clients + g.relay_clients, g.error.clone())
        };
        #[allow(deprecated)]
        egui::Area::new(egui::Id::new("remote-qr"))
            .anchor(egui::Align2::RIGHT_BOTTOM, vec2(-14.0, -14.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_rgb(23, 23, 27))
                    .inner_margin(12.0)
                    .show(ui, |ui| {
                        ui.set_width(250.0);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(i18n::t("📱 Phone remote", "📱 Remote telefon")).strong());
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("✖").on_hover_text(i18n::t("Hide (show again: 📱 Remote at the top)", "Sembunyi (buka semula: 📱 Remote di atas)")).clicked() {
                                    self.s.show_qr = false;
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.qr_internet, true, "Internet");
                            ui.selectable_value(&mut self.qr_internet, false, i18n::t("Local", "Tempatan"));
                        });
                        let (link, qr) = if self.qr_internet {
                            (self.relay_link.clone(), self.relay_qr.as_ref())
                        } else {
                            (self.local_url.clone(), self.local_qr.as_ref())
                        };
                        let side = 170.0;
                        let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
                        let p = ui.painter_at(rect);
                        p.rect_filled(rect, CornerRadius::same(6), Color32::WHITE);
                        if let Some((cells, w)) = qr {
                            let n = *w as f32 + 4.0; // quiet zone of 2 modules
                            let m = side / n;
                            for (i, dark) in cells.iter().enumerate() {
                                if *dark {
                                    let (x, y) = ((i % w) as f32 + 2.0, (i / w) as f32 + 2.0);
                                    p.rect_filled(
                                        Rect::from_min_size(rect.min + vec2(x * m, y * m), vec2(m + 0.3, m + 0.3)),
                                        CornerRadius::ZERO,
                                        Color32::BLACK,
                                    );
                                }
                            }
                        }
                        ui.add(egui::Label::new(egui::RichText::new(&link).small().color(Color32::from_rgb(245, 197, 66))).wrap());
                        let (dot, msg) = if self.qr_internet {
                            if relay_ok {
                                (Color32::from_rgb(90, 208, 138), i18n::t("Internet relay active", "Relay internet aktif").to_string())
                            } else {
                                (Color32::from_rgb(255, 107, 107), i18n::t("Relay not connected (needs internet)", "Relay belum bersambung (perlu internet)").to_string())
                            }
                        } else if let Some(e) = &err {
                            (Color32::from_rgb(255, 107, 107), e.clone())
                        } else {
                            (Color32::from_rgb(90, 208, 138), i18n::t("Tailscale / same Wi-Fi", "Tailscale / WiFi sama").to_string())
                        };
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 4.0, dot);
                            ui.small(msg);
                        });
                        ui.small(tr!("{phones} phone(s) connected", "{phones} telefon bersambung"));
                        if self.qr_internet
                            && ui
                                .small_button(i18n::t("⟳ New QR", "⟳ Jana QR baru"))
                                .on_hover_text(i18n::t("Old phones are disconnected; scan the new QR", "Telefon lama terputus; imbas QR baru"))
                                .clicked()
                        {
                            self.s.remote_session = remote::new_session();
                            self.set_relay();
                        }
                    });
            });
    }

    /// EN / BM switch, top right.
    fn lang_switch(&mut self, ui: &mut egui::Ui) {
        for (code, label) in [("ms", "BM"), ("en", "EN")] {
            let on = self.s.lang == code;
            if ui.selectable_label(on, label).on_hover_text(if code == "en" { "English" } else { "Bahasa Melayu" }).clicked() && !on {
                self.s.lang = code.into();
                i18n::set_lang(code);
            }
        }
        ui.label("🌐");
    }

    fn align(&self) -> Align {
        match self.s.align {
            TextAlign::Left => Align::Min,
            TextAlign::Center => Align::Center,
            TextAlign::Right => Align::Max,
        }
    }

    /// Text area (left, right) on a screen starting at `left` with width `w`.
    fn text_lr(&self, left: f32, w: f32) -> (f32, f32) {
        (left + w * self.s.margin_left, left + w * (1.0 - self.s.margin_right))
    }

    fn prompter_job(&self, scale: f32) -> JobParams {
        let w = self.prompter_size.x;
        let (l, r) = self.text_lr(0.0, w);
        JobParams {
            family: FontFamily::Name(PROMPT_FONT.into()),
            size: self.s.font_size * scale,
            line_spacing: self.s.line_spacing,
            wrap_width: (r - l) * scale,
            align: self.align(),
        }
    }

    fn layout(ctx: &egui::Context, spans: &[Span], p: &JobParams) -> Arc<Galley> {
        let job = markup::job(spans, p);
        ctx.fonts_mut(|f| f.layout_job(job))
    }

    fn speed_px(&self) -> f32 {
        if self.paused {
            0.0
        } else {
            self.speed_level as f32 * self.s.font_size * SPEED_FACTOR
        }
    }

    fn set_script(&mut self, text: String, title: String) {
        self.script = text;
        self.spans = markup::parse(&self.script);
        self.script_dirty = true;
        self.loaded_title = title;
        self.scroll = 0.0; // clamped to the first row centre in ui()
        self.glide = None;
        self.speed_level = 0;
        self.follow = true;
    }

    /// Ease the reading position to `target` instead of jumping there.
    fn glide_to(&mut self, target: f32, now: f64) {
        // Continue from wherever an in-flight glide currently is.
        let from = self.scroll;
        let to = target;
        let dist = (to - from).abs();
        if dist < 0.5 {
            self.glide = None;
            return;
        }
        let dur = (0.45 + (dist as f64 / 2500.0)).min(1.1);
        self.glide = Some(Glide { from, to, start: now, dur });
    }

    /// Space / play button: if scrolling → pause; otherwise start at the
    /// default speed.
    fn toggle_play(&mut self) {
        if !self.paused && self.speed_level != 0 {
            self.paused = true;
        } else {
            self.speed_level = self.s.default_speed;
            self.paused = false;
        }
    }

    fn is_running(&self) -> bool {
        !self.paused && self.speed_level != 0
    }

    fn handle_keys(&mut self, ctx: &egui::Context, line_h: f32) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (right, left, space, down, up, home, esc, now) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::Space),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::Escape),
                i.time,
            )
        });
        if right {
            self.speed_level = (self.speed_level + 1).min(40);
            self.paused = false;
        }
        if left {
            self.speed_level = (self.speed_level - 1).max(-20);
            self.paused = false;
        }
        if space {
            self.toggle_play();
        }
        let base = self.glide.map(|g| g.to).unwrap_or(self.scroll);
        if down {
            self.glide_to(base + line_h, now);
        }
        if up {
            self.glide_to(base - line_h, now);
        }
        if home {
            self.glide_to(0.0, now);
            self.speed_level = 0;
        }
        if esc {
            self.speed_level = 0;
        }
    }

    fn poll_net(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                NetMsg::Script(Ok(sc)) => {
                    self.status = tr!("Loaded: {} ({}) — code {}", "Dimuat: {} ({}) — kod {}", sc.title, sc.name, sc.code);
                    let t = format!("{} — {}", sc.code, sc.title);
                    let same = self.loaded_code.as_deref() == Some(sc.code.as_str());
                    let keep = self.keep_scroll.take().filter(|_| same);
                    self.loaded_code = Some(sc.code.clone());
                    self.set_script(sc.content, t);
                    if let Some(y) = keep {
                        self.scroll = y;
                    }
                }
                NetMsg::Script(Err(e)) => self.status = tr!("Could not load script: {e}", "Gagal muat skrip: {e}"),
                NetMsg::List(Ok(l)) => {
                    self.status = tr!("{} scripts on the server", "{} skrip di server", l.len());
                    self.list = l;
                    self.publish_scripts(Some(()), None);
                }
                NetMsg::List(Err(e)) => {
                    self.status = tr!("Could not load the list: {e}", "Gagal senarai: {e}");
                    self.publish_scripts(None, Some(&e));
                }
                NetMsg::Deleted(code, Ok(())) => {
                    self.list.retain(|it| it.code != code);
                    self.status = tr!("Script {code} deleted", "Skrip {code} dipadam");
                    self.publish_scripts(Some(()), None);
                }
                NetMsg::Deleted(code, Err(e)) => self.status = tr!("Could not delete {code}: {e}", "Gagal padam {code}: {e}"),
            }
        }
    }

    fn autosave(&mut self, now: f64) {
        if now - self.last_save < 2.0 {
            return;
        }
        self.last_save = now;
        let cur = serde_json::to_string(&self.s).unwrap_or_default();
        if cur != self.saved_settings {
            self.s.save();
            self.saved_settings = cur;
        }
        if self.script_dirty {
            settings::save_script(&self.script);
            self.script_dirty = false;
        }
    }

    // ------------------------------------------------------------ prompter

    /// Fullscreen only makes sense with a second display attached; with a
    /// single screen the prompter opens as a normal window.
    fn fullscreen(&self) -> bool {
        self.s.fullscreen && self.displays.len() > 1
    }

    fn display(&self) -> Disp {
        self.displays
            .get(self.s.monitor)
            .or(self.displays.first())
            .cloned()
            .unwrap()
    }

    fn prompter_builder(&self) -> ViewportBuilder {
        let d = self.display();
        ViewportBuilder::default()
            .with_title(i18n::t("Teleprompter — Screen 2", "Teleprompter — Skrin 2"))
            .with_position(d.pos + vec2(30.0, 30.0))
            .with_inner_size([960.0, 540.0])
    }

    fn show_prompter(&mut self, ctx: &egui::Context) {
        let id = ViewportId::from_hash_of("prompter-screen");
        let now = ctx.input(|i| i.time);
        let fs = self.fullscreen();
        let want = (fs, self.s.monitor);
        match self.prompter_fullscreen_applied {
            None => {
                // Just opened: window is created on the chosen screen, then go fullscreen.
                self.move_job = Some(MoveJob { stage: 2, at: now + 0.4, fullscreen: fs, monitor: want.1 });
            }
            Some(prev) if prev != want => {
                self.move_job = Some(MoveJob { stage: 0, at: now, fullscreen: fs, monitor: want.1 });
            }
            _ => {}
        }
        self.prompter_fullscreen_applied = Some(want);
        if let Some(mut j) = self.move_job {
            if now >= j.at {
                match j.stage {
                    0 => {
                        ctx.send_viewport_cmd_to(id, ViewportCommand::Fullscreen(false));
                        j.stage = 1;
                        j.at = now + 1.0;
                    }
                    1 => {
                        let d = self.displays.get(j.monitor).cloned().unwrap_or_else(|| self.display());
                        ctx.send_viewport_cmd_to(id, ViewportCommand::OuterPosition(d.pos + vec2(30.0, 30.0)));
                        ctx.send_viewport_cmd_to(id, ViewportCommand::InnerSize(vec2(960.0, 540.0)));
                        j.stage = 2;
                        j.at = now + 0.4;
                    }
                    _ => {
                        if j.fullscreen {
                            ctx.send_viewport_cmd_to(id, ViewportCommand::Fullscreen(true));
                        }
                        j.stage = 3;
                    }
                }
            }
            self.move_job = if j.stage >= 3 { None } else { Some(j) };
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let builder = self.prompter_builder();
        ctx.show_viewport_immediate(id, builder, |ui, _class| {
            let pctx = ui.ctx().clone();
            let rect = pctx.content_rect();
            if rect.width() > 50.0 && rect.height() > 50.0 {
                self.prompter_size = rect.size();
            }
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, CornerRadius::ZERO, Color32::BLACK);

            let p = self.prompter_job(1.0);
            let galley = Self::layout(&pctx, &self.spans, &p);
            let (l, r) = self.text_lr(rect.left(), rect.width());
            let ax = draw::anchor_x(self.align(), l, r);
            let read_y = rect.top() + rect.height() * self.s.read_line;
            let pos = pos2(ax, read_y - self.scroll);
            let flip = Flip { h: self.s.flip_h, v: self.s.flip_v, area: rect };
            let tex = pctx.fonts(|f| f.font_image_size());
            draw::paint_galley(&painter, &galley, pos, flip, tex, Color32::WHITE);
            if self.s.show_arrows {
                draw::reading_arrows(&painter, rect, read_y, self.s.arrow_size, ARROW, flip);
            }

            let line_h = self.s.font_size * self.s.line_spacing;
            self.handle_keys(&pctx, line_h);
            if ui.rect_contains_pointer(rect) {
                pctx.set_cursor_icon(CursorIcon::None);
            }
            if pctx.input(|i| i.viewport().close_requested()) {
                self.prompter_open = false;
            }
            // Double-click toggles fullscreen.
            if pctx.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary)) {
                self.s.fullscreen = !self.s.fullscreen;
            }
        });
    }

    // ------------------------------------------------------------ main window

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.heading("Teleprompter");
            ui.separator();
            let state = if self.paused {
                i18n::t("⏸ Paused", "⏸ Berhenti").to_string()
            } else if self.speed_level == 0 {
                i18n::t("⏹ Stopped", "⏹ Diam").to_string()
            } else if self.speed_level > 0 {
                tr!("▶ Speed {}x", "▶ Laju {}x", self.speed_level)
            } else {
                tr!("◀ Reverse {}x", "◀ Undur {}x", -self.speed_level)
            };
            // Fixed-width box so the buttons beside it never shift.
            let (r, _) = ui.allocate_exact_size(vec2(150.0, 26.0), Sense::hover());
            ui.painter().rect_filled(r, CornerRadius::same(4), Color32::from_gray(28));
            ui.painter().text(
                r.left_center() + vec2(10.0, 0.0),
                egui::Align2::LEFT_CENTER,
                state,
                egui::FontId::proportional(16.0),
                Color32::WHITE,
            );
            egui::ComboBox::from_id_salt("default_speed")
                .selected_text(format!("{}x", self.s.default_speed))
                .width(60.0)
                .show_ui(ui, |ui| {
                    for v in 1..=20 {
                        ui.selectable_value(&mut self.s.default_speed, v, format!("{v}x"));
                    }
                })
                .response
                .on_hover_text(i18n::t("Speed when you press Space", "Laju bila tekan Space"));
            if ui.button("◀").on_hover_text(i18n::t("Slower / reverse (←)", "Perlahan / undur (←)")).clicked() {
                self.speed_level = (self.speed_level - 1).max(-20);
            }
            let play = if self.is_running() { "⏸" } else { "▶" };
            if ui.button(play).on_hover_text(i18n::t("Space — play at the default speed / pause", "Space — main pada laju default / henti")).clicked() {
                self.toggle_play();
            }
            if ui.button("▶▶").on_hover_text(i18n::t("Faster (→)", "Laju (→)")).clicked() {
                self.speed_level = (self.speed_level + 1).min(40);
                self.paused = false;
            }
            if ui.button("⏹ Stop").clicked() {
                self.speed_level = 0;
            }
            if ui.button(i18n::t("⏮ Top", "⏮ Atas")).clicked() {
                let now = ui.input(|i| i.time);
                self.glide_to(0.0, now);
                self.speed_level = 0;
            }
            ui.separator();
            ui.toggle_value(&mut self.s.flip_h, "⇋ Flip H");
            ui.toggle_value(&mut self.s.flip_v, "⇅ Flip V");
            ui.separator();
            let label = if self.prompter_open { i18n::t("Close Screen 2", "Tutup Skrin 2") } else { i18n::t("Open Screen 2", "Buka Skrin 2") };
            if ui.button(label).clicked() {
                self.prompter_open = !self.prompter_open;
                self.prompter_fullscreen_applied = None;
            }
            ui.toggle_value(&mut self.s.fullscreen, i18n::t("⛶ Fullscreen", "⛶ Skrin penuh"));
            let cur = self.displays.get(self.s.monitor).map(|d| d.label.clone()).unwrap_or_default();
            egui::ComboBox::from_id_salt("monitor")
                .selected_text(cur)
                .width(220.0)
                .show_ui(ui, |ui| {
                    for (i, d) in self.displays.iter().enumerate() {
                        ui.selectable_value(&mut self.s.monitor, i, &d.label);
                    }
                });
            if ui.small_button("⟳").on_hover_text(i18n::t("Rescan displays", "Imbas semula skrin")).clicked() {
                self.displays = list_displays();
                if self.s.monitor >= self.displays.len() {
                    self.s.monitor = self.displays.len() - 1;
                }
            }
            if self.displays.len() < 2 {
                ui.label(egui::RichText::new(i18n::t("(1 display only — Screen 2 in a window)", "(1 skrin sahaja — Skrin 2 dalam tetingkap)")).weak());
            }
            ui.separator();
            ui.toggle_value(&mut self.edit_mode, i18n::t("✏ Edit script", "✏ Edit skrip"));
            ui.toggle_value(&mut self.s.match_main, i18n::t("⧉ Duplicate Screen 2", "⧉ Duplicate Skrin 2"))
                .on_hover_text(i18n::t("Left pane follows Screen 2 exactly: same lines, margins and motion", "Window kiri ikut Skrin 2 sebiji: baris, margin & gerakan sama"));
            ui.toggle_value(&mut self.s.show_qr, "📱 Remote")
                .on_hover_text(i18n::t("Show a QR code to control the prompter from a phone", "Tunjuk QR untuk kawal dari telefon"));
            if ui.button(i18n::t("📂 Open file", "📂 Buka fail")).clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Teks", &["txt", "md"])
                    .pick_file()
                {
                    match std::fs::read_to_string(&path) {
                        Ok(t) => {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default();
                            self.status = tr!("Opened file: {name}", "Fail dibuka: {name}");
                            self.loaded_code = None;
                            self.set_script(t, name);
                        }
                        Err(e) => self.status = tr!("Could not open the file: {e}", "Gagal buka fail: {e}"),
                    }
                }
            }
            if !self.follow && ui.button(i18n::t("🎯 Follow position", "🎯 Ikut posisi")).clicked() {
                self.follow = true;
            }
        });
    }

    fn script_view(&mut self, ui: &mut egui::Ui, pmap: &RowMap) {
        if self.edit_mode {
            ui.label(i18n::t("Edit script — **bold**  ·  [CUE: direction]   (press ✏ Edit script again to finish)", "Edit skrip — **tebal**  ·  [CUE: arahan]   (tekan ✏ Edit skrip sekali lagi untuk selesai)"));
            egui::ScrollArea::vertical().show(ui, |ui| {
                let r = ui.add(
                    egui::TextEdit::multiline(&mut self.script)
                        .font(egui::FontId::proportional(self.s.main_font_size))
                        .desired_width(f32::INFINITY)
                        .desired_rows(30),
                );
                if r.changed() {
                    self.spans = markup::parse(&self.script);
                    self.script_dirty = true;
                }
            });
            return;
        }

        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(6), Color32::from_gray(12));

        let pad = 44.0;
        let (p, anchor_x) = if self.s.match_main {
            // Same layout as screen 2, scaled to this panel's width.
            let scale = rect.width() / self.prompter_size.x.max(1.0);
            let (l, r) = self.text_lr(rect.left(), rect.width());
            (self.prompter_job(scale), draw::anchor_x(self.align(), l, r))
        } else {
            (
                JobParams {
                    family: FontFamily::Name(PROMPT_FONT.into()),
                    size: self.s.main_font_size,
                    line_spacing: 1.35,
                    wrap_width: (rect.width() - pad * 2.0).max(50.0),
                    align: Align::Min,
                },
                rect.left() + pad,
            )
        };
        let galley = Self::layout(ui.ctx(), &self.spans, &p);
        let mmap = RowMap::new(&galley);

        let (c, f) = pmap.y_to_pos(self.scroll);
        let cur_y = mmap.pos_to_y(c, f);

        // Free scrolling with the wheel / trackpad.
        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                self.main_offset -= dy;
                self.follow = false;
            }
        }
        let (by, bh) = mmap.row_band(cur_y);
        let lo = -rect.height() * 0.9;
        let hi = (mmap.height - rect.height() * 0.1).max(0.0);
        if self.follow {
            if self.s.match_main {
                // Duplicate mode: identical rows, so follow screen 2 continuously.
                self.main_glide = None;
                self.main_offset = cur_y - rect.height() * self.s.read_line;
            } else {
                // Hold the current row still on the reading line; when screen 2
                // reaches the next row here, ease (in/out) to it.
                let target = (by + bh * 0.5 - rect.height() * self.s.read_line).clamp(lo, hi);
                let now = ui.input(|i| i.time);
                if self.main_target.is_nan() || (target - self.main_target).abs() > 0.5 {
                    self.main_target = target;
                    let dist = (target - self.main_offset).abs();
                    self.main_glide = (dist > 0.5).then(|| Glide {
                        from: self.main_offset,
                        to: target,
                        start: now,
                        dur: (0.3 + dist as f64 / 3000.0).min(0.8),
                    });
                }
                if let Some(g) = self.main_glide {
                    let t = ((now - g.start) / g.dur) as f32;
                    self.main_offset = g.from + (g.to - g.from) * ease_in_out(t);
                    if t >= 1.0 {
                        self.main_glide = None;
                    } else {
                        ui.ctx().request_repaint();
                    }
                } else {
                    self.main_offset = target;
                }
            }
        } else {
            self.main_glide = None;
            self.main_target = f32::NAN;
        }
        self.main_offset = self.main_offset.clamp(lo, hi);

        let top = rect.top() - self.main_offset;
        let gpos = pos2(anchor_x, top);

        // Current line band.
        painter.rect_filled(
            Rect::from_min_size(pos2(rect.left(), top + by), vec2(rect.width(), bh)),
            CornerRadius::ZERO,
            Color32::from_rgba_unmultiplied(255, 64, 64, 28),
        );
        // Hover band.
        if let Some(hp) = resp.hover_pos() {
            let (hy, hh) = mmap.row_band(hp.y - top);
            painter.rect_stroke(
                Rect::from_min_size(pos2(rect.left() + 4.0, top + hy), vec2(rect.width() - 8.0, hh)),
                CornerRadius::same(4),
                Stroke::new(1.0, Color32::from_gray(90)),
                egui::StrokeKind::Inside,
            );
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }

        painter.galley(gpos, galley.clone(), Color32::WHITE);
        let arrow_y = if self.s.match_main { top + cur_y } else { top + by + bh * 0.5 };
        draw::reading_arrows(&painter, rect, arrow_y, 16.0, ARROW, Flip::none(rect));

        if resp.clicked() {
            if let Some(pp) = resp.interact_pointer_pos() {
                let cc = galley.cursor_from_pos(pp - gpos);
                let now = ui.input(|i| i.time);
                self.glide_to(pmap.row_center_of_char(cc.index.0), now);
                self.follow = true;
            }
        }
    }

    fn preview(&mut self, ui: &mut egui::Ui, pmap: &RowMap) {
        let w = ui.available_width();
        let aspect = self.prompter_size.y / self.prompter_size.x.max(1.0);
        let (rect, resp) = ui.allocate_exact_size(vec2(w, w * aspect), Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::ZERO, Color32::BLACK);
        let scale = rect.width() / self.prompter_size.x.max(1.0);

        let p = self.prompter_job(scale);
        let galley = Self::layout(ui.ctx(), &self.spans, &p);
        let vmap = RowMap::new(&galley);
        let (c, f) = pmap.y_to_pos(self.scroll);
        let vy = vmap.pos_to_y(c, f);
        let (l, r) = self.text_lr(rect.left(), rect.width());
        let ax = draw::anchor_x(self.align(), l, r);
        let read_y = rect.top() + rect.height() * self.s.read_line;
        let gpos = pos2(ax, read_y - vy);
        let flip = Flip { h: self.s.flip_h, v: self.s.flip_v, area: rect };
        let tex = ui.ctx().fonts(|f| f.font_image_size());
        draw::paint_galley(&painter, &galley, gpos, flip, tex, Color32::WHITE);
        if self.s.show_arrows {
            let sz = (self.s.arrow_size * scale).max(5.0);
            draw::reading_arrows(&painter, rect, read_y, sz, ARROW, flip);
        }
        // Margin guides.
        let guide = Stroke::new(1.0, Color32::from_rgba_unmultiplied(80, 160, 255, 70));
        for x in [l, r] {
            let a = flip.apply(pos2(x, rect.top()));
            let b = flip.apply(pos2(x, rect.bottom()));
            painter.line_segment([a, b], guide);
        }
        painter.rect_stroke(
            rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_gray(70)),
            egui::StrokeKind::Outside,
        );

        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                let d = if self.s.flip_v { dy } else { -dy };
                self.glide = None;
                self.scroll += d / scale;
            }
        }
        if resp.clicked() {
            if let Some(pp) = resp.interact_pointer_pos() {
                let up: Pos2 = flip.apply(pp);
                let gy = up.y - gpos.y;
                let (ch, _) = vmap.y_to_pos(gy);
                let now = ui.input(|i| i.time);
                self.glide_to(pmap.row_center_of_char(ch), now);
            }
        }
    }

    fn load_code(&mut self, code: String, ctx: &egui::Context) {
        self.status = tr!("Loading {code}…", "Memuat {code}…");
        net::fetch_script(
            self.s.server_url.clone(),
            self.s.admin_token.clone(),
            code,
            self.tx.clone(),
            ctx.clone(),
        );
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        egui::CollapsingHeader::new(i18n::t("Layout & alignment", "Susun atur & penjajaran"))
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(i18n::t("Align:", "Jajar:"));
                    ui.selectable_value(&mut self.s.align, TextAlign::Left, i18n::t("Left", "Kiri"));
                    ui.selectable_value(&mut self.s.align, TextAlign::Center, i18n::t("Centre", "Tengah"));
                    ui.selectable_value(&mut self.s.align, TextAlign::Right, i18n::t("Right", "Kanan"));
                });
                ui.add(egui::Slider::new(&mut self.s.font_size, 30.0..=200.0).text(i18n::t("Font size", "Saiz huruf")));
                ui.add(egui::Slider::new(&mut self.s.line_spacing, 1.0..=2.2).text(i18n::t("Line spacing", "Jarak baris")));
                ui.add(
                    egui::Slider::new(&mut self.s.margin_left, 0.0..=0.4)
                        .text(i18n::t("Left margin", "Margin kiri"))
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                );
                ui.add(
                    egui::Slider::new(&mut self.s.margin_right, 0.0..=0.4)
                        .text(i18n::t("Right margin", "Margin kanan"))
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                );
                ui.add(
                    egui::Slider::new(&mut self.s.read_line, 0.1..=0.9)
                        .text(i18n::t("Reading line", "Garis bacaan"))
                        .custom_formatter(|v, _| tr!("{:.0}% from top", "{:.0}% dari atas", v * 100.0)),
                );
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.s.show_arrows, i18n::t("Left/right cursors", "Kursor kiri/kanan"));
                    ui.add(egui::Slider::new(&mut self.s.arrow_size, 10.0..=120.0).text(i18n::t("size", "saiz")));
                });
                ui.checkbox(&mut self.s.match_main, i18n::t("Match main view to Screen 2", "Samakan paparan utama dengan Skrin 2"))
                    .on_hover_text(i18n::t("The script on the left uses the same lines, margins and alignment as Screen 2", "Skrip di kiri guna baris, margin & jajaran yang sama seperti Skrin 2"));
                ui.add_enabled_ui(!self.s.match_main, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("▼").on_hover_text(i18n::t("Smaller main text", "Kecilkan teks skrin utama")).clicked() {
                            self.s.main_font_size = (self.s.main_font_size - 2.0).max(14.0);
                        }
                        if ui.button("▲").on_hover_text(i18n::t("Larger main text", "Besarkan teks skrin utama")).clicked() {
                            self.s.main_font_size = (self.s.main_font_size + 2.0).min(100.0);
                        }
                        ui.add(
                            egui::Slider::new(&mut self.s.main_font_size, 14.0..=100.0)
                                .text(i18n::t("Main text size", "Saiz teks skrin utama")),
                        );
                    });
                });
                if ui.button(i18n::t("Reset layout", "Set semula susun atur")).clicked() {
                    let d = Settings::default();
                    self.s.font_size = d.font_size;
                    self.s.line_spacing = d.line_spacing;
                    self.s.margin_left = d.margin_left;
                    self.s.margin_right = d.margin_right;
                    self.s.read_line = d.read_line;
                    self.s.align = d.align;
                    self.s.arrow_size = d.arrow_size;
                }
            });

        egui::CollapsingHeader::new(i18n::t("Scripts from tele.johorstudio.com", "Skrip dari tele.johorstudio.com"))
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(i18n::t("Code:", "Kod:"));
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.code_input)
                            .desired_width(90.0)
                            .char_limit(6)
                            .hint_text("123456"),
                    );
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button(i18n::t("Load", "Muat")).clicked() || enter {
                        let code: String =
                            self.code_input.chars().filter(|c| c.is_ascii_digit()).collect();
                        if code.len() == 6 {
                            self.load_code(code, &ui.ctx().clone());
                        } else {
                            self.status = i18n::t("The code must be 6 digits", "Kod mesti 6 digit").into();
                        }
                    }
                    if ui.button(i18n::t("⟳ List", "⟳ Senarai")).clicked() {
                        self.status = i18n::t("Loading the list…", "Memuat senarai…").into();
                        net::fetch_list(
                            self.s.server_url.clone(),
                            self.s.admin_token.clone(),
                            self.tx.clone(),
                            ui.ctx().clone(),
                        );
                    }
                });
                if !self.list.is_empty() {
                    let mut load: Option<String> = None;
                    let mut delete: Option<String> = None;
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        for it in &self.list {
                            let txt = format!(
                                "{}  {}\n{} · {} · {} {} · {}",
                                it.code,
                                it.title,
                                it.name,
                                it.email,
                                it.chars,
                                i18n::t("chars", "aksara"),
                                fmt_age(it.updated)
                            );
                            ui.horizontal(|ui| {
                                let confirming = self.confirm_delete.as_deref() == Some(&it.code);
                                let side = if confirming { 150.0 } else { 34.0 };
                                let b = egui::Button::new(txt)
                                    .wrap()
                                    .min_size(vec2((ui.available_width() - side).max(80.0), 0.0));
                                if ui.add(b).on_hover_text(i18n::t("Click to load this script", "Klik untuk muat skrip")).clicked() {
                                    load = Some(it.code.clone());
                                }
                                if confirming {
                                    let yes = egui::Button::new(
                                        egui::RichText::new(i18n::t("Delete", "Padam")).color(Color32::WHITE),
                                    )
                                    .fill(Color32::from_rgb(190, 40, 40));
                                    if ui.add(yes).clicked() {
                                        delete = Some(it.code.clone());
                                    }
                                    if ui.button(i18n::t("Cancel", "Batal")).clicked() {
                                        self.confirm_delete = None;
                                    }
                                } else if ui.button("🗑").on_hover_text(i18n::t("Delete this script", "Padam skrip ini")).clicked() {
                                    self.confirm_delete = Some(it.code.clone());
                                }
                            });
                        }
                    });
                    if let Some(code) = delete {
                        self.confirm_delete = None;
                        self.status = tr!("Deleting {code}…", "Memadam {code}…");
                        net::delete_script(
                            self.s.server_url.clone(),
                            self.s.admin_token.clone(),
                            code,
                            self.tx.clone(),
                            ui.ctx().clone(),
                        );
                    }
                    if let Some(code) = load {
                        self.code_input = code.clone();
                        self.load_code(code, &ui.ctx().clone());
                    }
                }
            });

        egui::CollapsingHeader::new(i18n::t("Server settings", "Tetapan server")).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Server:");
                ui.text_edit_singleline(&mut self.s.server_url);
            });
            ui.horizontal(|ui| {
                ui.label(i18n::t("Admin token:", "Token admin:"));
                ui.add(egui::TextEdit::singleline(&mut self.s.admin_token).password(true));
            });
        });

        ui.add_space(6.0);
        ui.small(i18n::t("← slower/reverse · → faster · Space play/pause · ↑↓ line · Home top · Esc stop", "← perlahan/undur · → laju · Space main/henti · ↑↓ baris · Home atas · Esc diam"));
        ui.small(i18n::t("Click text on the left or in the preview to bring that line to the reading line. Double-click Screen 2 = toggle fullscreen.", "Klik teks di kiri atau dalam pratonton untuk letak baris itu pada garis bacaan. Dwi-klik Skrin 2 = togol skrin penuh."));
    }
}

fn fmt_age(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(ts);
    let d = (now - ts).max(0);
    if d < 3600 {
        tr!("{} min ago", "{} min lalu", d / 60)
    } else if d < 86400 {
        tr!("{} h ago", "{} jam lalu", d / 3600)
    } else {
        tr!("{} days ago", "{} hari lalu", d / 86400)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_net();

        // Prompter geometry (computed in the main context; same layout as screen 2).
        let pgal = Self::layout(&ctx, &self.spans, &self.prompter_job(1.0));
        let pmap = RowMap::new(&pgal);
        let line_h = self.s.font_size * self.s.line_spacing;

        self.handle_keys(&ctx, line_h);
        self.remote_cmds(&ctx, &pmap, line_h);
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let min = pmap.first_row_center();
        let max = (pmap.height - line_h * 0.5).max(min);
        if let Some(mut g) = self.glide {
            // Ease towards the clicked line; normal scrolling resumes after.
            g.to = g.to.clamp(min, max);
            let now = ctx.input(|i| i.time);
            let t = ((now - g.start) / g.dur) as f32;
            self.scroll = g.from + (g.to - g.from) * ease_in_out(t);
            self.glide = if t >= 1.0 { None } else { Some(g) };
            ctx.request_repaint();
        } else {
            let v = self.speed_px();
            if v != 0.0 {
                self.scroll += v * dt;
                ctx.request_repaint();
            }
        }
        self.scroll = self.scroll.clamp(min, max);
        if self.scroll >= max && self.speed_level > 0 {
            self.speed_level = 0;
        }
        self.remote_publish(&pgal, &pmap);

        if self.prompter_open {
            self.show_prompter(&ctx);
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    self.lang_switch(ui);
                    ui.separator();
                    ui.with_layout(
                        egui::Layout::left_to_right(Align::Center).with_main_wrap(true),
                        |ui| self.top_bar(ui),
                    );
                });
            });
            ui.horizontal(|ui| {
                if !self.loaded_title.is_empty() {
                    ui.label(
                        egui::RichText::new(&self.loaded_title)
                            .color(Color32::from_rgb(245, 197, 66)),
                    );
                    ui.separator();
                }
                ui.label(&self.status);
            });
            ui.add_space(2.0);
        });

        let total_w = ui.available_width();
        egui::Panel::right("right")
            .exact_size((total_w / 3.0).max(320.0))
            .resizable(false)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(i18n::t("Screen 2 preview", "Pratonton Skrin 2")).strong());
                self.preview(ui, &pmap);
                egui::ScrollArea::vertical().show(ui, |ui| self.controls(ui));
            });

        egui::CentralPanel::default_margins().show(ui, |ui| {
            self.script_view(ui, &pmap);
        });

        self.qr_card(&ctx);

        let now = ctx.input(|i| i.time);
        self.autosave(now);
    }
}

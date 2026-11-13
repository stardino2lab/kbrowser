//! `kb view`: live terminal viewer. Streams page frames with the kitty
//! graphics protocol, forwards mouse and keyboard to the page, shows 1, 1x2
//! or 2x2 tabs, and zooms by changing the CDP layout viewport.

use crate::i18n::{f, t, M};
use crate::actions::key_params;
use crate::cli;
use crate::ipc;
use crate::json::{self, Value};
use crate::platform::RawMode;
use crate::term::{Ev, MouseKind, Parser};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[2J\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[?2004h\x1b[>1u";
const LEAVE: &str = "\x1b_Ga=d,d=A,q=2\x1b\\\x1b[<u\x1b[?2004l\x1b[?1016l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[0m\x1b[?25h\x1b[?1049l";
/// Pixel-mouse support, kitty graphics, kitty keyboard, cells, pixels, cell
/// size; DA1 last because every terminal answers it (marks "replies done").
const QUERIES: &str = "\x1b[?1016$p\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[?u\x1b[18t\x1b[14t\x1b[16t\x1b[c";
const SIZE_QUERY: &str = "\x1b[18t\x1b[14t";
const ZOOMS: [i64; 4] = [0, 1280, 1920, 2560];

enum Msg {
    Input(Vec<u8>),
    Eof,
    Frame { pane: usize, tab: u32, w: i64, h: i64, data: Vec<u8> },
    Event(Value),
    Closed,
    Tabs(Value),
    Note(String),
}

#[derive(Default, Clone, Debug)]
struct Term {
    rows: u32,
    cols: u32,
    pw: u32,
    ph: u32,
    px_reported: bool,
    cell: Option<(u32, u32)>, // (h, w)
    graphics: Option<bool>,
    keyboard: Option<u32>,
    pixel_mouse: Option<bool>,
}

impl Term {
    fn apply(&mut self, ev: &Ev) -> bool {
        match ev {
            Ev::Cells { rows, cols } => (self.rows, self.cols) = (*rows, *cols),
            Ev::Pixels { h, w } => {
                (self.ph, self.pw) = (*h, *w);
                self.px_reported = *h > 0 && *w > 0;
            }
            Ev::CellPx { h, w } => self.cell = Some((*h, *w)),
            Ev::Graphics(ok) => self.graphics = Some(*ok),
            Ev::Keyboard(flags) => self.keyboard = Some(*flags),
            Ev::PixelMouse(ok) => self.pixel_mouse = Some(*ok),
            _ => return false,
        }
        true
    }
    /// Fill in whatever the terminal did not report.
    fn normalize(&mut self) {
        if self.rows == 0 || self.cols == 0 {
            let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
            self.cols = env("COLUMNS").unwrap_or(80);
            self.rows = env("LINES").unwrap_or(24);
        }
        if self.pw == 0 || self.ph == 0 {
            let (ch, cw) = self.cell.unwrap_or((20, 10));
            self.pw = self.cols * cw;
            self.ph = self.rows * ch;
        }
    }
    fn cell_px(&self) -> (f64, f64) {
        (self.pw as f64 / self.cols.max(1) as f64, self.ph as f64 / self.rows.max(1) as f64)
    }
}

#[derive(Default, Clone)]
struct Pane {
    tab: u32,
    zoom: i64, // layout viewport width in CSS px; 0 = match the pane
    // Image area in 1-based cells and in pixels.
    col: u32,
    row: u32,
    cols: u32,
    rows: u32,
    px_w: i64,
    px_h: i64,
    // CSS viewport size of the frames being shown.
    vw: i64,
    vh: i64,
    title: String,
    url: String,
    dialog: String,
}

enum PromptKind {
    Url,
    /// Note for pin `id`, typed here and mirrored onto the page by the daemon.
    Pin { id: u32, element: String },
}

struct Prompt {
    kind: PromptKind,
    text: String,
}

struct TabInfo {
    id: u32,
    title: String,
    url: String,
    dialog: String,
}

/// Display width: wide (CJK, Hangul, emoji) characters take two cells.
fn width(c: char) -> usize {
    let u = c as u32;
    if u >= 0x1100 && (u <= 0x115f || (0x2e80..=0xa4cf).contains(&u) || (0xac00..=0xd7a3).contains(&u)
        || (0xf900..=0xfaff).contains(&u) || (0xfe30..=0xfe4f).contains(&u) || (0xff00..=0xff60).contains(&u)
        || (0x1f300..=0x1faff).contains(&u) || (0x20000..=0x3fffd).contains(&u))
    {
        2
    } else {
        1
    }
}

/// Clip or pad `s` to exactly `w` terminal cells.
fn fit(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars().filter(|c| !c.is_control()) {
        if used + width(c) > w {
            break;
        }
        used += width(c);
        out.push(c);
    }
    out.push_str(&" ".repeat(w - used));
    out
}

struct View {
    out: Vec<u8>,
    stream: TcpStream,
    tx: Sender<Msg>,
    t: Term,
    layout: usize,
    panes: Vec<Pane>,
    focus: usize,
    tabs: Vec<TabInfo>,
    default_zoom: i64,
    leader: bool,
    prompt: Option<Prompt>,
    note: String,
    /// A click became a pin: its button release is not for the page.
    swallow_right: bool,
    swallow_left: bool,
    /// Leader keys (Ctrl+<letter>). Several by default because terminal
    /// multiplexers tend to claim one of them for themselves.
    leaders: Vec<String>,
    /// Armed by leader+m: the next left click pins instead of clicking.
    pin_mode: bool,
    held: i64,
    click: (Instant, i64, i64, i64),
    motion: Option<(usize, i64, i64, i64)>,
    last_motion: Instant,
    quit: bool,
}

fn req(cmd: &str, args: &[&str], tab: Option<u32>) -> Value {
    let mut r = obj! { "id" => 1, "cmd" => cmd, "args" => args.iter().map(|a| Value::from(*a)).collect::<Vec<_>>() };
    if let Some(t) = tab {
        r.set("tab", t);
    }
    r
}

fn flush_stdout(buf: &mut Vec<u8>) {
    if buf.is_empty() {
        return;
    }
    let mut so = std::io::stdout().lock();
    let _ = so.write_all(buf);
    let _ = so.flush();
    buf.clear();
}

impl View {
    fn put(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
    }

    fn send(&mut self, v: &Value) {
        if ipc::write_json(&mut self.stream, ipc::T_REQUEST, v).is_err() {
            self.quit = true;
        }
    }

    /// Run a daemon command off the UI thread; its outcome lands in the status line.
    fn spawn(&self, r: Value) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let note = match cli::request(&r) {
                Ok(v) => v.get("text").str().lines().next().unwrap_or("").to_string(),
                Err(e) => format!("error[{}]: {}", e.0, e.1),
            };
            let _ = tx.send(Msg::Note(note));
        });
    }

    fn refresh_tabs(&mut self, v: &Value) {
        self.tabs = v
            .get("tabs")
            .arr()
            .iter()
            .map(|t| TabInfo {
                id: t.get("tab").as_i64().unwrap_or(0) as u32,
                title: t.get("title").str().to_string(),
                url: t.get("url").str().to_string(),
                dialog: t.get("dialog").str().to_string(),
            })
            .collect();
    }

    /// A tab not shown in any pane, creating one if needed.
    fn spare_tab(&mut self, skip: Option<u32>) -> Option<u32> {
        let used: Vec<u32> = self.panes.iter().map(|p| p.tab).collect();
        if let Some(t) = self.tabs.iter().find(|t| !used.contains(&t.id) && Some(t.id) != skip) {
            return Some(t.id);
        }
        let mut r = req("tab", &["new"], None);
        r.set("keep_current", true);
        match cli::request(&r) {
            Ok(v) => {
                let id = v.get("tab").as_i64()? as u32;
                self.tabs.push(TabInfo { id, title: String::new(), url: "about:blank".into(), dialog: String::new() });
                Some(id)
            }
            Err(e) => {
                self.note = format!("error[{}]: {}", e.0, e.1);
                None
            }
        }
    }

    fn attach(&mut self, i: usize) {
        let p = &self.panes[i];
        let v = obj! {
            "cmd" => "view.attach", "pane" => i, "tab" => p.tab,
            "px_w" => p.px_w, "px_h" => p.px_h, "viewport_w" => p.zoom,
        };
        self.send(&v);
    }

    /// Recompute pane geometry for the current terminal size and layout,
    /// redraw everything and (re)subscribe each pane.
    fn relayout(&mut self) {
        let n = self.layout;
        for i in n..self.panes.len() {
            self.send(&obj! { "cmd" => "view.detach", "pane" => i });
        }
        self.panes.truncate(n);
        while self.panes.len() < n {
            let Some(tab) = self.spare_tab(None) else { break };
            self.panes.push(Pane { tab, zoom: self.default_zoom, ..Pane::default() });
        }
        if self.panes.is_empty() {
            self.quit = true;
            return;
        }
        self.focus = self.focus.min(self.panes.len() - 1);
        let (gc, gr) = match n {
            1 => (1u32, 1u32),
            2 => (2, 1),
            _ => (2, 2),
        };
        let (cw, ch) = self.t.cell_px();
        let pane_cols = (self.t.cols.saturating_sub(gc - 1) / gc).max(4);
        let pane_rows = (self.t.rows.saturating_sub(1) / gr).max(3);
        for (i, p) in self.panes.iter_mut().enumerate() {
            let (gx, gy) = (i as u32 % gc, i as u32 / gc);
            p.col = 1 + gx * (pane_cols + 1);
            p.row = 2 + gy * pane_rows; // title row sits just above
            p.cols = pane_cols;
            p.rows = pane_rows - 1;
            p.px_w = (p.cols as f64 * cw).floor() as i64;
            p.px_h = (p.rows as f64 * ch).floor() as i64;
        }
        self.put("\x1b_Ga=d,d=A,q=2\x1b\\\x1b[0m\x1b[2J");
        for i in 0..self.panes.len() {
            self.attach(i);
            self.draw_title(i);
        }
        self.draw_status();
    }

    fn draw_title(&mut self, i: usize) {
        let p = &self.panes[i];
        let zoom = if p.zoom == 0 { "fit".to_string() } else { p.zoom.to_string() };
        let right = format!(" {} ", zoom);
        let left = format!(" [{}] {} - {}", p.tab, p.title, p.url);
        let w = p.cols as usize;
        let text = format!("{}{}", fit(&left, w.saturating_sub(right.len())), fit(&right, right.len().min(w)));
        let style = if i == self.focus { "\x1b[0;30;46m" } else { "\x1b[0;37;100m" };
        let s = format!("\x1b[{};{}H{}{}\x1b[0m", p.row - 1, p.col, style, text);
        self.put(&s);
    }

    fn draw_status(&mut self) {
        let dialog = self.panes.get(self.focus).map(|p| p.dialog.clone()).unwrap_or_default();
        let text = if let Some(p) = &self.prompt {
            let cols = self.t.cols as usize;
            let head = match &p.kind {
                PromptKind::Url => format!(" {}", t(M::PromptOpen)),
                // A long element description must not push the note off the line.
                PromptKind::Pin { id, element, .. } => format!(" {}", f(M::PromptPin, &[&id.to_string(), fit(element, cols / 3).trim_end()])),
            };
            // Keep the end of the text (where typing happens) in view.
            let room = cols.saturating_sub(head.chars().map(width).sum::<usize>() + 1);
            let mut tail: Vec<char> = Vec::new();
            let mut used = 0;
            for c in p.text.chars().rev().filter(|c| !c.is_control()) {
                if used + width(c) > room {
                    break;
                }
                used += width(c);
                tail.push(c);
            }
            format!("{}{}_", head, tail.iter().rev().collect::<String>())
        } else if self.pin_mode {
            format!(" {}", t(M::PinMode))
        } else if self.leader {
            format!(" {}", t(M::ViewKeys))
        } else if !dialog.is_empty() {
            format!(" {}", f(M::Dialog, &[&dialog]))
        } else if !self.note.is_empty() {
            format!(" {}", self.note)
        } else {
            format!(" {}", f(M::Idle, &[&self.leaders.first().map(|l| l.to_uppercase()).unwrap_or_default()]))
        };
        let s = format!("\x1b[{};1H\x1b[0;7m{}\x1b[0m", self.t.rows, fit(&text, self.t.cols as usize));
        self.put(&s);
    }

    /// Place a PNG frame over the pane's cell area (kitty graphics, chunked).
    fn draw_frame(&mut self, i: usize, data: &[u8]) {
        let p = &self.panes[i];
        let head = format!("\x1b[?2026h\x1b[{};{}H", p.row, p.col);
        let first = format!("a=T,f=100,i={},q=2,C=1,c={},r={}", i + 1, p.cols, p.rows);
        self.put(&head);
        let mut chunks = data.chunks(4096).peekable();
        let mut is_first = true;
        while let Some(chunk) = chunks.next() {
            let more = if chunks.peek().is_some() { 1 } else { 0 };
            let ctl = if is_first { format!("{},m={}", first, more) } else { format!("m={}", more) };
            is_first = false;
            self.out.extend_from_slice(b"\x1b_G");
            self.out.extend_from_slice(ctl.as_bytes());
            self.out.push(b';');
            self.out.extend_from_slice(chunk);
            self.out.extend_from_slice(b"\x1b\\");
        }
        self.put("\x1b[?2026l");
    }

    fn input(&mut self, tab: u32, method: &str, params: Value) {
        self.send(&obj! { "cmd" => "view.input", "tab" => tab, "method" => method, "params" => params });
    }

    fn set_focus(&mut self, i: usize) {
        if i != self.focus && i < self.panes.len() {
            let old = self.focus;
            self.focus = i;
            self.draw_title(old);
            self.draw_title(i);
            self.draw_status();
        }
    }

    fn flush_motion(&mut self) {
        if let Some((i, x, y, mods)) = self.motion.take() {
            if let Some(tab) = self.panes.get(i).map(|p| p.tab) {
                let button = if self.held & 1 != 0 { "left" } else if self.held & 2 != 0 { "right" } else if self.held & 4 != 0 { "middle" } else { "none" };
                let p = obj! { "type" => "mouseMoved", "x" => x, "y" => y, "button" => button, "buttons" => self.held, "modifiers" => mods };
                self.input(tab, "Input.dispatchMouseEvent", p);
                self.last_motion = Instant::now();
            }
        }
    }

    fn on_mouse(&mut self, kind: MouseKind, button: u8, x: i64, y: i64, mods: i64) {
        let (cw, ch) = self.t.cell_px();
        // Terminal position -> window pixels.
        let (px, py) = if self.t.pixel_mouse == Some(true) {
            (x as f64, y as f64)
        } else {
            ((x as f64 - 0.5) * cw, (y as f64 - 0.5) * ch)
        };
        let origin = |p: &Pane| ((p.col - 1) as f64 * cw, (p.row - 1) as f64 * ch);
        let hit = self.panes.iter().position(|p| {
            let (x0, y0) = origin(p);
            px >= x0 && px < x0 + p.px_w as f64 && py >= y0 - ch && py < y0 + p.px_h as f64
        });
        // A drag that leaves the pane still belongs to the pane it started in.
        let Some(i) = hit.or((self.held != 0).then_some(self.focus)) else { return };
        if kind == MouseKind::Press {
            self.set_focus(i);
        }
        let p = &self.panes[i];
        let (x0, y0) = origin(p);
        if py < y0 && self.held == 0 {
            return; // title row
        }
        // Window pixels -> CSS pixels of the (possibly zoomed) layout viewport.
        let (vw, vh) = (if p.vw > 0 { p.vw } else { p.px_w }, if p.vh > 0 { p.vh } else { p.px_h });
        let cx = (((px - x0) * vw as f64 / p.px_w.max(1) as f64).floor() as i64).clamp(0, vw - 1);
        let cy = (((py - y0) * vh as f64 / p.px_h.max(1) as f64).floor() as i64).clamp(0, vh - 1);
        let tab = p.tab;
        let (name, mask) = match button {
            0 => ("left", 1),
            1 => ("middle", 4),
            _ => ("right", 2),
        };
        // Pin by left click: armed with leader+m, or with Alt held. For
        // terminals that keep the right button for their own menu.
        if button == 0 && (self.pin_mode || mods & 1 != 0) && kind == MouseKind::Press {
            self.pin_mode = false;
            self.swallow_left = true;
            self.start_pin(tab, cx, cy);
            return;
        }
        if button == 0 && kind == MouseKind::Release && std::mem::take(&mut self.swallow_left) {
            return;
        }
        // Plain right-click pins a note to the element; Ctrl+right-click is
        // a real right-click for the page.
        if button == 2 && mods & 2 == 0 && kind != MouseKind::Wheel && kind != MouseKind::Move {
            if kind == MouseKind::Press {
                self.swallow_right = true;
                self.start_pin(tab, cx, cy);
                return;
            }
            if std::mem::take(&mut self.swallow_right) {
                return;
            }
        }
        match kind {
            MouseKind::Move => {
                self.motion = Some((i, cx, cy, mods));
                if self.last_motion.elapsed() >= Duration::from_millis(15) {
                    self.flush_motion();
                }
            }
            MouseKind::Press => {
                self.motion = Some((i, cx, cy, mods));
                self.flush_motion();
                let (at, lx, ly, count) = self.click;
                let again = at.elapsed() < Duration::from_millis(400) && (lx - cx).abs() < 5 && (ly - cy).abs() < 5;
                let count = if again { count + 1 } else { 1 };
                self.click = (Instant::now(), cx, cy, count);
                self.held |= mask;
                let ev = obj! { "type" => "mousePressed", "x" => cx, "y" => cy, "button" => name, "buttons" => self.held, "clickCount" => count, "modifiers" => mods };
                self.input(tab, "Input.dispatchMouseEvent", ev);
            }
            MouseKind::Release => {
                self.flush_motion();
                self.held &= !mask;
                let ev = obj! { "type" => "mouseReleased", "x" => cx, "y" => cy, "button" => name, "buttons" => self.held, "clickCount" => self.click.3, "modifiers" => mods };
                self.input(tab, "Input.dispatchMouseEvent", ev);
            }
            MouseKind::Wheel => {
                let (dx, dy) = match button {
                    0 => (0, -120),
                    1 => (0, 120),
                    2 => (-120, 0),
                    _ => (120, 0),
                };
                let ev = obj! { "type" => "mouseWheel", "x" => cx, "y" => cy, "deltaX" => dx, "deltaY" => dy, "modifiers" => mods };
                self.input(tab, "Input.dispatchMouseEvent", ev);
            }
        }
    }

    fn start_pin(&mut self, tab: u32, x: i64, y: i64) {
        let mut r = req("pin", &["at", &x.to_string(), &y.to_string()], Some(tab));
        r.set("placeholder", t(M::NoteHint)); // shown in the on-page bubble
        match cli::request(&r) {
            Ok(v) => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::Pin {
                        id: v.get("pin").as_i64().unwrap_or(0) as u32,
                        element: v.get("element").str().to_string(),
                    },
                    text: v.get("note").str().to_string(),
                });
            }
            Err(e) => self.note = format!("error[{}]: {}", e.0, e.1),
        }
        self.draw_status();
    }

    /// Mirror the note being typed onto the page, next to the pinned spot.
    fn push_draft(&mut self) {
        if let Some(Prompt { kind: PromptKind::Pin { id, .. }, text }) = &self.prompt {
            let _ = cli::request(&req("pin", &["draft", &id.to_string(), text], None));
        }
    }

    fn save_pins(&mut self) {
        self.note = match cli::request(&req("pins", &[], None)) {
            Ok(v) => match cli::save_pins(&v, "kb-pins.md").and_then(|_| cli::save_pins(&v, "kb-pins.json")) {
                Ok(n) => {
                    let dir = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
                    f(M::SavedPins, &[&n.to_string(), &dir])
                }
                Err(e) => f(M::CannotWritePins, &[&e.to_string()]),
            },
            Err(e) => format!("error[{}]: {}", e.0, e.1),
        };
    }

    fn send_key(&mut self, name: &str, mods: i64) {
        let Some(tab) = self.panes.get(self.focus).map(|p| p.tab) else { return };
        let Some((mut down, up)) = key_params(name, mods) else { return };
        // Synthetic key events do not trigger browser editing shortcuts by themselves.
        if mods == 2 {
            let cmd = match name {
                "a" => "selectAll",
                "c" => "copy",
                "v" => "paste",
                "x" => "cut",
                "z" => "undo",
                "y" => "redo",
                _ => "",
            };
            if !cmd.is_empty() {
                down.set("commands", vec![Value::from(cmd)]);
            }
        }
        self.input(tab, "Input.dispatchKeyEvent", down);
        self.input(tab, "Input.dispatchKeyEvent", up);
    }

    fn send_text(&mut self, text: &str) {
        if let Some(tab) = self.panes.get(self.focus).map(|p| p.tab) {
            self.input(tab, "Input.insertText", obj! { "text" => text });
        }
    }

    fn rebind(&mut self, i: usize, tab: u32) {
        let p = &mut self.panes[i];
        p.tab = tab;
        (p.vw, p.vh) = (0, 0);
        p.title.clear();
        p.url.clear();
        p.dialog.clear();
        if let Some(t) = self.tabs.iter().find(|t| t.id == tab) {
            let p = &mut self.panes[i];
            p.title = t.title.clone();
            p.url = t.url.clone();
        }
        self.attach(i);
        self.draw_title(i);
    }

    fn command(&mut self, name: &str, mods: i64) {
        let i = self.focus;
        let Some(tab) = self.panes.get(i).map(|p| p.tab) else { return };
        let n = self.panes.len();
        match name {
            "1" | "2" | "4" => {
                self.layout = name.parse().unwrap_or(1);
                self.relayout();
            }
            "ArrowRight" | "ArrowDown" | "Tab" | "o" => self.set_focus((i + 1) % n),
            "ArrowLeft" | "ArrowUp" => self.set_focus((i + n - 1) % n),
            "z" => {
                let at = ZOOMS.iter().position(|z| *z == self.panes[i].zoom).unwrap_or(0);
                self.panes[i].zoom = ZOOMS[(at + 1) % ZOOMS.len()];
                self.attach(i);
                self.draw_title(i);
            }
            "l" => self.prompt = Some(Prompt { kind: PromptKind::Url, text: String::new() }),
            "a" => self.spawn(req("badges", &["toggle"], Some(tab))),
            "m" => self.pin_mode = true,
            "?" | "/" => self.note = t(M::ViewKeys).to_string(),
            "s" => self.save_pins(),
            "t" => {
                let mut r = req("tab", &["new"], None);
                r.set("keep_current", true);
                match cli::request(&r) {
                    Ok(v) => {
                        let id = v.get("tab").as_i64().unwrap_or(0) as u32;
                        self.tabs.push(TabInfo { id, title: String::new(), url: "about:blank".into(), dialog: String::new() });
                        self.rebind(i, id);
                    }
                    Err(e) => self.note = format!("error[{}]: {}", e.0, e.1),
                }
            }
            "w" => {
                if let Some(next) = self.spare_tab(Some(tab)) {
                    self.rebind(i, next);
                    self.spawn(req("tab", &["close", &tab.to_string()], None));
                }
            }
            "n" | "p" => {
                let used: Vec<u32> = self.panes.iter().map(|p| p.tab).collect();
                let free: Vec<u32> = self.tabs.iter().map(|t| t.id).filter(|t| *t == tab || !used.contains(t)).collect();
                if let Some(at) = free.iter().position(|t| *t == tab) {
                    let step = if name == "n" { 1 } else { free.len() - 1 };
                    let next = free[(at + step) % free.len()];
                    if next != tab {
                        self.rebind(i, next);
                    }
                }
            }
            "r" => self.spawn(req("reload", &[], Some(tab))),
            "[" => self.spawn(req("back", &[], Some(tab))),
            "]" => self.spawn(req("forward", &[], Some(tab))),
            "q" => self.quit = true,
            k if mods == 2 && self.leaders.iter().any(|l| l == k) => self.send_key(k, 2), // leader twice: literal
            _ => {}
        }
    }

    fn on_key(&mut self, name: &str, mods: i64) {
        if let Some(mut p) = self.prompt.take() {
            let mut keep = true;
            match name {
                "Enter" => {
                    keep = false;
                    let text = p.text.trim().to_string();
                    match &p.kind {
                        PromptKind::Url => {
                            if let (false, Some(tab)) = (text.is_empty(), self.panes.get(self.focus).map(|p| p.tab)) {
                                self.note = f(M::Opening, &[&text]);
                                self.spawn(req("open", &[&text], Some(tab)));
                            }
                        }
                        // An empty note deletes the pin (the daemon treats it so).
                        PromptKind::Pin { id, .. } => self.spawn(req("pin", &["note", &id.to_string(), &text], None)),
                    }
                }
                "Escape" => {
                    keep = false;
                    if let PromptKind::Pin { id, .. } = &p.kind {
                        self.spawn(req("pin", &["cancel", &id.to_string()], None));
                    }
                }
                "Backspace" => {
                    p.text.pop();
                }
                "u" if mods == 2 => p.text.clear(),
                "Space" => p.text.push(' '),
                k if k.chars().count() == 1 && mods & 7 == 0 => p.text.push_str(k),
                _ => {}
            }
            if keep {
                self.prompt = Some(p);
                self.push_draft();
            }
            self.draw_status();
            return;
        }
        if self.leader {
            self.leader = false;
            self.command(name, mods);
            self.draw_status();
            return;
        }
        if self.pin_mode && name == "Escape" {
            self.pin_mode = false;
            self.draw_status();
            return;
        }
        if mods == 2 && self.leaders.iter().any(|l| l == name) {
            self.leader = true;
            self.draw_status();
            return;
        }
        let pane = self.panes.get(self.focus).cloned().unwrap_or_default();
        if !pane.dialog.is_empty() && (name == "Enter" || name == "Escape") {
            let action = if name == "Enter" { "accept" } else { "dismiss" };
            self.spawn(req("dialog", &[action], Some(pane.tab)));
            return;
        }
        self.send_key(name, mods);
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Key { name, mods } => self.on_key(&name, mods),
            Ev::Text(s) | Ev::Paste(s) => {
                if let Some(p) = self.prompt.as_mut() {
                    p.text.push_str(s.trim_end_matches('\n'));
                    self.push_draft();
                    self.draw_status();
                } else {
                    self.send_text(&s);
                }
            }
            Ev::Mouse { kind, button, x, y, mods } => self.on_mouse(kind, button, x, y, mods),
            other => {
                // Size reports: re-layout when the terminal actually changed.
                let before = (self.t.rows, self.t.cols, self.t.pw, self.t.ph);
                self.t.apply(&other);
                if matches!(other, Ev::Cells { .. }) && !self.t.px_reported {
                    // This terminal never reports pixels: derive them from cells.
                    (self.t.pw, self.t.ph) = (0, 0);
                    self.t.normalize();
                }
                if before != (self.t.rows, self.t.cols, self.t.pw, self.t.ph) && self.t.rows > 0 && self.t.pw > 0 {
                    self.relayout();
                }
            }
        }
    }

    fn on_tabs(&mut self, v: &Value) {
        self.refresh_tabs(v);
        for i in 0..self.panes.len() {
            let tab = self.panes[i].tab;
            match self.tabs.iter().find(|t| t.id == tab) {
                Some(t) => {
                    let (title, url, dialog) = (t.title.clone(), t.url.clone(), t.dialog.clone());
                    let p = &mut self.panes[i];
                    if p.title != title || p.url != url || p.dialog != dialog {
                        (p.title, p.url, p.dialog) = (title, url, dialog);
                        self.draw_title(i);
                        self.draw_status();
                    }
                }
                None => {
                    // The tab was closed elsewhere (agent, page script).
                    if let Some(next) = self.spare_tab(None) {
                        self.rebind(i, next);
                    }
                }
            }
        }
    }
}

/// Send the capability queries and collect replies until DA1 or a timeout.
fn probe(rx: &Receiver<Msg>, parser: &mut Parser, extra: &mut Vec<Ev>) -> Term {
    let mut out = QUERIES.as_bytes().to_vec();
    flush_stdout(&mut out);
    let mut t = Term::default();
    let mut end = Instant::now() + Duration::from_millis(1500);
    let mut da1 = false;
    // DA1 does not reliably come last: under Windows ConPTY the console host
    // answers DA1 itself at once, while the graphics query travels to the
    // real terminal and back. So after DA1, keep listening a little longer
    // for the graphics reply.
    while !(da1 && t.graphics.is_some()) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok(Msg::Input(bytes)) => {
                let mut evs = Vec::new();
                parser.feed(&bytes, &mut evs);
                for ev in evs {
                    if ev == Ev::Da1 {
                        if !da1 {
                            da1 = true;
                            end = end.min(Instant::now() + Duration::from_millis(600));
                        }
                    } else if !t.apply(&ev) {
                        extra.push(ev);
                    }
                }
            }
            Ok(Msg::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }
    t
}

fn report(t: &Term, tty: bool) -> String {
    let yn = |o: Option<bool>| match o {
        Some(true) => "yes",
        Some(false) => "no",
        None => "no reply",
    };
    format!(
        "kb view --probe\n  tty:              {}\n  kitty graphics:   {}\n  size (cells):     {}\n  size (pixels):    {}\n  cell size (px):   {}\n  kitty keyboard:   {}\n  pixel mouse:      {}\n  TERM={} TERM_PROGRAM={}\n",
        if tty { "yes" } else { "no (stdin/stdout is not a terminal)" },
        yn(t.graphics),
        if t.cols > 0 { format!("{}x{}", t.cols, t.rows) } else { "no reply".into() },
        if t.pw > 0 { format!("{}x{}", t.pw, t.ph) } else { "no reply".into() },
        t.cell.map(|(h, w)| format!("{}x{}", w, h)).unwrap_or("no reply".into()),
        t.keyboard.map(|f| format!("yes (flags {})", f)).unwrap_or("no reply".into()),
        yn(t.pixel_mouse),
        std::env::var("TERM").unwrap_or_default(),
        std::env::var("TERM_PROGRAM").unwrap_or_default(),
    )
}

/// `kb view --keys`: show exactly what this terminal delivers for each key
/// press and mouse action. Tells apart "the terminal (or a multiplexer in
/// front of it) kept the key" from "kb misread it".
fn keys_mode(rx: &Receiver<Msg>, parser: &mut Parser) -> i32 {
    let mut out = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1016h\x1b[?2004h\x1b[>1u".to_vec();
    out.extend_from_slice(b"kb view --keys: press keys, click, right-click, scroll. Press q three times to quit.\r\n");
    out.extend_from_slice(b"Nothing printed for an action = the terminal did not pass it to kb.\r\n");
    flush_stdout(&mut out);
    let mut quits = 0;
    let show = |evs: Vec<Ev>, raw: &str, quits: &mut i32| {
        let mut out = Vec::new();
        for ev in evs {
            *quits = if matches!(&ev, Ev::Key { name, mods: 0 } if name == "q") { *quits + 1 } else { 0 };
            out.extend_from_slice(format!("{:<36} {:?}\r\n", raw, ev).as_bytes());
        }
        flush_stdout(&mut out);
    };
    while quits < 3 {
        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(Msg::Input(bytes)) => {
                let raw: String = bytes.iter().map(|b| if (0x20..0x7f).contains(b) { (*b as char).to_string() } else { format!("\\x{:02x}", b) }).collect();
                let mut evs = Vec::new();
                parser.feed(&bytes, &mut evs);
                show(evs, &raw, &mut quits);
            }
            Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if parser.pending() {
                    let mut evs = Vec::new();
                    parser.flush(&mut evs);
                    show(evs, "(lone ESC)", &mut quits);
                }
            }
            Ok(_) => {}
        }
    }
    let mut out = b"\x1b[<u\x1b[?2004l\x1b[?1016l\x1b[?1006l\x1b[?1002l\x1b[?1000l".to_vec();
    flush_stdout(&mut out);
    0
}

/// Restores the terminal even if the viewer panics.
struct Screen;
impl Drop for Screen {
    fn drop(&mut self) {
        let mut out = LEAVE.as_bytes().to_vec();
        flush_stdout(&mut out);
    }
}

pub fn run(opts: &Value, url: Option<&str>) -> i32 {
    let raw = RawMode::enter();
    let (tx, rx) = mpsc::channel::<Msg>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Msg::Eof);
                        return;
                    }
                    Ok(n) => {
                        if tx.send(Msg::Input(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }
    let mut parser = Parser::default();
    let mut early = Vec::new();

    if opts.get("keys").as_bool() == Some(true) {
        let code = keys_mode(&rx, &mut parser);
        drop(raw);
        return code;
    }
    if opts.get("probe").as_bool() == Some(true) {
        // Ask with the modes switched on, exactly as the viewer runs, or
        // "supported but currently off" would be reported as unsupported.
        let mut on = b"\x1b[?1016h\x1b[>1u".to_vec();
        flush_stdout(&mut on);
        let t = probe(&rx, &mut parser, &mut early);
        let mut off = b"\x1b[<u\x1b[?1016l".to_vec();
        flush_stdout(&mut off);
        let tty = raw.is_tty();
        drop(raw);
        print!("{}", report(&t, tty));
        return if t.graphics == Some(true) { 0 } else { 3 };
    }

    let mut out = ENTER.as_bytes().to_vec();
    flush_stdout(&mut out);
    let screen = Screen;
    let mut t = probe(&rx, &mut parser, &mut early);
    if t.graphics != Some(true) && opts.get("force").as_bool() != Some(true) {
        let tty = raw.is_tty();
        drop(screen);
        drop(raw);
        eprintln!("error[terminal]: this terminal did not confirm kitty graphics support");
        eprintln!("hint: use Kitty, Ghostty, WezTerm or Herdr; --force tries anyway. Details:");
        if let Some(why) = crate::i18n::explain("terminal") {
            eprintln!("({})", why);
        }
        eprint!("{}", report(&t, tty));
        return 3;
    }
    t.normalize();

    let fail = |screen: Screen, raw: RawMode, msg: String| {
        drop(screen);
        drop(raw);
        eprintln!("error[daemon]: {}", msg);
        3
    };
    let stream = match cli::stream() {
        Ok(s) => s,
        Err(e) => return fail(screen, raw, e),
    };
    stream.set_read_timeout(None).ok();
    {
        let (tx, mut s) = (tx.clone(), match stream.try_clone() {
            Ok(s) => s,
            Err(e) => return fail(screen, raw, e.to_string()),
        });
        std::thread::spawn(move || loop {
            let msg = match ipc::read_frame(&mut s) {
                Ok((ipc::T_BINARY, body)) if body.len() >= 4 => {
                    let n = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
                    let meta = body.get(4..4 + n).and_then(|m| std::str::from_utf8(m).ok()).and_then(|m| json::parse(m).ok());
                    match meta {
                        Some(m) => Msg::Frame {
                            pane: m.get("pane").as_i64().unwrap_or(0) as usize,
                            tab: m.get("tab").as_i64().unwrap_or(0) as u32,
                            w: m.get("w").as_i64().unwrap_or(0),
                            h: m.get("h").as_i64().unwrap_or(0),
                            data: body[4 + n..].to_vec(),
                        },
                        None => continue,
                    }
                }
                Ok((ipc::T_EVENT, body)) => match std::str::from_utf8(&body).ok().and_then(|b| json::parse(b).ok()) {
                    Some(v) => Msg::Event(v),
                    None => continue,
                },
                Ok(_) => continue,
                Err(_) => {
                    let _ = tx.send(Msg::Closed);
                    return;
                }
            };
            if tx.send(msg).is_err() {
                return;
            }
        });
    }
    {
        let tx = tx.clone();
        std::thread::spawn(move || loop {
            if let Ok(v) = cli::request(&req("tabs", &[], None)) {
                if tx.send(Msg::Tabs(v)).is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(700));
        });
    }

    let zoom = match opts.get("zoom").str() {
        "" | "fit" => 0,
        z => z.parse::<i64>().unwrap_or(0).clamp(0, 7680),
    };
    let layout = match opts.get("layout").as_i64() {
        Some(2) => 2,
        Some(4) => 4,
        _ => 1,
    };
    let mut v = View {
        out: Vec::new(),
        stream,
        tx,
        t,
        layout,
        panes: Vec::new(),
        focus: 0,
        tabs: Vec::new(),
        default_zoom: zoom,
        leader: false,
        prompt: None,
        note: String::new(),
        swallow_right: false,
        swallow_left: false,
        leaders: match opts.get("leader").as_str() {
            Some(l) if l.len() == 1 && l.chars().all(|c| c.is_ascii_lowercase()) => vec![l.to_string()],
            _ => vec!["g".to_string(), "b".to_string()],
        },
        pin_mode: false,
        held: 0,
        click: (Instant::now(), -100, -100, 0),
        motion: None,
        last_motion: Instant::now(),
        quit: false,
    };
    match cli::request(&req("tabs", &[], None)) {
        Ok(tabs) => v.refresh_tabs(&tabs),
        Err(e) => return fail(screen, raw, e.1),
    }
    v.relayout();
    if let Some(url) = url {
        if let Some(tab) = v.panes.first().map(|p| p.tab) {
            v.spawn(req("open", &[url], Some(tab)));
        }
    }
    for ev in early {
        v.on_event(ev);
    }

    let mut next_size_poll = Instant::now() + Duration::from_millis(500);
    let mut esc_deadline: Option<Instant> = None;
    while !v.quit {
        flush_stdout(&mut v.out);
        let now = Instant::now();
        let mut wake = next_size_poll;
        if let Some(d) = esc_deadline {
            wake = wake.min(d);
        }
        if v.motion.is_some() {
            wake = wake.min(now + Duration::from_millis(15));
        }
        let first = match rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Drain what is queued; only the newest frame per pane is drawn.
        let mut frames: Vec<Option<Vec<u8>>> = vec![None; v.panes.len()];
        for msg in first.into_iter().chain(rx.try_iter().take(256)) {
            match msg {
                Msg::Input(bytes) => {
                    let mut evs = Vec::new();
                    parser.feed(&bytes, &mut evs);
                    esc_deadline = parser.pending().then(|| Instant::now() + Duration::from_millis(40));
                    for ev in evs {
                        v.on_event(ev);
                    }
                    frames.resize(v.panes.len(), None);
                }
                Msg::Eof | Msg::Closed => v.quit = true,
                Msg::Frame { pane, tab, w, h, data } => {
                    if let Some(p) = v.panes.get_mut(pane).filter(|p| p.tab == tab) {
                        (p.vw, p.vh) = (w, h);
                        if pane < frames.len() {
                            frames[pane] = Some(data);
                        }
                    }
                }
                Msg::Event(e) => {
                    v.note = format!("{}: {}", e.get("code").str(), e.get("message").str());
                    v.draw_status();
                }
                Msg::Tabs(tabs) => {
                    v.on_tabs(&tabs);
                    frames.resize(v.panes.len(), None);
                }
                Msg::Note(n) => {
                    v.note = n;
                    v.draw_status();
                }
            }
        }
        for (i, f) in frames.into_iter().enumerate() {
            if let (Some(data), true) = (f, i < v.panes.len()) {
                v.draw_frame(i, &data);
            }
        }
        let now = Instant::now();
        if esc_deadline.is_some_and(|d| now >= d) {
            esc_deadline = None;
            let mut evs = Vec::new();
            parser.flush(&mut evs);
            for ev in evs {
                v.on_event(ev);
            }
        }
        if v.motion.is_some() && v.last_motion.elapsed() >= Duration::from_millis(15) {
            v.flush_motion();
        }
        if now >= next_size_poll {
            next_size_poll = now + Duration::from_millis(500);
            v.put(SIZE_QUERY);
        }
    }
    flush_stdout(&mut v.out);
    drop(v);
    drop(screen);
    drop(raw);
    0
}

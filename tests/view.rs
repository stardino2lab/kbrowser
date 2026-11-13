//! Stage 2 end-to-end tests. The test process plays the terminal: it answers
//! the viewer's capability/size queries, injects key and mouse bytes on
//! stdin, and parses the kitty graphics stream from stdout down to decoded
//! PNG pixels. Real `kb` binary, real headless Chrome.
//!
//! This proves protocol correctness. It cannot prove that a given terminal
//! application actually paints the images.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- PNG decode

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    acc: u32,
    cnt: u32,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> u32 {
        while self.cnt < n {
            self.acc |= (self.d[self.pos] as u32) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
        let v = self.acc & ((1u32 << n) - 1);
        self.acc >>= n;
        self.cnt -= n;
        v
    }
}

struct Huff {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

fn huff(lengths: &[u8]) -> Huff {
    let mut counts = [0u16; 16];
    for l in lengths {
        counts[*l as usize] += 1;
    }
    counts[0] = 0;
    let mut offs = [0u16; 16];
    for i in 1..16 {
        offs[i] = offs[i - 1] + counts[i - 1];
    }
    let mut symbols = vec![0u16; lengths.len()];
    for (sym, l) in lengths.iter().enumerate() {
        if *l != 0 {
            symbols[offs[*l as usize] as usize] = sym as u16;
            offs[*l as usize] += 1;
        }
    }
    Huff { counts, symbols }
}

fn decode(b: &mut Bits, h: &Huff) -> u16 {
    let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
    for len in 1..16 {
        code |= b.bits(1) as i32;
        let count = h.counts[len] as i32;
        if code - count < first {
            return h.symbols[(index + (code - first)) as usize];
        }
        index += count;
        first += count;
        first <<= 1;
        code <<= 1;
    }
    panic!("bad huffman code");
}

fn inflate(data: &[u8]) -> Vec<u8> {
    const LBASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
    const LEXT: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
    const DBASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
    const DEXT: [u32; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
    let mut b = Bits { d: data, pos: 2, acc: 0, cnt: 0 }; // skip zlib header
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = b.bits(1);
        match b.bits(2) {
            0 => {
                b.acc = 0;
                b.cnt = 0;
                let len = u16::from_le_bytes([b.d[b.pos], b.d[b.pos + 1]]) as usize;
                b.pos += 4;
                out.extend_from_slice(&b.d[b.pos..b.pos + len]);
                b.pos += len;
            }
            kind @ (1 | 2) => {
                let (lit, dist) = if kind == 1 {
                    let mut l = [8u8; 288];
                    l[144..256].fill(9);
                    l[256..280].fill(7);
                    (huff(&l), huff(&[5u8; 30]))
                } else {
                    let (hlit, hdist, hclen) = (b.bits(5) as usize + 257, b.bits(5) as usize + 1, b.bits(4) as usize + 4);
                    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut cl = [0u8; 19];
                    for o in ORDER.iter().take(hclen) {
                        cl[*o] = b.bits(3) as u8;
                    }
                    let clh = huff(&cl);
                    let mut lens: Vec<u8> = Vec::new();
                    while lens.len() < hlit + hdist {
                        match decode(&mut b, &clh) {
                            16 => {
                                let prev = *lens.last().unwrap();
                                let n = 3 + b.bits(2);
                                lens.extend(std::iter::repeat(prev).take(n as usize));
                            }
                            17 => {
                                let n = 3 + b.bits(3);
                                lens.extend(std::iter::repeat(0).take(n as usize));
                            }
                            18 => {
                                let n = 11 + b.bits(7);
                                lens.extend(std::iter::repeat(0).take(n as usize));
                            }
                            s => lens.push(s as u8),
                        }
                    }
                    (huff(&lens[..hlit]), huff(&lens[hlit..hlit + hdist]))
                };
                loop {
                    let sym = decode(&mut b, &lit) as usize;
                    if sym < 256 {
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let len = LBASE[sym - 257] as usize + b.bits(LEXT[sym - 257]) as usize;
                        let ds = decode(&mut b, &dist) as usize;
                        let d = DBASE[ds] as usize + b.bits(DEXT[ds]) as usize;
                        for _ in 0..len {
                            out.push(out[out.len() - d]);
                        }
                    }
                }
            }
            _ => panic!("bad deflate block"),
        }
        if last == 1 {
            return out;
        }
    }
}

struct Image {
    w: usize,
    h: usize,
    bpp: usize,
    px: Vec<u8>,
}

impl Image {
    fn rgb(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let i = (y * self.w + x) * self.bpp;
        (self.px[i], self.px[i + 1], self.px[i + 2])
    }
}

fn png_size(png: &[u8]) -> (usize, usize) {
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]), "not a PNG");
    let be = |i: usize| u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
    (be(16), be(20))
}

fn decode_png(png: &[u8]) -> Image {
    let (w, h) = png_size(png);
    let (depth, color, interlace) = (png[24], png[25], png[28]);
    assert_eq!((depth, interlace), (8, 0), "unsupported PNG variant");
    let bpp = match color {
        2 => 3,
        6 => 4,
        c => panic!("unsupported PNG color type {}", c),
    };
    let mut idat = Vec::new();
    let mut i = 8;
    while i + 8 <= png.len() {
        let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
        if &png[i + 4..i + 8] == b"IDAT" {
            idat.extend_from_slice(&png[i + 8..i + 8 + len]);
        }
        i += 12 + len;
    }
    let raw = inflate(&idat);
    let stride = w * bpp;
    assert_eq!(raw.len(), (stride + 1) * h, "unexpected decompressed size");
    let mut px = vec![0u8; stride * h];
    for y in 0..h {
        let filter = raw[y * (stride + 1)];
        let line = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        for x in 0..stride {
            let a = if x >= bpp { px[y * stride + x - bpp] as i32 } else { 0 };
            let b = if y > 0 { px[(y - 1) * stride + x] as i32 } else { 0 };
            let c = if x >= bpp && y > 0 { px[(y - 1) * stride + x - bpp] as i32 } else { 0 };
            let pred = match filter {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
                }
                f => panic!("bad PNG filter {}", f),
            };
            px[y * stride + x] = (line[x] as i32 + pred) as u8;
        }
    }
    Image { w, h, bpp, px }
}

fn b64(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

// ------------------------------------------------------------- environment

static SEQ: AtomicU32 = AtomicU32::new(0);
/// Non-zero while a test wants new fake terminals to answer the graphics query late.
static SLOW_GFX: AtomicU32 = AtomicU32::new(0);

struct Env {
    home: PathBuf,
    base: String,
}

const APP: &str = r#"<!doctype html><meta charset="utf-8"><title>App</title>
<style>body { margin: 0; font: 16px sans-serif; background: #fff; } h1, form, p { margin: 10px; }</style>
<h1>App</h1>
<form id="login">
  <input id="user" aria-label="User" style="width:200px;height:30px">
  <button type="button" id="go" class="primary" style="width:120px;height:40px"
          onclick="window.goCount = (window.goCount || 0) + 1"><span>Log in</span></button>
</form>
<p id="para">Plain paragraph text</p>
<ul><li>one</li><li>two <a href="javascript:void 0">Second link</a></li></ul>
<div style="height:1500px"></div>
<a id="deep" href="javascript:void 0">Deep link</a>
<script>
window.events = [];
for (const t of ['mousedown', 'contextmenu']) addEventListener(t, e => events.push(t + ':' + e.button));
</script>"#;

fn page(path: &str) -> Option<String> {
    if path == "/app" {
        return Some(APP.to_string());
    }
    // /paint/<left rgb hex>/<right rgb hex>
    let mut seg = path.trim_start_matches('/').split('/');
    if seg.next() != Some("paint") {
        return None;
    }
    let (a, b) = (seg.next()?, seg.next()?);
    Some(format!(
        r#"<!doctype html><meta charset="utf-8"><title>Paint {a}</title>
<style>
html, body {{ margin: 0; height: 100%; overflow: hidden; }}
#l, #r {{ position: fixed; top: 0; bottom: 0; width: 50%; }}
#l {{ left: 0; background: #{a}; }}
#r {{ right: 0; background: #{b}; }}
#i {{ position: fixed; left: 20px; top: 20px; width: 300px; height: 30px; font-size: 16px; }}
</style>
<div id="l"></div><div id="r"></div><input id="i">
<script>
window.clicks = []; window.wheel = 0;
addEventListener('mousedown', e => clicks.push([e.clientX, e.clientY, e.isTrusted, e.button]));
addEventListener('wheel', e => {{ window.wheel = e.deltaY; }});
</script>"#
    ))
}

fn serve() -> String {
    const UNSAFE: &[u16] = &[1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668, 6669, 6679, 6697, 10080];
    let listener = loop {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        if !UNSAFE.contains(&l.local_addr().unwrap().port()) {
            break l;
        }
    };
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let resp = match page(head.split_whitespace().nth(1).unwrap_or("/")) {
                    Some(b) => format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", b.len(), b),
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });
    base
}

impl Env {
    fn new(name: &str) -> Env {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let home = std::env::temp_dir().join(format!(
            "kb-view-{}-{}-{:x}{}",
            std::process::id(), name, t & 0xffff_ffff, SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&home).unwrap();
        Env { home, base: serve() }
    }
    fn paint(&self, a: &str, b: &str) -> String {
        format!("{}/paint/{}/{}", self.base, a, b)
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_kb"));
        c.args(args).env("KB_HOME", &self.home).env_remove("KB_HEADED");
        c
    }
    fn ok(&self, args: &[&str]) -> String {
        let o = self.cmd(args).stdin(Stdio::null()).output().expect("run kb");
        assert!(o.status.success(), "kb {:?} failed: {}", args, String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    /// Poll a JS expression until it yields `want`.
    fn eval_until(&self, tab: &str, js: &str, want: &str) {
        let end = Instant::now() + Duration::from_secs(10);
        let mut got = String::new();
        while Instant::now() < end {
            got = self.ok(&["--tab", tab, "eval", js]);
            if got == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("eval {:?} on tab {}: wanted {:?}, last value {:?}", js, tab, want, got);
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Leave the daemon's side of the story next to the failure.
            let log = std::fs::read_to_string(self.home.join("daemon.log")).unwrap_or_default();
            let lines: Vec<&str> = log.lines().filter(|l| !["tabs -> ", "eval -> ", "pins -> "].iter().any(|n| l.contains(n))).collect();
            eprintln!("--- daemon.log (last lines)
{}", lines[lines.len().saturating_sub(40)..].join("
"));
        }
        let _ = self.cmd(&["stop"]).stdin(Stdio::null()).output();
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.home).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

// ---------------------------------------------------------- fake terminal

#[derive(Clone)]
struct Shown {
    id: u32,
    row: u32,
    col: u32,
    c: u32,
    r: u32,
    png: Vec<u8>,
}

struct TermState {
    raw: Vec<u8>,
    at: usize,
    cursor: (u32, u32),
    frames: Vec<Shown>,
    partial: Option<(Shown, Vec<u8>)>,
    clears: usize,
    // What this "terminal" claims to be.
    rows: u32,
    cols: u32,
    cell: (u32, u32), // (w, h)
    graphics: bool,
    pixel_mouse: bool,
    /// Delay the graphics reply so it arrives after DA1, as under Windows
    /// ConPTY where the console host answers DA1 by itself.
    slow_graphics: bool,
    late: Vec<u8>,
}

struct FakeTerm {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    st: Arc<Mutex<TermState>>,
}

fn kv(ctl: &str, key: &str) -> Option<String> {
    ctl.split(',').find_map(|p| p.strip_prefix(&format!("{}=", key)).map(String::from))
}

/// Consume complete escape sequences from the output; returns query replies.
fn digest(s: &mut TermState) -> Vec<u8> {
    let mut reply = Vec::new();
    loop {
        let b = &s.raw[s.at..];
        if b.is_empty() {
            break;
        }
        if b[0] != 0x1b {
            s.at += 1;
            continue;
        }
        if b.len() < 2 {
            break;
        }
        if b[1] == b'_' {
            let Some(end) = b.windows(2).position(|w| w == b"\x1b\\") else { break };
            let body = b[2..end].to_vec();
            s.at += end + 2;
            if body.first() != Some(&b'G') {
                continue;
            }
            let split = body.iter().position(|c| *c == b';').unwrap_or(body.len());
            let ctl = String::from_utf8_lossy(&body[1..split]).into_owned();
            let payload = body.get(split + 1..).unwrap_or(&[]).to_vec();
            match kv(&ctl, "a").as_deref() {
                Some("q") => {
                    if s.graphics {
                        let ok = format!("\x1b_Gi={};OK\x1b\\", kv(&ctl, "i").unwrap_or_default()).into_bytes();
                        if s.slow_graphics {
                            s.late.extend_from_slice(&ok);
                        } else {
                            reply.extend_from_slice(&ok);
                        }
                    }
                }
                Some("d") => s.clears += 1,
                Some("T") => {
                    let num = |k: &str| kv(&ctl, k).and_then(|v| v.parse().ok()).unwrap_or(0);
                    assert_eq!(kv(&ctl, "f").as_deref(), Some("100"), "frames must be sent as PNG");
                    let shown = Shown { id: num("i"), row: s.cursor.0, col: s.cursor.1, c: num("c"), r: num("r"), png: Vec::new() };
                    s.partial = Some((shown, payload));
                    if kv(&ctl, "m").as_deref() != Some("1") {
                        finish(s);
                    }
                }
                None => {
                    // Continuation chunk: only m= is present.
                    if let Some((_, acc)) = s.partial.as_mut() {
                        acc.extend_from_slice(&payload);
                    }
                    if kv(&ctl, "m").as_deref() != Some("1") {
                        finish(s);
                    }
                }
                _ => {}
            }
        } else if b[1] == b'[' {
            let Some(fin) = b[2..].iter().position(|c| (0x40..=0x7e).contains(c)) else { break };
            let body = String::from_utf8_lossy(&b[2..2 + fin]).into_owned();
            let last = b[2 + fin];
            s.at += fin + 3;
            let (cw, ch) = s.cell;
            match (body.as_str(), last) {
                (pos, b'H') => {
                    let mut n = pos.split(';').map(|v| v.parse().unwrap_or(1));
                    s.cursor = (n.next().unwrap_or(1), n.next().unwrap_or(1));
                }
                ("18", b't') => reply.extend_from_slice(format!("\x1b[8;{};{}t", s.rows, s.cols).as_bytes()),
                ("14", b't') => reply.extend_from_slice(format!("\x1b[4;{};{}t", s.rows * ch, s.cols * cw).as_bytes()),
                ("16", b't') => reply.extend_from_slice(format!("\x1b[6;{};{}t", ch, cw).as_bytes()),
                ("?u", _) | ("?", b'u') => reply.extend_from_slice(b"\x1b[?1u"),
                ("?1016$", b'p') => reply.extend_from_slice(if s.pixel_mouse { b"\x1b[?1016;1$y" } else { b"\x1b[?1016;0$y" }),
                ("", b'c') => reply.extend_from_slice(b"\x1b[?62;4c"),
                _ => {}
            }
        } else {
            s.at += 2;
        }
    }
    reply
}

fn finish(s: &mut TermState) {
    if let Some((mut shown, acc)) = s.partial.take() {
        shown.png = b64(&acc);
        s.frames.push(shown);
    }
}

impl FakeTerm {
    fn start(env: &Env, args: &[&str], rows: u32, cols: u32, graphics: bool, pixel_mouse: bool) -> FakeTerm {
        let mut child = env
            .cmd(args)
            .current_dir(&env.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kb view");
        let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let st = Arc::new(Mutex::new(TermState {
            raw: Vec::new(),
            at: 0,
            cursor: (1, 1),
            frames: Vec::new(),
            partial: None,
            clears: 0,
            rows,
            cols,
            cell: (10, 20),
            graphics,
            pixel_mouse,
            slow_graphics: std::env::var_os("KB_TEST_SLOW_GFX").is_some() || SLOW_GFX.load(Ordering::SeqCst) != 0,
            late: Vec::new(),
        }));
        let mut stdout = child.stdout.take().unwrap();
        let (st2, stdin2) = (st.clone(), stdin.clone());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let (reply, late) = {
                    let mut s = st2.lock().unwrap();
                    s.raw.extend_from_slice(&buf[..n]);
                    let reply = digest(&mut s);
                    (reply, std::mem::take(&mut s.late))
                };
                if !late.is_empty() {
                    let stdin3 = stdin2.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(250));
                        let _ = stdin3.lock().unwrap().write_all(&late);
                    });
                }
                if !reply.is_empty() {
                    let _ = stdin2.lock().unwrap().write_all(&reply);
                }
            }
        });
        FakeTerm { child, stdin, st }
    }

    fn send(&self, bytes: &[u8]) {
        let mut s = self.stdin.lock().unwrap();
        s.write_all(bytes).unwrap();
        s.flush().unwrap();
    }

    fn frame_count(&self) -> usize {
        self.st.lock().unwrap().frames.len()
    }

    /// First frame at index >= `from` that satisfies `pred`.
    fn wait_frame(&self, from: usize, what: &str, pred: impl Fn(&Shown) -> bool) -> Shown {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            {
                let s = self.st.lock().unwrap();
                if let Some(f) = s.frames.iter().skip(from).find(|f| pred(f)) {
                    return f.clone();
                }
            }
            if Instant::now() > end {
                let s = self.st.lock().unwrap();
                let seen: Vec<String> = s
                    .frames
                    .iter()
                    .skip(from)
                    .map(|f| format!("id={} at {},{} cells {}x{} png {:?}", f.id, f.row, f.col, f.c, f.r, png_size(&f.png)))
                    .collect();
                panic!("no frame matching [{}] within 20s; frames since index {}: {:#?}", what, from, seen);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.st.lock().unwrap().raw).into_owned()
    }

    fn resize(&self, rows: u32, cols: u32) {
        let mut s = self.st.lock().unwrap();
        (s.rows, s.cols) = (rows, cols);
    }

    /// Wait for the viewer to exit; returns (exit code, stderr).
    fn wait_exit(&mut self) -> (i32, String) {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut err = String::new();
                if let Some(mut e) = self.child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                return (status.code().unwrap_or(-1), err);
            }
            assert!(Instant::now() < end, "kb view did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn quit(&mut self) -> (i32, String) {
        self.send(b"\x02q");
        self.wait_exit()
    }
}

impl Drop for FakeTerm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn near(rgb: (u8, u8, u8), want: (u8, u8, u8)) -> bool {
    let d = |a: u8, b: u8| (a as i32 - b as i32).abs() <= 12;
    d(rgb.0, want.0) && d(rgb.1, want.1) && d(rgb.2, want.2)
}

/// Does the frame show `left`/`right` in the middle of each half?
fn halves(f: &Shown, left: (u8, u8, u8), right: (u8, u8, u8)) -> bool {
    let img = decode_png(&f.png);
    near(img.rgb(img.w / 4, img.h * 3 / 4), left) && near(img.rgb(img.w * 3 / 4, img.h * 3 / 4), right)
}

const RED: (u8, u8, u8) = (255, 0, 0);
const BLUE: (u8, u8, u8) = (0, 0, 255);
const GREEN: (u8, u8, u8) = (0, 255, 0);

// 100x30 cells of 10x20 px: one pane = 100 cols x 28 rows = 1000x560 px,
// below a 1-row title (20 px) and above a 1-row status line.
const ROWS: u32 = 30;
const COLS: u32 = 100;

// --------------------------------------------------------------------- tests

#[test]
fn v00_png_decoder_self_check() {
    // 2x2 RGB PNG (red, green / blue, white), stored deflate, filters 0 and 1.
    let raw: [u8; 14] = [0, 255, 0, 0, 0, 255, 0, 1, 0, 0, 255, 255, 255, 0];
    let mut z = vec![0x78, 0x01, 0x01, 14, 0, !14u8, 0xff];
    z.extend_from_slice(&raw);
    z.extend_from_slice(&[0, 0, 0, 0]);
    let mut png = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10];
    let mut chunk = |kind: &[u8], data: &[u8]| {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&[0; 4]);
    };
    chunk(b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0, 0, 0]);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    let img = decode_png(&png);
    assert_eq!((img.w, img.h), (2, 2));
    assert_eq!([img.rgb(0, 0), img.rgb(1, 0), img.rgb(0, 1), img.rgb(1, 1)], [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)]);
}

#[test]
fn v01_terminal_without_graphics_is_refused() {
    let env = Env::new("nogfx");
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, false, true);
    let (code, err) = term.wait_exit();
    assert_eq!(code, 3, "stderr: {}", err);
    assert!(err.contains("kitty graphics"), "{}", err);
    assert!(err.contains("size (cells):     100x30"), "probe details should be shown: {}", err);
    let raw = term.raw();
    assert!(raw.contains("\x1b[?1049h") && raw.contains("\x1b[?1049l"), "terminal must be restored");
    assert_eq!(term.frame_count(), 0);

    let mut probe = FakeTerm::start(&env, &["view", "--probe"], ROWS, COLS, true, true);
    assert_eq!(probe.wait_exit().0, 0);
    let report = probe.raw();
    assert!(report.contains("kitty graphics:   yes") && report.contains("size (pixels):    1000x600"), "{}", report);
    assert!(!report.contains("\x1b[?1049h"), "probe must not take over the screen");
}

/// Regression: found on a real terminal (Ghostty engine behind Windows
/// ConPTY). The graphics reply arrived after DA1 and the viewer refused a
/// terminal that does support graphics.
#[test]
fn v01b_graphics_reply_arriving_after_da1_is_still_accepted() {
    let env = Env::new("slowgfx");
    SLOW_GFX.fetch_add(1, Ordering::SeqCst);
    let mut probe = FakeTerm::start(&env, &["view", "--probe"], ROWS, COLS, true, true);
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, true, true);
    SLOW_GFX.fetch_sub(1, Ordering::SeqCst);
    assert_eq!(probe.wait_exit().0, 0);
    let report = probe.raw();
    assert!(report.contains("kitty graphics:   yes"), "{}", report);
    assert!(report.contains("pixel mouse:      yes") && report.contains("kitty keyboard:   yes"), "{}", report);
    term.wait_frame(0, "a frame despite the late reply", |f| png_size(&f.png) == (1000, 560));
    assert_eq!(term.quit().0, 0);
}

#[test]
fn v02_frames_are_live_pixels_and_viewer_coexists_with_cli() {
    let env = Env::new("stream");
    env.ok(&["open", &env.paint("ff0000", "0000ff")]);
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, true, true);

    // Rendered at exactly the pane's pixel size (after the viewport settles).
    let f = term.wait_frame(0, "red|blue page at 1000x560", |f| png_size(&f.png) == (1000, 560) && halves(f, RED, BLUE));
    assert_eq!((f.id, f.row, f.col, f.c, f.r), (1, 2, 1, 100, 28), "placement");
    env.eval_until("1", "[innerWidth, innerHeight, devicePixelRatio].join()", "1000,560,1");

    // Live: a change made by the CLI (the agent) shows up in the stream.
    let before = term.frame_count();
    env.ok(&["eval", "document.getElementById('l').style.background = '#00ff00'; 1"]);
    term.wait_frame(before, "green|blue after agent edit", |f| halves(f, GREEN, BLUE));

    // Agent navigation while the human is watching.
    let before = term.frame_count();
    env.ok(&["open", &env.paint("0000ff", "ff0000")]);
    term.wait_frame(before, "blue|red after agent navigation", |f| halves(f, BLUE, RED));
    let end = Instant::now() + Duration::from_secs(5);
    while !term.raw().contains("Paint 0000ff") {
        assert!(Instant::now() < end, "title bar never showed the new page title");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(env.ok(&["snapshot"]).contains("textbox"), "snapshot works while viewing");
    assert!(env.ok(&["tabs"]).starts_with("* 1 "), "viewer must not move the agent's current tab");

    let (code, err) = term.quit();
    assert_eq!(code, 0, "{}", err);
    let raw = term.raw();
    for restore in ["\x1b[?1049l", "\x1b[?25h", "\x1b[?1006l", "\x1b[?2004l", "\x1b[<u", "a=d,d=A"] {
        assert!(raw.ends_with(restore) || raw.contains(restore), "missing restore sequence {:?}", restore);
    }
    // The viewport emulation is gone once nobody is watching.
    let end = Instant::now() + Duration::from_secs(5);
    while env.ok(&["eval", "innerWidth"]) == "1000" {
        assert!(Instant::now() < end, "viewport override was not cleared on exit");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn v03_mouse_reaches_the_page_at_the_right_point() {
    let env = Env::new("mouse");
    env.ok(&["open", &env.paint("ff0000", "0000ff")]);

    // Pixel-precise mouse (SGR 1016): window px -> minus the 20px title row.
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, true, true);
    term.wait_frame(0, "first frame", |_| true);
    term.send(b"\x1b[<0;600;320M\x1b[<0;600;320m");
    env.eval_until("1", "JSON.stringify(clicks)", "[[600,300,true,0]]");
    // Ctrl+right button (a plain right-click pins a note instead: see w03).
    term.send(b"\x1b[<18;40;500M\x1b[<18;40;500m");
    env.eval_until("1", "JSON.stringify(clicks[1])", "[40,480,true,2]");
    term.send(b"\x1b[<65;600;320M"); // wheel down
    env.eval_until("1", "wheel > 0", "true");
    term.send(b"\x1b[<0;600;10M\x1b[<0;600;10m"); // title row: not forwarded
    std::thread::sleep(Duration::from_millis(300));
    env.eval_until("1", "clicks.length", "2");
    assert_eq!(term.quit().0, 0);

    // Cell-precision fallback: cell (61,17) -> centre px (605,330) -> css (605,310).
    env.ok(&["eval", "clicks.length = 0; 1"]);
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, true, false);
    term.wait_frame(0, "first frame", |_| true);
    term.send(b"\x1b[<0;61;17M\x1b[<0;61;17m");
    env.eval_until("1", "JSON.stringify(clicks)", "[[605,310,true,0]]");
    assert_eq!(term.quit().0, 0);
}

#[test]
fn v04_keyboard_text_ime_and_paste_reach_the_page() {
    let env = Env::new("keys");
    env.ok(&["open", &env.paint("ff0000", "0000ff")]);
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, true, true);
    term.wait_frame(0, "first frame", |_| true);
    let value = "document.getElementById('i').value";

    term.send(b"\x1b[<0;100;55M\x1b[<0;100;55m"); // click into the input
    env.eval_until("1", "document.activeElement.id", "i");
    term.send(b"hi A");
    env.eval_until("1", value, "hi A");
    term.send("한글".as_bytes()); // committed IME text
    env.eval_until("1", value, "hi A한글");
    term.send(b"\x1b[200~ pasted \"text\"\x1b[201~"); // bracketed paste
    env.eval_until("1", value, "hi A한글 pasted \"text\"");
    term.send(b"\x7f\x7f"); // Backspace
    env.eval_until("1", value, "hi A한글 pasted \"tex");
    term.send(b"\x1b[H"); // Home, then type at the start
    term.send(b">");
    env.eval_until("1", value, ">hi A한글 pasted \"tex");
    term.send(b"\x01"); // Ctrl+A selects all ...
    term.send(b"z"); // ... so typing replaces it
    env.eval_until("1", value, "z");
    term.send(b"\x1b[97;5u"); // Ctrl+A in the kitty keyboard encoding
    term.send(b"\x1b[3~"); // Delete
    env.eval_until("1", value, "");

    // URL prompt: ^B l, type, Enter -> the focused tab navigates.
    term.send(b"\x02l");
    term.send(env.paint("00ff00", "00ff00").as_bytes());
    term.send(b"\r");
    env.eval_until("1", "document.title", "Paint 00ff00");
    assert_eq!(term.quit().0, 0);
}

#[test]
fn v05_zoom_changes_the_layout_viewport_not_css() {
    let env = Env::new("zoom");
    env.ok(&["open", &env.paint("ff0000", "0000ff")]);
    let mut term = FakeTerm::start(&env, &["view", "--zoom", "1920"], ROWS, COLS, true, true);

    // 1920 CSS px wide at device scale 1; height keeps the pane's aspect.
    env.eval_until("1", "[innerWidth, innerHeight, devicePixelRatio].join()", "1920,1075,1");
    env.eval_until(
        "1",
        "[document.documentElement.style.zoom, document.body.style.zoom, getComputedStyle(document.body).zoom, getComputedStyle(document.documentElement).transform].join('|')",
        "||1|none",
    );
    let f = term.wait_frame(0, "zoomed frame scaled into the pane", |f| png_size(&f.png) == (1000, 560) && halves(f, RED, BLUE));
    assert_eq!((f.c, f.r), (100, 28));

    // A click still lands where the user points: px (600,320) -> css (1152,575).
    term.send(b"\x1b[<0;600;320M\x1b[<0;600;320m");
    env.eval_until("1", "clicks.length", "1");
    let click = env.ok(&["eval", "JSON.stringify(clicks[0])"]);
    assert!(click == "[1152,575,true,0]" || click == "[1152,576,true,0]", "{}", click);

    // ^B z cycles 1920 -> 2560 -> fit.
    term.send(b"\x02z");
    env.eval_until("1", "[innerWidth, innerHeight, devicePixelRatio].join()", "2560,1434,1");
    let before = term.frame_count();
    term.send(b"\x02z");
    env.eval_until("1", "innerWidth", "1000");
    term.wait_frame(before, "frame after returning to fit", |f| png_size(&f.png) == (1000, 560));
    assert!(term.raw().contains(" 2560 ") && term.raw().contains(" fit "), "title shows the zoom level");
    assert_eq!(term.quit().0, 0);
}

#[test]
fn v06_multi_view_layouts_show_separate_tabs() {
    let env = Env::new("grid");
    let colors = ["ff0000", "00ff00", "0000ff", "ffff00"];
    let rgb = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0)];
    env.ok(&["open", &env.paint(colors[0], "000000")]);
    for c in &colors[1..] {
        env.ok(&["tab", "new", &env.paint(c, "000000")]);
    }
    env.ok(&["tab", "1"]);

    // 2x2: each pane 49 cols x 13 rows = 490x260 px.
    let mut term = FakeTerm::start(&env, &["view", "--layout", "4"], ROWS, COLS, true, true);
    let at = [(2, 1), (2, 51), (16, 1), (16, 51)];
    for i in 0..4 {
        // The very first frame after a viewport change may still have the
        // old aspect; wait for the settled one.
        let f = term.wait_frame(0, &format!("pane {} colour at 490x260", i + 1), |f| {
            f.id == i as u32 + 1 && png_size(&f.png) == (490, 260) && halves(f, rgb[i], (0, 0, 0))
        });
        assert_eq!((f.row, f.col, f.c, f.r), (at[i].0, at[i].1, 49, 13), "pane {} placement", i + 1);
    }
    env.eval_until("3", "[innerWidth, innerHeight].join()", "490,260");

    // A click in the bottom-right pane goes to tab 4 only:
    // px (700,400) -> pane origin (500,300) -> css (200,100).
    term.send(b"\x1b[<0;700;400M\x1b[<0;700;400m");
    env.eval_until("4", "JSON.stringify(clicks)", "[[200,100,true,0]]");
    env.eval_until("1", "clicks.length", "0");
    // Keys follow the focus that click set.
    term.send(b"\x1b[<0;600;335M\x1b[<0;600;335m"); // inside tab 4's input
    term.send(b"four");
    env.eval_until("4", "document.getElementById('i').value", "four");
    env.eval_until("2", "document.getElementById('i').value", "");

    // 1x2: two panes of 49 cols x 28 rows, old images cleared.
    let (before, clears) = (term.frame_count(), term.st.lock().unwrap().clears);
    term.send(b"\x022");
    for i in 0..2 {
        let f = term.wait_frame(before, &format!("1x2 pane {}", i + 1), |f| f.id == i as u32 + 1 && png_size(&f.png) == (490, 560) && halves(f, rgb[i], (0, 0, 0)));
        assert_eq!((f.row, f.col, f.c, f.r), (2, [1, 51][i], 49, 28));
    }
    assert!(term.st.lock().unwrap().clears > clears, "old images must be deleted on re-layout");
    // Tabs no longer shown get their own viewport back.
    let end = Instant::now() + Duration::from_secs(5);
    while env.ok(&["--tab", "3", "eval", "innerWidth"]) == "490" {
        assert!(Instant::now() < end, "tab 3 still has the pane viewport");
        std::thread::sleep(Duration::from_millis(100));
    }

    // Single view, then move focus with the keyboard (^B 2, ^B right).
    term.send(b"\x022");
    term.send(b"\x02\x1b[C");
    term.send(b"\x1b[<0;10;10M\x1b[<0;10;10m"); // title row of pane 1: focus only
    term.send(b"\x02\x1b[C"); // -> pane 2
    term.send(b"\x1b[200~kbd\x1b[201~");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(term.quit().0, 0);
    assert_eq!(env.ok(&["tabs"]).lines().count(), 4, "layout changes must not close tabs");
}

// ------------------------------------------------------ stage 3: badges, pins

/// Visible pin notes of the main document, same format as LABELS.
const NOTES: &str = "(() => { const h = document.documentElement.querySelector(':scope > [data-kb-overlay]'); if (!h) return 'none'; \
    return [...h.shadowRoot.querySelectorAll('div')].filter(s => s.style.display !== 'none') \
    .map(s => s.textContent + '@' + Math.round(parseFloat(s.style.left)) + ',' + Math.round(parseFloat(s.style.top))).sort().join(' '); })()";

/// Visible overlay labels of the main document: "text@left,top" sorted, space separated.
const LABELS: &str = "(() => { const h = document.documentElement.querySelector(':scope > [data-kb-overlay]'); if (!h) return 'none'; \
    return [...h.shadowRoot.querySelectorAll('span')].filter(s => s.style.display !== 'none') \
    .map(s => s.textContent + '@' + Math.round(parseFloat(s.style.left)) + ',' + Math.round(parseFloat(s.style.top))).sort().join(' '); })()";

/// "x,y" of an element's top-left (or top-right) corner, rounded like the overlay does.
fn corner(env: &Env, sel: &str, right: bool) -> String {
    let side = if right { "right" } else { "left" };
    env.ok(&["eval", &format!("(() => {{ const r = document.querySelector('{}').getBoundingClientRect(); return Math.round(r.{}) + ',' + Math.round(r.top); }})()", sel, side)])
}

fn ref_of(snapshot: &str, needle: &str) -> String {
    let line = snapshot.lines().find(|l| l.contains(needle) && l.contains('@')).unwrap_or_else(|| panic!("no ref for {:?} in:\n{}", needle, snapshot));
    line[line.find('@').unwrap() + 1..].chars().take_while(|c| c.is_ascii_digit()).collect()
}

/// Any pixel of roughly `want` colour inside the box?
fn has_color(f: &Shown, x0: usize, y0: usize, w: usize, h: usize, want: (u8, u8, u8)) -> bool {
    let img = decode_png(&f.png);
    (y0..(y0 + h).min(img.h)).any(|y| (x0..(x0 + w).min(img.w)).any(|x| near(img.rgb(x, y), want)))
}

const BADGE_YELLOW: (u8, u8, u8) = (255, 212, 0);
const PIN_PINK: (u8, u8, u8) = (229, 0, 106);

#[test]
fn w01_badges_show_the_agents_refs_without_disturbing_the_page() {
    let env = Env::new("badges");
    env.ok(&["open", &format!("{}/app", env.base)]);
    let snap = env.ok(&["snapshot"]);
    let text = env.ok(&["text"]);
    let (user, go, link) = (ref_of(&snap, "textbox \"User\""), ref_of(&snap, "button \"Log in\""), ref_of(&snap, "link \"Second link\""));
    let deep = ref_of(&snap, "link \"Deep link\"");

    let on = env.ok(&["badges", "on"]);
    assert!(on.contains("badges on") && on.contains("labels=4"), "{}", on);
    // Same numbers as the snapshot, each at its element's top-left corner.
    // The link far below the fold has a label too, but it is not shown.
    let want = {
        let mut v = vec![
            format!("@{}@{}", user, corner(&env, "#user", false)),
            format!("@{}@{}", go, corner(&env, "#go", false)),
            format!("@{}@{}", link, corner(&env, "li a", false)),
        ];
        v.sort();
        v.join(" ")
    };
    env.eval_until("1", LABELS, &want);

    // Non-interference: the agent sees and does exactly what it did before.
    assert_eq!(env.ok(&["snapshot"]), snap, "snapshot must not change when badges are on");
    assert_eq!(env.ok(&["text"]), text, "page text must not change when badges are on");
    env.ok(&["click", &go]); // the badge sits on this button's corner
    env.eval_until("1", "window.goCount", "1");
    assert_eq!(env.ok(&["eval", "document.body.querySelector('[data-kb-overlay]')"]), "null", "overlay lives outside <body>");

    // Labels follow scrolling.
    env.ok(&["eval", "scrollTo(0, 100000); 1"]);
    env.eval_until("1", LABELS, &format!("@{}@{}", deep, corner(&env, "#deep", false)));
    env.ok(&["eval", "scrollTo(0, 0); 1"]);
    env.eval_until("1", LABELS, &want);

    // A new element gets a badge by itself, with the number a snapshot gives it.
    env.ok(&["eval", "const b = document.createElement('button'); b.id = 'late'; b.textContent = 'Late'; document.getElementById('login').append(b); 1"]);
    let end = Instant::now() + Duration::from_secs(10);
    while !env.ok(&["eval", LABELS]).contains(&corner(&env, "#late", false)) {
        assert!(Instant::now() < end, "no badge for the new element: {}", env.ok(&["eval", LABELS]));
        std::thread::sleep(Duration::from_millis(200));
    }
    let late = ref_of(&env.ok(&["snapshot"]), "button \"Late\"");
    assert!(env.ok(&["eval", LABELS]).contains(&format!("@{}@{}", late, corner(&env, "#late", false))));

    // Badges come back by themselves after a navigation, and go away on request.
    env.ok(&["open", &env.paint("ff0000", "0000ff")]);
    let input = ref_of(&env.ok(&["snapshot"]), "textbox");
    env.eval_until("1", LABELS, &format!("@{}@20,20", input));
    assert!(env.ok(&["badges", "off"]).contains("badges off"));
    env.eval_until("1", LABELS, "none");
    assert!(env.ok(&["badges"]).contains("badges on"), "bare `kb badges` toggles");
}

#[test]
fn w02_viewer_toggles_badges_and_scales_them_with_zoom() {
    let env = Env::new("vbadges");
    env.ok(&["open", &format!("{}/app", env.base)]);
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, true, true);
    term.wait_frame(0, "first frame", |f| png_size(&f.png) == (1000, 560));
    let at: Vec<usize> = corner(&env, "#go", false).split(',').map(|n| n.parse().unwrap()).collect();
    let on_button = |f: &Shown, c| png_size(&f.png) == (1000, 560) && has_color(f, at[0], at[1], 30, 14, c);
    assert!(!on_button(&term.wait_frame(0, "plain frame", |f| png_size(&f.png) == (1000, 560)), BADGE_YELLOW));

    term.send(b"\x02a"); // ^B a
    let before = term.frame_count();
    term.wait_frame(0, "badge pixels in the stream", |f| on_button(f, BADGE_YELLOW));
    env.eval_until("1", "document.documentElement.querySelector(':scope > [data-kb-overlay]')?.shadowRoot.querySelector('span')?.style.fontSize", "11px");
    term.send(b"\x02a");
    env.eval_until("1", LABELS, "none");
    term.wait_frame(before, "badge gone from the stream", |f| png_size(&f.png) == (1000, 560) && !has_color(f, at[0], at[1], 30, 14, BADGE_YELLOW));

    // Zoomed out to a 1920 px viewport in a 1000 px pane: labels grow 1.92x
    // so they are still readable after the frame is scaled down.
    term.send(b"\x02a");
    term.send(b"\x02z\x02z"); // fit -> 1280 -> 1920
    env.eval_until("1", "innerWidth", "1920");
    env.eval_until("1", "document.documentElement.querySelector(':scope > [data-kb-overlay]')?.shadowRoot.querySelector('span')?.style.fontSize", "21px");
    assert_eq!(term.quit().0, 0);
    // With the viewer gone the scale returns to normal.
    env.eval_until("1", "document.documentElement.querySelector(':scope > [data-kb-overlay]')?.shadowRoot.querySelector('span')?.style.fontSize", "11px");
}

#[test]
fn w03_right_click_pins_a_note_and_hands_it_to_the_agent() {
    let env = Env::new("pin");
    env.ok(&["open", &format!("{}/app", env.base)]);
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, true, true);
    term.wait_frame(0, "first frame", |f| png_size(&f.png) == (1000, 560));
    // Centre of the button's inner <span>, in window pixels (title row = 20 px).
    let c = env.ok(&["eval", "(() => { const r = document.querySelector('#go span').getBoundingClientRect(); return Math.round(r.left + r.width / 2) + ',' + Math.round(r.top + r.height / 2 + 20); })()"]);
    let (x, y) = c.split_once(',').unwrap();
    let right = |kind: char| format!("\x1b[<2;{};{}{}", x, y, kind).into_bytes();

    // Right-click -> prompt -> note (with Hangul) -> Enter.
    term.send(&right('M'));
    term.send(&right('m'));
    let end = Instant::now() + Duration::from_secs(5);
    while !term.raw().contains("pin #1 on button \"Log in\"") {
        assert!(Instant::now() < end, "no pin prompt in the status line");
        std::thread::sleep(Duration::from_millis(50));
    }
    term.send("색 바꿔줘 pls".as_bytes());
    term.send(b"\r");
    let end = Instant::now() + Duration::from_secs(5);
    let json = loop {
        let j = env.ok(&["pins", "--json"]);
        if j.contains("\"note\":\"색 바꿔줘 pls\"") {
            break j;
        }
        assert!(Instant::now() < end, "pin was not saved: {}", j);
        std::thread::sleep(Duration::from_millis(100));
    };
    // The click was on the inner <span>, the pin is on the button.
    for want in ["\"id\":1", "\"selector\":\"#go\"", "\"role\":\"button\"", "\"name\":\"Log in\"", "\"tag\":\"button\"", "\"attached\":true", "\"title\":\"App\"", "/app\""] {
        assert!(json.contains(want), "missing {} in {}", want, json);
    }
    // The page never saw the right-click.
    assert_eq!(env.ok(&["eval", "JSON.stringify(events)"]), "[]");
    // The pin carries a ref the agent can act on right away.
    let pin_ref = json.split("\"ref\":").nth(1).unwrap().chars().take_while(|c| c.is_ascii_digit()).collect::<String>();
    env.ok(&["click", &pin_ref]);
    env.eval_until("1", "window.goCount", "1");
    // Marker #1 sits on the element's top-right corner, and is in the stream.
    env.eval_until("1", LABELS, &format!("#1@{}", corner(&env, "#go", true)));
    // The note text is shown next to the marker, just right of the element.
    env.eval_until("1", NOTES, &format!("색 바꿔줘 pls@{}", corner(&env, "#go", true)));
    let tr: Vec<usize> = corner(&env, "#go", true).split(',').map(|n| n.parse().unwrap()).collect();
    term.wait_frame(0, "pin marker pixels", |f| png_size(&f.png) == (1000, 560) && has_color(f, tr[0] - 24, tr[1], 24, 14, PIN_PINK));

    // Markdown hand-off.
    let md = env.ok(&["pins"]);
    for want in ["# kb pins (1)", "## #1 색 바꿔줘 pls", "- page: App - ", &format!("- element: button \"Log in\" (ref @{}, tab 1)", pin_ref), "- selector: `#go`", "- text: \"Log in\"", "- box: ", "- created: 20"] {
        assert!(md.contains(want), "missing {:?} in:\n{}", want, md);
    }

    // Escape abandons a new pin.
    let p = env.ok(&["eval", "(() => { const r = document.querySelector('#para').getBoundingClientRect(); return Math.round(r.left + 20) + ',' + Math.round(r.top + r.height / 2 + 20); })()"]);
    let (px, py) = p.split_once(',').unwrap();
    term.send(format!("\x1b[<2;{};{}M\x1b[<2;{};{}m", px, py, px, py).as_bytes());
    let end = Instant::now() + Duration::from_secs(5);
    while !term.raw().contains("pin #2 on p") {
        assert!(Instant::now() < end, "no prompt for the paragraph pin");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The empty input bubble opens on the page, where the click landed.
    let left: i64 = env.ok(&["eval", "Math.round(document.querySelector('#para').getBoundingClientRect().left)"]).trim().parse().unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let n = env.ok(&["eval", NOTES]);
        if let Some(at) = n.split("type a note, Enter saves, Esc cancels@").nth(1) {
            let x: i64 = at.split(',').next().unwrap().parse().unwrap();
            assert!((x - left - 20).abs() <= 3, "bubble must sit at the click, not at the element's edge: {}", n);
            break;
        }
        assert!(Instant::now() < end, "no input bubble on the page: {}", n);
        std::thread::sleep(Duration::from_millis(50));
    }
    term.send(b"never mind");
    // What is typed shows up in that bubble, with a caret.
    let end = Instant::now() + Duration::from_secs(5);
    while !env.ok(&["eval", NOTES]).contains("never mind\u{258f}@") {
        assert!(Instant::now() < end, "typed note is not on the page: {}", env.ok(&["eval", NOTES]));
        std::thread::sleep(Duration::from_millis(50));
    }
    term.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(600));
    assert!(!env.ok(&["eval", NOTES]).contains("never mind"), "Escape must remove the bubble");
    assert!(env.ok(&["pins"]).starts_with("# kb pins (1)"), "Escape must not leave a pin behind");

    // Right-click the same element again: edit the existing note.
    term.send(&right('M'));
    term.send(&right('m'));
    term.send(b" now");
    term.send(b"\r");
    let end = Instant::now() + Duration::from_secs(5);
    while !env.ok(&["pins"]).contains("## #1 색 바꿔줘 pls now") {
        assert!(Instant::now() < end, "note was not edited:\n{}", env.ok(&["pins"]));
        std::thread::sleep(Duration::from_millis(100));
    }

    // ^B s writes both hand-off files next to the viewer.
    term.send(b"\x02s");
    let (md_file, json_file) = (env.home.join("kb-pins.md"), env.home.join("kb-pins.json"));
    let end = Instant::now() + Duration::from_secs(5);
    while !(md_file.exists() && json_file.exists()) {
        assert!(Instant::now() < end, "^B s did not write the pin files");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(std::fs::read_to_string(&md_file).unwrap().contains("## #1 색 바꿔줘 pls now"));
    let saved = std::fs::read_to_string(&json_file).unwrap();
    assert!(saved.starts_with("{\"pins\":[{") && saved.contains("\"selector\":\"#go\""), "{}", saved);

    // Ctrl+right-click is a real right-click for the page.
    env.ok(&["eval", "events.length = 0; 1"]); // drop the agent's own click from above
    term.send(format!("\x1b[<18;{};{}M\x1b[<18;{};{}m", x, y, x, y).as_bytes());
    env.eval_until("1", "JSON.stringify(events)", "[\"mousedown:2\",\"contextmenu:2\"]");

    // Clearing the note (Ctrl+U) and pressing Enter deletes the pin.
    term.send(&right('M'));
    term.send(&right('m'));
    term.send(b"\x15\r");
    let end = Instant::now() + Duration::from_secs(5);
    while !env.ok(&["pins"]).starts_with("# kb pins (0)") {
        assert!(Instant::now() < end, "empty note did not delete the pin");
        std::thread::sleep(Duration::from_millis(100));
    }
    env.eval_until("1", LABELS, "none");
    assert_eq!(term.quit().0, 0);
}

/// Regression from a real terminal multiplexer (Herdr): it keeps Ctrl+B and
/// the right mouse button for itself, so neither ever reaches kb.
#[test]
fn w05_works_without_ctrl_b_and_without_the_right_button() {
    let env = Env::new("nomux");
    env.ok(&["open", &format!("{}/app", env.base)]);
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, true, true);
    term.wait_frame(0, "first frame", |f| png_size(&f.png) == (1000, 560));
    let c = env.ok(&["eval", "(() => { const r = document.querySelector('#go').getBoundingClientRect(); return Math.round(r.left + r.width / 2) + ',' + Math.round(r.top + r.height / 2 + 20); })()"]);
    let (x, y) = c.split_once(',').unwrap();
    let wait_for = |term: &FakeTerm, what: &str| {
        let end = Instant::now() + Duration::from_secs(5);
        while !term.raw().contains(what) {
            assert!(Instant::now() < end, "status line never showed {:?}", what);
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // Ctrl+G is a leader too: legacy byte and kitty encoding.
    term.send(b"\x07z");
    env.eval_until("1", "innerWidth", "1280");
    term.send(b"\x1b[103;5uz");
    env.eval_until("1", "innerWidth", "1920");
    term.send(b"\x07z\x07z");
    env.eval_until("1", "innerWidth", "1000");

    // Leader m, then a plain left click, pins instead of clicking.
    term.send(b"\x07m");
    wait_for(&term, "pin: click the element");
    term.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x, y, x, y).as_bytes());
    wait_for(&term, "pin #1 on button \"Log in\"");
    term.send(b"via leader\r");
    let end = Instant::now() + Duration::from_secs(5);
    while !env.ok(&["pins"]).contains("## #1 via leader") {
        assert!(Instant::now() < end, "pin not saved:\n{}", env.ok(&["pins"]));
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(env.ok(&["eval", "JSON.stringify(events) + (window.goCount || 0)"]), "[]0", "the pinning click must not reach the page");
    // Pin mode is one-shot: the next click is a normal click again.
    term.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x, y, x, y).as_bytes());
    env.eval_until("1", "window.goCount", "1");

    // Alt+click pins as well (SGR modifier bit 8).
    env.ok(&["eval", "events.length = 0; 1"]);
    let p = env.ok(&["eval", "(() => { const r = document.querySelector('#user').getBoundingClientRect(); return Math.round(r.left + 20) + ',' + Math.round(r.top + r.height / 2 + 20); })()"]);
    let (ux, uy) = p.split_once(',').unwrap();
    term.send(format!("\x1b[<8;{};{}M\x1b[<8;{};{}m", ux, uy, ux, uy).as_bytes());
    wait_for(&term, "pin #2 on textbox \"User\"");
    term.send(b"\x1b"); // cancel
    assert_eq!(env.ok(&["eval", "JSON.stringify(events)"]), "[]");

    // Escape leaves an armed pin mode without pinning.
    term.send(b"\x07m");
    term.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(200));
    term.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x, y, x, y).as_bytes());
    env.eval_until("1", "window.goCount", "2");
    term.send(b"\x07q");
    assert_eq!(term.wait_exit().0, 0);
    assert!(env.ok(&["pins"]).starts_with("# kb pins (1)"));

    // A custom leader replaces the defaults: Ctrl+B is then just a key.
    let mut term = FakeTerm::start(&env, &["view", "--leader", "x"], ROWS, COLS, true, true);
    term.wait_frame(0, "first frame", |f| png_size(&f.png) == (1000, 560));
    term.send(b"\x02z");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.ok(&["eval", "innerWidth"]), "1000", "Ctrl+B must not be a leader with --leader x");
    term.send(b"\x18z");
    env.eval_until("1", "innerWidth", "1280");
    term.send(b"\x18q");
    assert_eq!(term.wait_exit().0, 0);

    // --keys reports what arrives, including a right-click when one does.
    let mut keys = FakeTerm::start(&env, &["view", "--keys"], ROWS, COLS, true, true);
    keys.send(b"\x07\x1b[<2;5;5M");
    wait_for(&keys, "Key { name: \"g\", mods: 2 }");
    wait_for(&keys, "Mouse { kind: Press, button: 2");
    keys.send(b"qqq");
    assert_eq!(keys.wait_exit().0, 0);
}

#[test]
fn w04_pins_survive_reloads_navigation_and_daemon_restart() {
    let env = Env::new("pinlife");
    let app = format!("{}/app", env.base);
    env.ok(&["open", &app]);
    let snap = env.ok(&["snapshot"]);
    let link = ref_of(&snap, "link \"Second link\"");

    // The agent can pin too; an element without an id gets a structural selector.
    let added = env.ok(&["pin", "add", &link, "check this link"]);
    assert!(added.contains("ok pin #1 on") && added.contains("link \"Second link\""), "{}", added);
    env.ok(&["pin", "add", &ref_of(&snap, "textbox \"User\""), "placeholder missing"]);
    let json = env.ok(&["pins", "--json"]);
    let sel = json.split("\"selector\":\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    assert_eq!(env.ok(&["eval", &format!("document.querySelector('{}').textContent", sel)]), "Second link", "selector {} must find the element", sel);
    env.eval_until("1", LABELS, &format!("#1@{} #2@{}", corner(&env, "li a", true), corner(&env, "#user", true)));

    // Files for the agent.
    let (md, js) = (env.home.join("out.md"), env.home.join("out.json"));
    assert!(env.ok(&["pins", "--save", md.to_str().unwrap()]).contains("saved 2 pins"));
    env.ok(&["pins", "--save", js.to_str().unwrap()]);
    assert!(std::fs::read_to_string(&md).unwrap().contains("## #2 placeholder missing"));
    assert!(std::fs::read_to_string(&js).unwrap().contains("\"note\":\"check this link\""));

    // Reload: refs die, pins re-attach by selector and get fresh refs.
    env.ok(&["reload"]);
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let md = env.ok(&["pins"]);
        if md.matches("(ref @").count() == 2 {
            let r = md.split("(ref @").nth(1).unwrap().chars().take_while(|c| c.is_ascii_digit()).collect::<String>();
            assert_ne!(r, link, "a reload must issue a new ref");
            assert_eq!(env.ok(&["text", &r]), "Second link");
            break;
        }
        assert!(Instant::now() < end, "pins did not re-attach after reload:\n{}", md);
        std::thread::sleep(Duration::from_millis(200));
    }
    env.eval_until("1", LABELS, &format!("#1@{} #2@{}", corner(&env, "li a", true), corner(&env, "#user", true)));

    // Elsewhere: still listed, clearly detached, no marker drawn.
    env.ok(&["open", &env.paint("ff0000", "0000ff")]);
    let away = env.ok(&["pins"]);
    assert_eq!(away.matches("detached").count(), 2, "{}", away);
    assert!(env.ok(&["pins", "--json"]).contains("\"attached\":false"));
    env.eval_until("1", LABELS, "none");

    // Daemon restart: pins are on disk.
    env.ok(&["stop"]);
    let cold = env.ok(&["pins"]);
    assert!(cold.contains("## #1 check this link") && cold.contains("## #2 placeholder missing"), "{}", cold);
    env.ok(&["open", &app]);
    let end = Instant::now() + Duration::from_secs(10);
    while env.ok(&["pins"]).contains("detached") {
        assert!(Instant::now() < end, "pins did not attach after restart");
        std::thread::sleep(Duration::from_millis(200));
    }
    env.eval_until("1", LABELS, &format!("#1@{} #2@{}", corner(&env, "li a", true), corner(&env, "#user", true)));

    // Editing and removing from the CLI; numbers are never reused.
    assert!(env.ok(&["pin", "note", "2", "needs", "a", "placeholder"]).contains("pin #2 saved"));
    assert!(env.ok(&["pins"]).contains("## #2 needs a placeholder"));
    env.ok(&["pin", "rm", "1"]);
    env.eval_until("1", LABELS, &format!("#2@{}", corner(&env, "#user", true)));
    let third = env.ok(&["pin", "add", &ref_of(&env.ok(&["snapshot"]), "button \"Log in\""), "third"]);
    assert!(third.contains("pin #3"), "{}", third);
    assert!(env.ok(&["pins", "clear"]).contains("cleared 2 pins"));
    assert!(env.ok(&["pins"]).contains("No pins."));
    env.eval_until("1", LABELS, "none");
    let o = env.cmd(&["pin", "rm", "9"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(o.status.code(), Some(5));
}

#[test]
fn v07_terminal_resize_reflows_the_view() {
    let env = Env::new("resize");
    env.ok(&["open", &env.paint("ff0000", "0000ff")]);
    let mut term = FakeTerm::start(&env, &["view"], ROWS, COLS, true, true);
    term.wait_frame(0, "initial size", |f| png_size(&f.png) == (1000, 560));

    let before = term.frame_count();
    term.resize(24, 80); // 800x480 px window -> pane 80 cols x 22 rows = 800x440
    let f = term.wait_frame(before, "resized frame", |f| png_size(&f.png) == (800, 440) && halves(f, RED, BLUE));
    assert_eq!((f.row, f.col, f.c, f.r), (2, 1, 80, 22));
    env.eval_until("1", "[innerWidth, innerHeight].join()", "800,440");
    assert_eq!(term.quit().0, 0);
}

#[test]
fn w06_viewer_speaks_the_chosen_language() {
    let env = Env::new("viewlang");
    env.ok(&["open", &format!("{}/app", env.base)]);
    let term = FakeTerm::start(&env, &["--lang", "ko", "view"], ROWS, COLS, true, true);
    term.wait_frame(0, "first frame", |f| png_size(&f.png) == (1000, 560));
    let wait_for = |term: &FakeTerm, what: &str| {
        let end = Instant::now() + Duration::from_secs(5);
        while !term.raw().contains(what) {
            assert!(Instant::now() < end, "status line never showed {:?}", what);
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    wait_for(&term, "kb view | Ctrl+G 다음 ? = 단축키");
    term.send(b"\x07");
    wait_for(&term, "1/2/4 분할");
    // Pin mode and the on-page input bubble use the same language.
    term.send(b"m");
    wait_for(&term, "핀: 메모할 요소를 클릭하세요");
    let c = env.ok(&["eval", "(() => { const r = document.querySelector('#go').getBoundingClientRect(); return Math.round(r.left + r.width / 2) + ',' + Math.round(r.top + r.height / 2 + 20); })()"]);
    let (x, y) = c.split_once(',').unwrap();
    term.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x, y, x, y).as_bytes());
    wait_for(&term, "핀 #1 (button");
    env.eval_until("1", &format!("({}).includes('메모 입력, Enter 저장, Esc 취소')", NOTES), "true");
    term.send("색 바꿔줘\r".as_bytes());
    let end = Instant::now() + Duration::from_secs(5);
    while !env.ok(&["pins"]).contains("## #1 색 바꿔줘") {
        assert!(Instant::now() < end, "pin was not saved");
        std::thread::sleep(Duration::from_millis(100));
    }
    // What the agent reads is unchanged.
    assert!(env.ok(&["pins"]).contains("- element: button \"Log in\""));
    term.send(b"\x07q");
}

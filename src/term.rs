//! Terminal input parser: bytes from the terminal -> keys, mouse, pastes and
//! replies to our capability/size queries. Pure and incremental, so the same
//! code runs against a real terminal and against the test harness.

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MouseKind {
    Press,
    Release,
    Move,
    Wheel,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Ev {
    /// Key name as `kb press` understands it, plus CDP modifier bits
    /// (alt 1, ctrl 2, meta 4, shift 8).
    Key { name: String, mods: i64 },
    /// Committed non-ASCII text (IME output such as Hangul).
    Text(String),
    /// `button`: 0 left, 1 middle, 2 right; for wheel 0 up, 1 down, 2 left, 3 right.
    /// `x`/`y` are 1-based cells, or 0-based pixels in SGR-pixel mode.
    Mouse { kind: MouseKind, button: u8, x: i64, y: i64, mods: i64 },
    Paste(String),
    Cells { rows: u32, cols: u32 },
    Pixels { h: u32, w: u32 },
    CellPx { h: u32, w: u32 },
    Graphics(bool),
    Keyboard(u32),
    PixelMouse(bool),
    Da1,
}

enum Step {
    Done(usize, Option<Ev>),
    Need,
}

#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    paste: bool,
}

/// Terminal modifier bits (shift 1, alt 2, ctrl 4, super 8) -> CDP bits.
fn cdp_mods(t: i64) -> i64 {
    (if t & 1 != 0 { 8 } else { 0 }) | (if t & 2 != 0 { 1 } else { 0 }) | (if t & 4 != 0 { 2 } else { 0 }) | (if t & 8 != 0 { 4 } else { 0 })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn key(name: &str, mods: i64) -> Option<Ev> {
    Some(Ev::Key { name: name.to_string(), mods })
}

impl Parser {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Ev>) {
        self.buf.extend_from_slice(bytes);
        loop {
            if self.buf.is_empty() {
                return;
            }
            match self.step() {
                Step::Done(n, ev) => {
                    self.buf.drain(..n);
                    out.extend(ev);
                }
                Step::Need => return,
            }
        }
    }

    /// True while an escape sequence is incomplete (a lone ESC may be the Escape key).
    pub fn pending(&self) -> bool {
        !self.buf.is_empty() && !self.paste
    }

    /// Called when no more bytes arrived for a while: a buffered ESC was the Escape key.
    pub fn flush(&mut self, out: &mut Vec<Ev>) {
        if self.paste || self.buf.is_empty() {
            return;
        }
        if self.buf[0] == 0x1b {
            out.extend(key("Escape", 0));
            self.buf.remove(0);
            let rest = std::mem::take(&mut self.buf);
            self.feed(&rest, out);
            if self.buf.first() == Some(&0x1b) {
                // Still stuck on a broken sequence: drop it.
                self.buf.clear();
            }
        } else {
            self.buf.clear();
        }
    }

    fn step(&mut self) -> Step {
        let b = &self.buf;
        if self.paste {
            return match find(b, b"\x1b[201~") {
                Some(end) => {
                    let text = String::from_utf8_lossy(&b[..end]).replace("\r\n", "\n").replace('\r', "\n");
                    self.paste = false;
                    Step::Done(end + 6, Some(Ev::Paste(text)))
                }
                None => Step::Need,
            };
        }
        match b[0] {
            0x1b => {
                let Some(&b1) = b.get(1) else { return Step::Need };
                match b1 {
                    b'[' => self.csi(),
                    b'O' => match b.get(2) {
                        None => Step::Need,
                        Some(c) => Step::Done(
                            3,
                            match c {
                                b'A' => key("ArrowUp", 0),
                                b'B' => key("ArrowDown", 0),
                                b'C' => key("ArrowRight", 0),
                                b'D' => key("ArrowLeft", 0),
                                b'H' => key("Home", 0),
                                b'F' => key("End", 0),
                                b'P' => key("F1", 0),
                                b'Q' => key("F2", 0),
                                b'R' => key("F3", 0),
                                b'S' => key("F4", 0),
                                _ => None,
                            },
                        ),
                    },
                    // APC / DCS / OSC strings end with ST (ESC \) or BEL.
                    b'_' | b'P' | b']' => {
                        let end = match (find(&b[2..], b"\x1b\\"), b[2..].iter().position(|c| *c == 7)) {
                            (Some(st), Some(bel)) if bel < st => (bel, 1),
                            (Some(st), _) => (st, 2),
                            (None, Some(bel)) => (bel, 1),
                            (None, None) => return Step::Need,
                        };
                        let body = &b[2..2 + end.0];
                        let ev = (b1 == b'_' && body.first() == Some(&b'G'))
                            .then(|| Ev::Graphics(find(body, b";OK").is_some()));
                        Step::Done(2 + end.0 + end.1, ev)
                    }
                    0x1b => Step::Done(1, key("Escape", 0)),
                    _ => {
                        // ESC + key = Alt+key.
                        self.buf.remove(0);
                        match self.step() {
                            Step::Done(n, Some(Ev::Key { name, mods })) => {
                                self.buf.insert(0, 0x1b);
                                Step::Done(n + 1, Some(Ev::Key { name, mods: mods | 1 }))
                            }
                            Step::Done(n, _) => {
                                self.buf.insert(0, 0x1b);
                                Step::Done(n + 1, None)
                            }
                            Step::Need => {
                                self.buf.insert(0, 0x1b);
                                Step::Need
                            }
                        }
                    }
                }
            }
            0x0d | 0x0a => Step::Done(1, key("Enter", 0)),
            0x09 => Step::Done(1, key("Tab", 0)),
            0x7f | 0x08 => Step::Done(1, key("Backspace", 0)),
            0x00 => Step::Done(1, key("Space", 2)),
            c @ 0x01..=0x1a => Step::Done(1, key(&((b'a' + c - 1) as char).to_string(), 2)),
            0x1c..=0x1f => Step::Done(1, None),
            c if c < 0x80 => Step::Done(1, key(&(c as char).to_string(), 0)),
            c => {
                let len = if c >= 0xf0 { 4 } else if c >= 0xe0 { 3 } else if c >= 0xc0 { 2 } else { 1 };
                if b.len() < len {
                    return Step::Need;
                }
                match std::str::from_utf8(&b[..len]) {
                    Ok(s) => Step::Done(len, Some(Ev::Text(s.to_string()))),
                    Err(_) => Step::Done(1, None),
                }
            }
        }
    }

    fn csi(&mut self) -> Step {
        let b = &self.buf;
        // ESC [ params(0x30-0x3f) intermediates(0x20-0x2f) final(0x40-0x7e)
        let Some(fin) = b[2..].iter().position(|c| (0x40..=0x7e).contains(c)) else {
            return if b.len() > 256 { Step::Done(b.len(), None) } else { Step::Need };
        };
        let total = fin + 3;
        let body = std::str::from_utf8(&b[2..2 + fin]).unwrap_or("");
        let last = b[2 + fin];
        let nums = |s: &str| -> Vec<i64> {
            s.split(';').map(|p| p.split(':').next().unwrap_or("").parse::<i64>().unwrap_or(0)).collect()
        };
        // Second parameter carries "1 + modifier bits" for keys.
        let mods = |n: &[i64]| cdp_mods(n.get(1).copied().unwrap_or(1).max(1) - 1);

        if let Some(rest) = body.strip_prefix('<') {
            // SGR mouse: ESC [ < button ; x ; y (M press | m release)
            let n = nums(rest);
            if n.len() != 3 || !(last == b'M' || last == b'm') {
                return Step::Done(total, None);
            }
            let (code, m) = (n[0], cdp_mods(((n[0] >> 2) & 1) | (((n[0] >> 3) & 1) << 1) | (((n[0] >> 4) & 1) << 2)));
            let button = (code & 3) as u8;
            let kind = if code & 64 != 0 {
                MouseKind::Wheel
            } else if code & 32 != 0 {
                MouseKind::Move
            } else if last == b'M' {
                MouseKind::Press
            } else {
                MouseKind::Release
            };
            return Step::Done(total, Some(Ev::Mouse { kind, button, x: n[1], y: n[2], mods: m }));
        }
        if let Some(rest) = body.strip_prefix('?') {
            let ev = match last {
                b'u' => Some(Ev::Keyboard(rest.parse().unwrap_or(0))),
                b'c' => Some(Ev::Da1),
                // DECRPM: ESC [ ? 1016 ; state $ y   (1 set, 3 permanently set)
                b'y' if rest.starts_with("1016;") => {
                    let state = rest[5..].trim_end_matches('$');
                    Some(Ev::PixelMouse(state == "1" || state == "3"))
                }
                _ => None,
            };
            return Step::Done(total, ev);
        }
        let n = nums(body);
        let m = mods(&n);
        let ev = match last {
            b'A' => key("ArrowUp", m),
            b'B' => key("ArrowDown", m),
            b'C' => key("ArrowRight", m),
            b'D' => key("ArrowLeft", m),
            b'H' => key("Home", m),
            b'F' => key("End", m),
            b'P' => key("F1", m),
            b'Q' => key("F2", m),
            b'S' => key("F4", m),
            b'Z' => key("Tab", 8),
            b'~' => match n[0] {
                200 => {
                    self.paste = true;
                    None
                }
                1 | 7 => key("Home", m),
                2 => key("Insert", m),
                3 => key("Delete", m),
                4 | 8 => key("End", m),
                5 => key("PageUp", m),
                6 => key("PageDown", m),
                11..=15 => key(&format!("F{}", n[0] - 10), m),
                17..=21 => key(&format!("F{}", n[0] - 11), m),
                23 | 24 => key(&format!("F{}", n[0] - 12), m),
                _ => None,
            },
            b't' => match n.as_slice() {
                [8, rows, cols] => Some(Ev::Cells { rows: *rows as u32, cols: *cols as u32 }),
                [4, h, w] => Some(Ev::Pixels { h: *h as u32, w: *w as u32 }),
                [6, h, w] => Some(Ev::CellPx { h: *h as u32, w: *w as u32 }),
                _ => None,
            },
            // Kitty keyboard protocol: ESC [ codepoint ; mods[:event] u
            b'u' => {
                let event = body.split(';').nth(1).and_then(|p| p.split(':').nth(1)).unwrap_or("1");
                if event == "3" {
                    None // key release
                } else {
                    match n[0] {
                        27 => key("Escape", m),
                        13 => key("Enter", m),
                        9 => key("Tab", m),
                        127 | 8 => key("Backspace", m),
                        32 => key("Space", m),
                        c if (33..127).contains(&c) => key(&(c as u8 as char).to_string(), m),
                        c if c >= 160 && c < 57344 => char::from_u32(c as u32).map(|ch| Ev::Text(ch.to_string())),
                        _ => None,
                    }
                }
            }
            _ => None,
        };
        Step::Done(total, ev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Ev> {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(bytes, &mut out);
        out
    }
    fn k(name: &str, mods: i64) -> Ev {
        Ev::Key { name: name.into(), mods }
    }

    #[test]
    fn plain_and_control_keys() {
        assert_eq!(parse(b"aZ\r\t\x7f"), vec![k("a", 0), k("Z", 0), k("Enter", 0), k("Tab", 0), k("Backspace", 0)]);
        assert_eq!(parse(b"\x02\x01"), vec![k("b", 2), k("a", 2)]);
        assert_eq!(parse(b"\x1bx"), vec![k("x", 1)]);
    }

    #[test]
    fn utf8_text_even_when_split() {
        let hangul = "한".as_bytes();
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(&hangul[..1], &mut out);
        assert!(out.is_empty());
        p.feed(&hangul[1..], &mut out);
        assert_eq!(out, vec![Ev::Text("한".into())]);
    }

    #[test]
    fn csi_keys() {
        assert_eq!(parse(b"\x1b[A\x1b[1;5C\x1b[3~\x1b[5;2~\x1bOP\x1b[15~\x1b[Z"), vec![
            k("ArrowUp", 0), k("ArrowRight", 2), k("Delete", 0), k("PageUp", 8), k("F1", 0), k("F5", 0), k("Tab", 8),
        ]);
    }

    #[test]
    fn kitty_keys() {
        assert_eq!(parse(b"\x1b[98;5u\x1b[27u\x1b[97;1:3u\x1b[13;2u"), vec![k("b", 2), k("Escape", 0), k("Enter", 8)]);
    }

    #[test]
    fn lone_escape_needs_flush() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.feed(b"\x1b", &mut out);
        assert!(out.is_empty() && p.pending());
        p.flush(&mut out);
        assert_eq!(out, vec![k("Escape", 0)]);
        assert!(!p.pending());
    }

    #[test]
    fn sgr_mouse() {
        use MouseKind::*;
        assert_eq!(parse(b"\x1b[<0;10;5M\x1b[<0;10;5m\x1b[<35;11;6M\x1b[<65;1;1M\x1b[<2;3;4M\x1b[<16;7;8M"), vec![
            Ev::Mouse { kind: Press, button: 0, x: 10, y: 5, mods: 0 },
            Ev::Mouse { kind: Release, button: 0, x: 10, y: 5, mods: 0 },
            Ev::Mouse { kind: Move, button: 3, x: 11, y: 6, mods: 0 },
            Ev::Mouse { kind: Wheel, button: 1, x: 1, y: 1, mods: 0 },
            Ev::Mouse { kind: Press, button: 2, x: 3, y: 4, mods: 0 },
            Ev::Mouse { kind: Press, button: 0, x: 7, y: 8, mods: 2 },
        ]);
    }

    #[test]
    fn paste_and_replies() {
        assert_eq!(parse(b"\x1b[200~hi\r\nthere\x1b[201~x"), vec![Ev::Paste("hi\nthere".into()), k("x", 0)]);
        assert_eq!(
            parse(b"\x1b[8;40;120t\x1b[4;800;1200t\x1b[6;20;10t\x1b_Gi=31;OK\x1b\\\x1b_Gi=31;ENOTSUP\x1b\\\x1b[?1u\x1b[?1016;1$y\x1b[?1016;0$y\x1b[?62;4c"),
            vec![
                Ev::Cells { rows: 40, cols: 120 }, Ev::Pixels { h: 800, w: 1200 }, Ev::CellPx { h: 20, w: 10 },
                Ev::Graphics(true), Ev::Graphics(false), Ev::Keyboard(1), Ev::PixelMouse(true), Ev::PixelMouse(false), Ev::Da1,
            ]
        );
    }

    #[test]
    fn split_sequences_resume() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        for chunk in [&b"\x1b[<0;1"[..], b"0;5", b"M\x1b_Gi=3", b"1;OK\x1b", b"\\"] {
            p.feed(chunk, &mut out);
        }
        assert_eq!(out, vec![Ev::Mouse { kind: MouseKind::Press, button: 0, x: 10, y: 5, mods: 0 }, Ev::Graphics(true)]);
    }
}

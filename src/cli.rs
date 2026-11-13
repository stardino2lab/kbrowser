//! Thin CLI: parse arguments, auto-start the daemon, send one request.

use crate::ipc;
use crate::json::{self, Value};
use crate::platform;
use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

const HELP: &str = r#"kb - terminal browser for AI agents and humans (Chrome over CDP)

USAGE  kb <command> [args] [--tab <id>] [--json] [--timeout <ms>]
       A background daemon and headless Chrome start on first use.

PAGE
  open <url>               navigate and wait for load
  snapshot                 accessibility tree; interactive elements get @N refs
      -i                   interactive elements only
      --scope <ref>        only the subtree under a ref
      --max-nodes <n>      cap output (default 2000, truncation is marked)
  text [ref]               visible text of the page, or of one element
  eval <js> | --stdin      run JavaScript, print the result
  screenshot <file.png>    save a PNG of the viewport
  wait <text|ref>          wait until text/element appears (--gone: disappears,
                           --text: treat a numeric argument as text)
  back | forward | reload

INPUT (real, trusted browser events)
  click <ref>              scrolls into view; fails if the element is covered
  fill <ref> <text>        text fields, <select> (value or label), checkbox
                           (true/false), file inputs (path). --stdin for text
  press <key>              Enter, Tab, Escape, ArrowDown, a, Control+a ...
      --snapshot           (click/fill/press/open) append a fresh snapshot
  dialog accept [text] | dialog dismiss      answer alert/confirm/prompt

TABS
  tabs                     list tabs (* = current)
  tab new [url] | tab <id> | tab close [id]
  --tab <id>               target a tab without changing the current one

BADGES AND PINS (shared by the human in `kb view` and the agent)
  badges [on|off|toggle]   draw each ref's @N on the page itself, so it shows
                           in the viewer and in kb screenshot
  pins                     notes pinned to elements, as markdown (--json)
      --save <file>        write them: .json gives JSON, anything else markdown
  pin add <ref> <note>     pin a note to an element
  pin note <N> <text>      change pin #N's note      pin rm <N>     pins clear
      A pin records page URL, selector, role/name, text and box. It carries
      a live ref while its element is on an open page; otherwise it is
      listed as detached.

VIEWER (human, live; needs kitty graphics: Kitty, Ghostty, WezTerm, Herdr)
  view [url]               stream tabs into the terminal; mouse and keys go
                           to the page. Shares tabs with the commands above.
      --layout 1|2|4       one tab, 1x2, or 2x2
      --zoom fit|1280|1920|2560   layout viewport width (not CSS zoom)
      --probe              report what this terminal supports, then exit
      --force              skip the graphics capability check
      --keys               print what each key/mouse action delivers, then q q q
      --leader <letter>    leader is Ctrl+<letter> (default: Ctrl+G or Ctrl+B)
      Keys: leader then 1/2/4 layout, arrows pane, z zoom, l url, t new tab,
            w close tab, n/p next/prev tab, r reload, [ ] history, q quit,
            a toggle @N badges, m pin, s save pins (kb-pins.md + .json), ? help
      Pin a note on an element: right-click it, or Alt+click it, or leader m
      then click it (Enter saves, Esc cancels, empty note deletes).
      Ctrl+right-click sends a real right-click.
      Inside tmux, Herdr and the like, Ctrl+B and the right button usually
      belong to the multiplexer: use Ctrl+G and leader m.

DAEMON
  status | stop            --headed on the first command shows the window

REFS  Write refs as plain numbers: kb click 3   (@3 and e3 also work, but
      PowerShell swallows a bare @3). A ref keeps its number for as long as
      its element exists; it goes stale when the element is removed or the
      page navigates. Then run kb snapshot again.

EXIT  0 ok | 1 usage | 2 stale ref | 3 daemon/Chrome | 4 timeout
      5 not actionable (covered, hidden, disabled) | 6 no such tab
      7 JavaScript exception | 8 dialog open
      Errors go to stderr as  error[code]: message  (JSON with --json).

LANGUAGE
  lang [code|auto]         show or set the language (remembered): en ko ja zh es fr de
  --lang <code>            for this run only. KB_LANG does the same.
                           Command output, JSON and error codes are always English.

ENV   KB_HOME state dir | KB_CHROME browser executable | KB_HEADED=1 | KB_LANG
"#;

const COMMANDS: &[&str] = &[
    "open", "snapshot", "click", "fill", "press", "eval", "text", "wait", "screenshot", "back",
    "forward", "reload", "dialog", "tabs", "tab", "status", "stop", "badges", "pin", "pins",
];

fn exit_code(code: &str) -> i32 {
    match code {
        "usage" => 1,
        "stale_ref" => 2,
        "timeout" => 4,
        "not_actionable" => 5,
        "no_such_tab" => 6,
        "eval_error" => 7,
        "dialog" => 8,
        _ => 3,
    }
}

fn fail(json_out: bool, code: &str, msg: &str, hint: &str) -> i32 {
    if json_out {
        eprintln!("{}", obj! { "ok" => false, "error" => obj! { "code" => code, "message" => msg, "hint" => hint } });
    } else {
        eprintln!("error[{}]: {}", code, msg);
        if !hint.is_empty() {
            eprintln!("hint: {}", hint);
        }
        // The lines above are a stable contract; this one is for the reader.
        if let Some(why) = crate::i18n::explain(code) {
            eprintln!("({})", why);
        }
    }
    exit_code(code)
}

/// Connect to a live daemon and authenticate. Liveness is "hello succeeds";
/// the pid in daemon.json is never trusted.
fn connect(home: &Path) -> Option<TcpStream> {
    let info = json::parse(&fs::read_to_string(home.join("daemon.json")).ok()?).ok()?;
    let port = info.get("port").as_i64()? as u16;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let hello = obj! { "cmd" => "hello", "token" => info.get("token").str(), "protocol" => ipc::PROTOCOL_VERSION };
    ipc::write_json(&mut s, ipc::T_REQUEST, &hello).ok()?;
    let ack = ipc::read_json(&mut s, ipc::T_RESPONSE).ok()?;
    (ack.get("ok").as_bool() == Some(true)).then_some(s)
}

fn connect_or_spawn(home: &Path) -> Result<TcpStream, String> {
    if let Some(s) = connect(home) {
        return Ok(s);
    }
    fs::create_dir_all(home).map_err(|e| format!("cannot create {}: {}", home.display(), e))?;
    let err_file = home.join("daemon.err");
    let _ = fs::remove_file(&err_file);
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own executable: {}", e))?;
    platform::spawn_daemon(&exe).map_err(|e| format!("cannot start daemon: {}", e))?;
    let end = Instant::now() + Duration::from_secs(45);
    while Instant::now() < end {
        if let Some(s) = connect(home) {
            return Ok(s);
        }
        if let Ok(msg) = fs::read_to_string(&err_file) {
            return Err(format!("daemon failed to start: {}", msg.trim()));
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Err(format!("daemon did not start in 45s (see {})", home.join("daemon.log").display()))
}

/// Write a `kb pins` result to a file: JSON for `.json`, markdown otherwise.
pub fn save_pins(result: &Value, path: &str) -> std::io::Result<usize> {
    let pins = result.get("pins");
    let body = if path.to_ascii_lowercase().ends_with(".json") {
        obj! { "pins" => pins.clone() }.to_string()
    } else {
        format!("{}\n", result.get("text").str())
    };
    fs::write(path, body)?;
    Ok(pins.arr().len())
}

/// Long-lived authenticated connection for the viewer (starts the daemon if needed).
pub fn stream() -> Result<TcpStream, String> {
    connect_or_spawn(&platform::home_dir())
}

/// One request/response on a fresh connection. Err = (code, message, hint).
pub fn request(req: &Value) -> Result<Value, (String, String, String)> {
    let lost = |e: String| ("daemon".to_string(), e, "retry the command".to_string());
    let mut s = connect_or_spawn(&platform::home_dir()).map_err(lost)?;
    let budget = req.get("timeout").as_i64().unwrap_or(30_000).max(0) as u64 + 60_000;
    s.set_read_timeout(Some(Duration::from_millis(budget))).ok();
    let resp = ipc::write_json(&mut s, ipc::T_REQUEST, req)
        .and_then(|_| ipc::read_json(&mut s, ipc::T_RESPONSE))
        .map_err(|e| lost(format!("lost connection to daemon: {}", e)))?;
    if resp.get("ok").as_bool() == Some(true) {
        Ok(resp.get("result").clone())
    } else {
        let e = resp.get("error");
        Err((e.get("code").as_str().unwrap_or("daemon").to_string(), e.get("message").str().to_string(), e.get("hint").str().to_string()))
    }
}

pub fn run(argv: Vec<String>) -> i32 {
    let mut pos: Vec<String> = Vec::new();
    let mut req = obj! {};
    let mut json_out = false;
    let mut stdin_arg = false;
    let mut it = argv.into_iter();
    let mut literal = false;
    while let Some(a) = it.next() {
        if literal {
            pos.push(a);
            continue;
        }
        let mut value = |name: &str| it.next().ok_or(format!("{} needs a value", name));
        let parsed: Result<(), String> = (|| {
            match a.as_str() {
                "--" => literal = true,
                "--json" => json_out = true,
                "--stdin" => stdin_arg = true,
                "--headed" => std::env::set_var("KB_HEADED", "1"),
                "--gone" => req.set("gone", true),
                "--text" => req.set("as_text", true),
                "--snapshot" => req.set("snapshot", true),
                "-i" | "--interactive" => req.set("interactive", true),
                "--scope" => req.set("scope", value("--scope")?),
                "--zoom" => req.set("zoom", value("--zoom")?),
                "--save" => req.set("save", value("--save")?),
                "--probe" => req.set("probe", true),
                "--keys" => req.set("keys", true),
                "--leader" => req.set("leader", value("--leader")?.to_ascii_lowercase()),
                "--force" => req.set("force", true),
                "--lang" => {
                    let v = value("--lang")?;
                    if !crate::i18n::valid(&v) {
                        return Err(format!("--lang: unknown language {:?} (use {} or auto)", v, crate::i18n::CODES.join(" ")));
                    }
                    std::env::set_var("KB_LANG", v);
                }
                "--tab" | "--timeout" | "--max-nodes" | "--layout" => {
                    let v = value(&a)?;
                    let n: i64 = v.parse().map_err(|_| format!("{} needs a number, got {:?}", a, v))?;
                    req.set(&a[2..].replace('-', "_"), n);
                }
                _ => pos.push(a.clone()),
            }
            Ok(())
        })();
        if let Err(e) = parsed {
            return fail(json_out, "usage", &e, "run: kb help");
        }
    }

    let cmd = match pos.first().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => {
            print!("{}", crate::i18n::help().unwrap_or(HELP));
            return 0;
        }
        Some("lang") => {
            let file = platform::home_dir().join("lang");
            let Some(want) = pos.get(1) else {
                println!("{}", crate::i18n::code(crate::i18n::lang()));
                return 0;
            };
            if !crate::i18n::valid(want) {
                let msg = format!("unknown language {:?} (use {} or auto)", want, crate::i18n::CODES.join(" "));
                return fail(json_out, "usage", &msg, "run: kb help");
            }
            let saved = fs::create_dir_all(platform::home_dir()).and_then(|_| fs::write(&file, want.trim().to_ascii_lowercase()));
            return match saved {
                Ok(()) => {
                    println!("ok language {}", want.trim().to_ascii_lowercase());
                    0
                }
                Err(e) => fail(json_out, "daemon", &format!("cannot write {}: {}", file.display(), e), ""),
            };
        }
        Some("daemon") => return crate::daemon::run(),
        Some("view") => return crate::view::run(&req, pos.get(1).map(String::as_str)),
        Some(c) if COMMANDS.contains(&c) => c.to_string(),
        Some(c) => return fail(json_out, "usage", &format!("unknown command: {}", c), "run: kb help"),
    };
    let mut args: Vec<Value> = pos[1..].iter().map(Value::from).collect();
    if stdin_arg {
        let mut text = String::new();
        if std::io::stdin().read_to_string(&mut text).is_err() {
            return fail(json_out, "usage", "cannot read stdin as UTF-8", "");
        }
        args.push(Value::from(text.strip_suffix('\n').map(|t| t.strip_suffix('\r').unwrap_or(t)).unwrap_or(&text)));
    }
    // Reject obviously incomplete commands before a daemon or browser starts.
    let need = match cmd.as_str() {
        "fill" => 2,
        "open" | "click" | "press" | "eval" | "wait" | "screenshot" | "dialog" | "tab" | "pin" => 1,
        _ => 0,
    };
    if args.len() < need {
        return fail(json_out, "usage", &format!("{} needs {} argument(s), got {}", cmd, need, args.len()), "run: kb help");
    }
    req.set("id", 1);
    req.set("cmd", &cmd);
    req.set("args", args);

    let home = platform::home_dir();
    let stream = if cmd == "stop" || cmd == "status" {
        // These never start a daemon.
        match connect(&home) {
            Some(s) => Ok(s),
            None if cmd == "stop" => {
                println!("ok not running");
                return 0;
            }
            None => Err("daemon is not running".to_string()),
        }
    } else {
        connect_or_spawn(&home)
    };
    let mut s = match stream {
        Ok(s) => s,
        Err(e) => return fail(json_out, "daemon", &e, "any command except status/stop starts it"),
    };

    let budget = req.get("timeout").as_i64().unwrap_or(30_000).max(0) as u64 + 60_000;
    s.set_read_timeout(Some(Duration::from_millis(budget))).ok();
    let resp = ipc::write_json(&mut s, ipc::T_REQUEST, &req).and_then(|_| ipc::read_json(&mut s, ipc::T_RESPONSE));
    let resp = match resp {
        Ok(r) => r,
        Err(e) => return fail(json_out, "daemon", &format!("lost connection to daemon: {}", e), "retry the command"),
    };
    if cmd == "stop" {
        // Return only once the daemon (and Chrome) are gone: it closes the socket on exit.
        s.set_read_timeout(Some(Duration::from_secs(15))).ok();
        let mut sink = [0u8; 64];
        while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    }
    if resp.get("ok").as_bool() == Some(true) {
        let result = resp.get("result");
        if let (Some(path), "pins") = (req.get("save").as_str(), cmd.as_str()) {
            // Written here, not by the daemon: the path is relative to the caller.
            return match save_pins(result, path) {
                Ok(n) => {
                    println!("ok saved {} pins to {}", n, path);
                    0
                }
                Err(e) => fail(json_out, "usage", &format!("cannot write {}: {}", path, e), ""),
            };
        }
        if json_out {
            println!("{}", result);
        } else if !result.get("text").str().is_empty() {
            println!("{}", result.get("text").str());
        }
        0
    } else {
        let e = resp.get("error");
        fail(json_out, e.get("code").as_str().unwrap_or("daemon"), e.get("message").str(), e.get("hint").str())
    }
}

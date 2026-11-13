//! Stage 1 end-to-end tests: the real `kb` binary driving real headless
//! Chrome against a local fixture server. Each test gets its own state dir,
//! daemon and browser. The fixtures are built so that a fake implementation
//! (setting `.value`, calling `.click()`, hardcoding output) fails.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SEQ: AtomicU32 = AtomicU32::new(0);

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

struct Env {
    home: PathBuf,
    base: String,
    nonce: String,
}

fn page(nonce: &str, path: &str) -> Option<String> {
    let body = match path {
        "/form" => r#"<title>Form NONCE</title>
<h1>Sign in NONCE</h1>
<input id="email" aria-label="Email">
<input id="ctl" aria-label="Controlled">
<select id="color" aria-label="Color"><option value="r">Red</option><option value="b">Blue</option></select>
<label><input id="agree" type="checkbox"> Agree</label>
<button id="go">Log in</button>
<a href="/late" target="_blank">Popup</a>
<div id="out"></div>
<div style="height:3000px"></div>
<button id="far" onclick="document.title='far-clicked'">Far away</button>
<script>
window.log = [];
const email = document.getElementById('email'), ctl = document.getElementById('ctl');
for (const type of ['keydown', 'beforeinput', 'input', 'change'])
  email.addEventListener(type, e => log.push(type + ':' + e.isTrusted));
document.getElementById('go').addEventListener('click', e => {
  log.push('click:' + e.isTrusted);
  document.getElementById('out').textContent = 'hello ' + email.value;
});
// Framework-style controlled input: only trusted input events are accepted,
// anything written through the DOM property is reverted.
let real = '';
ctl.addEventListener('input', e => { if (e.isTrusted) real = ctl.value; });
setInterval(() => { if (ctl.value !== real) ctl.value = real; }, 15);
</script>"#,
        "/overlay" => r#"<title>Overlay</title>
<button id="b" onclick="window.clicked=true">Covered</button>
<div id="cover" style="position:fixed;inset:0;background:rgba(0,0,0,.4)"></div>
<script>window.clicked = false;</script>"#,
        // Slightly taller than the viewport: the link needs a small scroll, and
        // the overlay case must still be detected once the page is scrolled.
        "/scrolled" => r#"<title>Scrolled</title>
<div id="spacer" style="height:690px">spacer</div>
<p id="para">Some text around <a href="javascript:void 0" onclick="window.linked=1">Deep link</a> here.</p>
<button id="b2" onclick="window.b2=1">Low button</button>
<div style="height:400px"></div>
<script>window.linked = 0; window.b2 = 0;</script>"#,
        "/iframe" => r#"<title>Outer NONCE</title><h1>Outer</h1>
<div style="height:120px"></div>
<iframe src="/inner" style="margin-left:200px;width:400px;height:200px"></iframe>"#,
        "/inner" => r#"<title>Inner</title><input aria-label="Inner field">
<button onclick="parent.document.title='inner-clicked'">Inner button</button>"#,
        "/late" => r#"<title>Late NONCE</title><p id="first">early</p>
<script>
setTimeout(() => { const p = document.createElement('p'); p.textContent = 'late-NONCE'; document.body.append(p); }, 500);
setTimeout(() => document.getElementById('first').remove(), 700);
</script>"#,
        "/spa" => r#"<title>Spa</title>
<button onclick="window.stay=(window.stay||0)+1">Stay</button>
<div id="slot"><button id="target">Target</button></div>
<button onclick="history.pushState({}, '', '/pushed')">Push</button>
<button onclick="document.getElementById('slot').innerHTML='<button>Target</button>'">Rerender</button>"#,
        "/alert" => r#"<title>Alert</title><button onclick="alert('hi NONCE'); window.after=1">Boom</button>"#,
        "/pushed" => "<title>Pushed</title>",
        _ => return None,
    };
    Some(format!("<!doctype html><meta charset=\"utf-8\">{}", body.replace("NONCE", nonce)))
}

fn serve(nonce: String) -> String {
    // Chrome refuses to load from "unsafe" ports (ERR_UNSAFE_PORT); the OS
    // can hand one out, so rebind until we get a port Chrome accepts.
    const UNSAFE: &[u16] = &[
        1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668, 6669, 6679, 6697, 10080,
    ];
    let listener = loop {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        if !UNSAFE.contains(&l.local_addr().unwrap().port()) {
            break l;
        }
    };
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let nonce = nonce.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/");
                let resp = match page(&nonce, path) {
                    Some(b) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        b.len(), b
                    ),
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
        let nonce = format!("n{:x}{}", t & 0xffff_ffff, SEQ.fetch_add(1, Ordering::SeqCst));
        let home = std::env::temp_dir().join(format!("kb-e2e-{}-{}-{}", std::process::id(), name, nonce));
        std::fs::create_dir_all(&home).unwrap();
        Env { home, base: serve(nonce.clone()), nonce }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Run `kb` with captured pipes; panics if it does not exit (a daemon
    /// holding our pipe would show up here as a hang).
    fn run_stdin(&self, args: &[&str], stdin: Option<&str>) -> Out {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kb"))
            .args(args)
            .env("KB_HOME", &self.home)
            .env_remove("KB_HEADED")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kb");
        if let Some(text) = stdin {
            child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
        }
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(o) => o.expect("kb output"),
            Err(_) => {
                kill(pid);
                panic!("kb {:?} did not exit within 120s (pipe held open?)", args);
            }
        };
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn run(&self, args: &[&str]) -> Out {
        self.run_stdin(args, None)
    }

    /// Run and require success; returns trimmed stdout.
    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert_eq!(o.code, 0, "kb {:?} failed\nstdout: {}\nstderr: {}", args, o.stdout, o.stderr);
        o.stdout.trim().to_string()
    }

    fn eval(&self, js: &str) -> String {
        self.ok(&["eval", js])
    }

    /// Value of a top-level number field in `kb status --json`.
    fn status_num(&self, key: &str) -> u32 {
        let s = self.ok(&["status", "--json"]);
        let tail = s.split(&format!("\"{}\":", key)).nth(1).unwrap_or_else(|| panic!("no {} in {}", key, s));
        tail.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap()
    }

    /// Command lines of live processes that mention this test's state dir
    /// (Chrome and all its children carry it in --user-data-dir).
    fn procs(&self) -> Vec<String> {
        let home = self.home.to_string_lossy().into_owned();
        procs_matching(&home)
    }

    fn wait_no_procs(&self, what: &str) {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let p = self.procs();
            if p.is_empty() {
                return;
            }
            assert!(Instant::now() < end, "{}: processes still alive: {:#?}", what, p);
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["stop"]);
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.home).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(windows)]
fn procs_matching(needle: &str) -> Vec<String> {
    let script = format!(
        "Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -ne $PID -and $_.CommandLine -and $_.CommandLine.Contains('{}') }} | ForEach-Object {{ $_.CommandLine }}",
        needle.replace('\'', "''")
    );
    let out = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

#[cfg(unix)]
fn procs_matching(needle: &str) -> Vec<String> {
    let mut v = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().parse::<u32>().ok() == Some(std::process::id()) {
            continue;
        }
        if let Ok(raw) = std::fs::read(e.path().join("cmdline")) {
            let cmd = String::from_utf8_lossy(&raw).replace('\0', " ");
            if cmd.contains(needle) {
                v.push(cmd);
            }
        }
    }
    v
}

#[cfg(windows)]
fn kill(pid: u32) {
    let _ = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).output();
}

#[cfg(unix)]
fn kill(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    let out = Command::new("tasklist").args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{}\"", pid))
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // A killed daemon may linger as a zombie until init reaps it.
    match std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
        Ok(s) => !s.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'),
        Err(_) => false,
    }
}

/// Ref number on the snapshot line containing `needle`.
fn ref_of(snapshot: &str, needle: &str) -> String {
    let line = snapshot
        .lines()
        .find(|l| l.contains(needle) && l.contains('@'))
        .unwrap_or_else(|| panic!("no ref line with {:?} in:\n{}", needle, snapshot));
    let tail = &line[line.find('@').unwrap() + 1..];
    tail.chars().take_while(|c| c.is_ascii_digit()).collect()
}

#[test]
fn t01_concurrent_autostart_yields_one_daemon() {
    let env = Env::new("race");
    let outs: Vec<Out> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..3).map(|_| s.spawn(|| env.run(&["tabs"]))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for o in &outs {
        assert_eq!(o.code, 0, "stderr: {}", o.stderr);
        assert!(o.stdout.contains("about:blank"), "tabs output: {}", o.stdout);
    }
    assert!(alive(env.status_num("pid")));
    // One browser process (the only Chrome process without --type=).
    let browsers: Vec<String> = env.procs().into_iter().filter(|c| !c.contains("--type=")).collect();
    assert_eq!(browsers.len(), 1, "expected exactly one Chrome browser process: {:#?}", browsers);
}

#[test]
fn t02_snapshot_reflects_live_page() {
    let env = Env::new("snap");
    let opened = env.ok(&["open", &env.url("/form")]);
    assert!(opened.contains("navigated=yes") && opened.contains(&format!("Form {}", env.nonce)), "{}", opened);
    let snap = env.ok(&["snapshot"]);
    assert!(snap.starts_with("[tab 1] Form "), "{}", snap);
    assert!(snap.contains(&format!("heading \"Sign in {}\"", env.nonce)), "{}", snap);
    for needle in ["textbox \"Email\"", "button \"Log in\"", "combobox \"Color\"", "checkbox \"Agree\"", "link \"Popup\""] {
        assert!(snap.lines().any(|l| l.contains(needle) && l.contains('@')), "missing ref for {}:\n{}", needle, snap);
    }
    assert!(!snap.lines().any(|l| l.contains("heading") && l.contains('@')), "heading must not get a ref");

    let interactive = env.ok(&["snapshot", "-i"]);
    assert!(!interactive.contains("heading") && interactive.contains("textbox \"Email\""), "{}", interactive);
    let json = env.ok(&["snapshot", "--json"]);
    assert!(json.starts_with('{') && json.contains("\"ref\":") && json.contains(&env.nonce), "{}", json);
    let capped = env.ok(&["snapshot", "--max-nodes", "2"]);
    assert!(capped.contains("truncated"), "{}", capped);

    assert_eq!(env.ok(&["text", &ref_of(&snap, "button \"Log in\"")]), "Log in");
    assert!(env.ok(&["text"]).contains(&env.nonce));
}

#[test]
fn t03_fill_and_click_send_trusted_events() {
    let env = Env::new("input");
    env.ok(&["open", &env.url("/form")]);
    let snap = env.ok(&["snapshot"]);
    let email = ref_of(&snap, "textbox \"Email\"");
    let go = ref_of(&snap, "button \"Log in\"");

    env.ok(&["fill", &email, "user@example.com"]);
    env.ok(&["press", "Tab"]); // blur -> change
    let clicked = env.ok(&["click", &format!("@{}", go)]); // @N form is accepted too
    assert!(clicked.contains("navigated=no"), "{}", clicked);

    assert_eq!(env.eval("document.getElementById('out').textContent"), "hello user@example.com");
    let log = env.eval("JSON.stringify(window.log)");
    for want in ["beforeinput:true", "input:true", "keydown:true", "change:true", "click:true"] {
        assert!(log.contains(want), "missing {} in {}", want, log);
    }
    assert!(!log.contains(":false"), "untrusted (synthetic) event seen: {}", log);

    // Refill replaces instead of appending; empty text clears.
    env.ok(&["fill", &email, "second"]);
    assert_eq!(env.eval("document.getElementById('email').value"), "second");
    env.ok(&["fill", &email, ""]);
    assert_eq!(env.eval("document.getElementById('email').value"), "");

    // Text through stdin survives quoting-hostile content.
    let tricky = "a \"quoted\" 'text' $x `y` @3 --flag";
    let o = env.run_stdin(&["fill", &email, "--stdin"], Some(tricky));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(env.eval("document.getElementById('email').value"), tricky);
}

#[test]
fn t04_controlled_input_rejects_js_value_but_accepts_fill() {
    let env = Env::new("ctl");
    env.ok(&["open", &env.url("/form")]);
    let ctl = ref_of(&env.ok(&["snapshot"]), "textbox \"Controlled\"");
    // Sanity: the fixture really reverts DOM-property writes.
    env.eval("document.getElementById('ctl').value = 'cheat'; 1");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(env.eval("document.getElementById('ctl').value"), "");
    env.ok(&["fill", &ctl, "real typing"]);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(env.eval("document.getElementById('ctl').value"), "real typing");
}

#[test]
fn t05_click_scrolls_offscreen_element_into_view() {
    let env = Env::new("scroll");
    env.ok(&["open", &env.url("/form")]);
    let far = ref_of(&env.ok(&["snapshot"]), "button \"Far away\"");
    assert_eq!(env.eval("window.scrollY"), "0");
    let out = env.ok(&["click", &far]);
    assert!(out.contains("title=\"far-clicked\""), "{}", out);
    assert_ne!(env.eval("window.scrollY"), "0");

    // Hit testing must use viewport coordinates on a scrolled page: an
    // uncovered link is clickable, a covered button is still refused.
    env.ok(&["open", &env.url("/scrolled")]);
    let snap = env.ok(&["snapshot"]);
    env.ok(&["click", &ref_of(&snap, "link \"Deep link\"")]);
    assert_eq!(env.eval("window.linked"), "1");
    assert_ne!(env.eval("window.scrollY"), "0", "fixture should have required a scroll");
    env.eval("const c = document.createElement('div'); c.id = 'veil'; c.style.cssText = 'position:fixed;inset:0'; document.body.append(c); 1");
    let o = env.run(&["click", &ref_of(&snap, "button \"Low button\"")]);
    assert_eq!(o.code, 5, "stdout: {} stderr: {}", o.stdout, o.stderr);
    assert!(o.stderr.contains("obscured by <div#veil>"), "{}", o.stderr);
    assert_eq!(env.eval("window.b2"), "0");
}

#[test]
fn t06_obscured_element_is_not_clicked() {
    let env = Env::new("cover");
    env.ok(&["open", &env.url("/overlay")]);
    let b = ref_of(&env.ok(&["snapshot"]), "button \"Covered\"");
    let o = env.run(&["click", &b]);
    assert_eq!(o.code, 5, "stdout: {} stderr: {}", o.stdout, o.stderr);
    assert!(o.stderr.contains("obscured by <div#cover>"), "{}", o.stderr);
    assert_eq!(env.eval("window.clicked"), "false");
    // Once the cover is gone the same ref works.
    env.eval("document.getElementById('cover').remove(); 1");
    env.ok(&["click", &b]);
    assert_eq!(env.eval("window.clicked"), "true");
}

#[test]
fn t07_same_origin_iframe_is_snapshotted_and_usable() {
    let env = Env::new("frame");
    env.ok(&["open", &env.url("/iframe")]);
    let snap = env.ok(&["snapshot"]);
    let field = ref_of(&snap, "textbox \"Inner field\"");
    let button = ref_of(&snap, "button \"Inner button\"");
    env.ok(&["fill", &field, "in frame"]);
    assert_eq!(env.eval("frames[0].document.querySelector('input').value"), "in frame");
    // Proves click coordinates are right for a node inside an offset iframe.
    env.ok(&["click", &button]);
    assert_eq!(env.eval("document.title"), "inner-clicked");
}

#[test]
fn t08_wait_for_dynamic_content() {
    let env = Env::new("wait");
    env.ok(&["open", &env.url("/late")]);
    let late = format!("late-{}", env.nonce);
    assert!(!env.ok(&["text"]).contains(&late), "content must not be there yet");
    env.ok(&["wait", &late]);
    assert!(env.ok(&["text"]).contains(&late));
    env.ok(&["wait", "early", "--gone"]);
    let o = env.run(&["wait", "never-there", "--timeout", "400"]);
    assert_eq!(o.code, 4, "{}", o.stderr);
}

#[test]
fn t09_refs_are_stable_until_the_node_or_document_goes() {
    let env = Env::new("spa");
    env.ok(&["open", &env.url("/spa")]);
    let snap = env.ok(&["snapshot"]);
    let (stay, target) = (ref_of(&snap, "\"Stay\""), ref_of(&snap, "\"Target\""));
    let (push, rerender) = (ref_of(&snap, "\"Push\""), ref_of(&snap, "\"Rerender\""));

    // A second snapshot (another agent, the human's viewer) does not renumber.
    let again = env.ok(&["snapshot"]);
    assert_eq!(snap, again);

    // pushState is not a document change: refs survive.
    let pushed = env.ok(&["click", &push]);
    assert!(pushed.contains("navigated=no") && pushed.contains("/pushed"), "{}", pushed);
    env.ok(&["click", &stay]);
    assert_eq!(env.eval("window.stay"), "1");

    // Re-render replaces the node without any navigation: old ref is stale.
    env.ok(&["click", &rerender]);
    let o = env.run(&["click", &target]);
    assert_eq!(o.code, 2, "stdout: {} stderr: {}", o.stdout, o.stderr);
    assert!(o.stderr.contains("stale ref") && o.stderr.contains("Target"), "{}", o.stderr);
    let after = env.ok(&["snapshot"]);
    assert_eq!(ref_of(&after, "\"Stay\""), stay, "surviving node keeps its number");
    let new_target = ref_of(&after, "\"Target\"");
    assert_ne!(new_target, target, "replacement node must not reuse the old number");
    env.ok(&["click", &new_target]);

    // A real navigation invalidates everything issued before it.
    env.ok(&["open", &env.url("/form")]);
    let o = env.run(&["click", &stay]);
    assert_eq!(o.code, 2, "{}", o.stderr);
    let o = env.run(&["click", "9999"]);
    assert_eq!(o.code, 1, "never-issued ref is a usage error: {}", o.stderr);
}

#[test]
fn t11_dialog_does_not_hang_and_can_be_answered() {
    let env = Env::new("dialog");
    env.ok(&["open", &env.url("/alert")]);
    let boom = ref_of(&env.ok(&["snapshot"]), "button \"Boom\"");
    let started = Instant::now();
    let out = env.ok(&["click", &boom]);
    assert!(started.elapsed() < Duration::from_secs(10), "click hung on the dialog");
    assert!(out.contains(&format!("dialog=alert \"hi {}\"", env.nonce)), "{}", out);

    let blocked = env.run(&["eval", "1+1"]);
    assert_eq!(blocked.code, 8, "stdout: {} stderr: {}", blocked.stdout, blocked.stderr);
    assert!(blocked.stderr.contains("kb dialog accept"), "{}", blocked.stderr);

    env.ok(&["dialog", "accept"]);
    assert_eq!(env.eval("window.after"), "1", "script after alert() must have resumed");
    assert_eq!(env.run(&["dialog", "accept"]).code, 5, "no dialog left");
}

#[test]
fn t12_tabs_are_addressable_and_popups_are_reported() {
    let env = Env::new("tabs");
    env.ok(&["open", &env.url("/form")]);
    let made = env.ok(&["tab", "new", &env.url("/spa")]);
    assert!(made.contains("tab=2"), "{}", made);
    let tabs = env.ok(&["tabs"]);
    assert_eq!(tabs.lines().count(), 2, "{}", tabs);
    assert!(tabs.lines().any(|l| l.starts_with("* 2 Spa")), "{}", tabs);

    // --tab reaches a background tab without changing the current one.
    assert!(env.ok(&["--tab", "1", "eval", "document.title"]).starts_with("Form "));
    assert_eq!(env.eval("document.title"), "Spa");
    let snap1 = env.ok(&["snapshot", "--tab", "1"]);
    env.ok(&["fill", &ref_of(&snap1, "textbox \"Email\""), "bg", "--tab", "1"]);
    assert_eq!(env.ok(&["--tab", "1", "eval", "document.getElementById('email').value"]), "bg");

    // target=_blank link: the click result names the new tab.
    let popup = env.ok(&["click", &ref_of(&snap1, "link \"Popup\""), "--tab", "1"]);
    assert!(popup.contains("new_tab=3"), "{}", popup);
    assert!(env.ok(&["--tab", "3", "eval", "document.title"]).starts_with("Late "));

    env.ok(&["tab", "close", "3"]);
    env.ok(&["tab", "close"]); // current (2)
    let tabs = env.ok(&["tabs"]);
    assert_eq!(tabs.lines().count(), 1, "{}", tabs);
    assert!(tabs.starts_with("* 1 Form"), "{}", tabs);
    assert_eq!(env.run(&["--tab", "42", "eval", "1"]).code, 6);
}

#[test]
fn t13_recovers_after_chrome_is_killed() {
    let env = Env::new("crash");
    env.ok(&["open", &env.url("/form")]);
    let chrome = env.status_num("chrome_pid");
    kill(chrome);
    std::thread::sleep(Duration::from_millis(1000));
    let mut last = String::new();
    let recovered = (0..3).any(|_| {
        let o = env.run(&["open", &env.url("/spa")]);
        last = o.stderr;
        o.code == 0
    });
    assert!(recovered, "no recovery after Chrome was killed: {}", last);
    assert_ne!(env.status_num("chrome_pid"), chrome);
    assert_eq!(env.eval("document.title"), "Spa");
}

#[test]
fn t14_stop_and_daemon_kill_leave_no_processes() {
    let env = Env::new("procs");
    env.ok(&["open", &env.url("/form")]);
    let daemon = env.status_num("pid");
    assert!(!env.procs().is_empty(), "Chrome should be running under this state dir");

    assert_eq!(env.ok(&["stop"]), "ok stopped");
    env.wait_no_procs("after kb stop");
    assert!(!alive(daemon), "daemon still alive after stop");
    assert_eq!(env.run(&["status"]).code, 3, "status must not start a daemon");
    assert_eq!(env.ok(&["stop"]), "ok not running");

    // Hard-kill the daemon: Chrome must not be orphaned.
    env.ok(&["open", &env.url("/form")]);
    let daemon2 = env.status_num("pid");
    assert_ne!(daemon2, daemon);
    kill(daemon2);
    env.wait_no_procs("after daemon was killed");

    // And the next command just works (stale daemon.json is ignored).
    env.ok(&["open", &env.url("/spa")]);
    assert_eq!(env.eval("document.title"), "Spa");
}

#[test]
fn t15_fill_handles_select_checkbox_and_rejects_bad_input() {
    let env = Env::new("kinds");
    env.ok(&["open", &env.url("/form")]);
    let snap = env.ok(&["snapshot"]);
    let color = ref_of(&snap, "combobox \"Color\"");
    let agree = ref_of(&snap, "checkbox \"Agree\"");

    env.ok(&["fill", &color, "Blue"]); // by label
    assert_eq!(env.eval("document.getElementById('color').value"), "b");
    env.ok(&["fill", &color, "r"]); // by value
    assert_eq!(env.eval("document.getElementById('color').value"), "r");
    let bad = env.run(&["fill", &color, "Green"]);
    assert_eq!(bad.code, 5, "{}", bad.stderr);
    assert!(bad.stderr.contains("\"Red\", \"Blue\""), "options should be listed: {}", bad.stderr);

    env.ok(&["fill", &agree, "true"]);
    assert_eq!(env.eval("document.getElementById('agree').checked"), "true");
    env.ok(&["fill", &agree, "true"]); // already checked: no toggle
    assert_eq!(env.eval("document.getElementById('agree').checked"), "true");
    env.ok(&["fill", &agree, "false"]);
    assert_eq!(env.eval("document.getElementById('agree').checked"), "false");
    assert!(env.ok(&["snapshot"]).lines().any(|l| l.contains("checkbox \"Agree\"") && !l.contains("checked")));

    let go = ref_of(&snap, "button \"Log in\"");
    assert_eq!(env.run(&["fill", &go, "x"]).code, 5, "a button is not fillable");
}

#[test]
fn t16_errors_history_screenshot() {
    let env = Env::new("misc");
    assert_eq!(env.run(&["frobnicate"]).code, 1);
    assert_eq!(env.run(&["click"]).code, 1);
    assert!(env.procs().is_empty(), "usage errors must not start a browser");

    env.ok(&["open", &env.url("/form")]);
    let thrown = env.run(&["eval", "throw new Error('boom')", "--json"]);
    assert_eq!(thrown.code, 7);
    assert!(thrown.stderr.contains("\"code\":\"eval_error\"") && thrown.stderr.contains("boom"), "{}", thrown.stderr);
    assert_eq!(env.eval("({a: [1, 'x']})"), "{\"a\":[1,\"x\"]}");
    assert_eq!(env.eval("new Promise(r => setTimeout(() => r(7), 50))"), "7");

    env.ok(&["open", &env.url("/spa")]);
    let back = env.ok(&["back"]);
    assert!(back.contains("navigated=yes") && back.contains("/form"), "{}", back);
    let fwd = env.ok(&["forward"]);
    assert!(fwd.contains("/spa"), "{}", fwd);
    env.eval("window.marker = 1");
    let reloaded = env.ok(&["reload", "--snapshot"]);
    assert!(reloaded.contains("navigated=yes") && reloaded.contains("button \"Stay\""), "{}", reloaded);
    assert_eq!(env.eval("typeof window.marker"), "undefined");

    let png = env.home.join("shot.png");
    env.ok(&["screenshot", png.to_str().unwrap()]);
    let bytes = std::fs::read(&png).unwrap();
    assert!(bytes.len() > 1000 && bytes.starts_with(&[0x89, b'P', b'N', b'G']), "not a PNG ({} bytes)", bytes.len());

    // A failed navigation is an error, not a silent success. (Last: Chrome
    // leaves its error page in the history.)
    let missing = env.run(&["open", "http://127.0.0.1:1/nothing"]);
    assert_eq!(missing.code, 3, "{}", missing.stderr);
    assert!(missing.stderr.contains("ERR_"), "{}", missing.stderr);
}

#[test]
fn t17_languages_change_human_text_but_not_the_contract() {
    let env = Env::new("lang");
    // English by default, whatever the machine's locale is.
    assert_eq!(env.ok(&["lang"]), "en");
    assert!(env.ok(&["help"]).starts_with("kb - terminal browser"));

    // One run only.
    let ko = env.ok(&["--lang", "ko", "help"]);
    assert!(ko.contains("터미널 브라우저") && ko.contains("open <url>") && ko.contains("--layout 1|2|4"), "{}", ko);
    assert!(env.ok(&["help"]).starts_with("kb - terminal browser"), "--lang must not stick");

    // The error line and exit code stay as they are; an explanation is added.
    let en = env.run(&["click"]);
    let e = env.run(&["--lang", "ko", "click"]);
    assert_eq!((e.code, en.code), (1, 1));
    assert!(e.stderr.starts_with(en.stderr.trim_end()), "contract lines changed:\n{}\nvs\n{}", e.stderr, en.stderr);
    assert!(e.stderr.contains("(명령 사용법이 틀렸습니다"), "{}", e.stderr);
    assert_eq!(en.stderr.trim_end().lines().count(), 2, "English adds nothing: {}", en.stderr);
    let j = env.run(&["--lang", "ko", "--json", "click"]);
    assert!(j.stderr.trim_start().starts_with('{') && !j.stderr.contains("명령"), "JSON errors stay untouched: {}", j.stderr);

    // Remembered choice, every language.
    for (code, word) in [("ja", "端末ブラウザ"), ("zh", "终端浏览器"), ("es", "navegador de terminal"), ("fr", "navigateur de terminal"), ("de", "Terminal-Browser"), ("ko", "터미널 브라우저")] {
        assert_eq!(env.ok(&["lang", code]), format!("ok language {}", code));
        assert_eq!(env.ok(&["lang"]), code);
        assert!(env.ok(&["help"]).contains(word), "{} help lacks {:?}", code, word);
    }
    // The flag wins over the remembered choice.
    assert!(env.ok(&["--lang", "en", "help"]).starts_with("kb - terminal browser"));
    assert_eq!(env.run(&["lang", "klingon"]).code, 1);
    assert_eq!(env.run(&["--lang", "klingon", "help"]).code, 1);
    assert_eq!(env.ok(&["lang"]), "ko", "a rejected language must not be saved");
    // "auto" follows the OS and never fails.
    env.ok(&["lang", "auto"]);
    assert!(["en", "ko", "ja", "zh", "es", "fr", "de"].contains(&env.ok(&["lang"]).as_str()));
    assert!(env.procs().is_empty(), "language commands must not start a browser");
}

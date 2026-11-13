//! Background daemon: owns Chrome, tracks tabs/refs/dialogs from CDP events,
//! and serves CLI requests over loopback TCP.

use crate::cdp::{Cdp, CdpErr, Event};
use crate::ipc;
use crate::json::Value;
use crate::overlay;
use crate::pins::{self, Pin};
use crate::platform;
use crate::snapshot;
use crate::util::{self, log};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub struct KbErr {
    pub code: &'static str,
    pub msg: String,
    pub hint: String,
}

impl KbErr {
    pub fn new(code: &'static str, msg: impl Into<String>) -> KbErr {
        KbErr { code, msg: msg.into(), hint: String::new() }
    }
    pub fn hint(mut self, h: impl Into<String>) -> KbErr {
        self.hint = h.into();
        self
    }
    pub fn usage(msg: impl Into<String>) -> KbErr {
        KbErr::new("usage", msg).hint("run: kb help")
    }
}

pub type Res<T> = Result<T, KbErr>;

pub struct Dialog {
    pub kind: String,
    pub message: String,
}

#[derive(Clone)]
pub struct RefEntry {
    pub frame: String,
    pub backend: i64,
    pub role: String,
    pub name: String,
}

pub struct Tab {
    pub id: u32,
    pub target: String,
    pub session: String,
    pub url: String,
    pub title: String,
    pub refs: HashMap<u32, RefEntry>,
    pub ref_ix: HashMap<(String, i64), u32>,
    /// Next ref number. Never reset, so a number is never reused in a tab.
    pub next_ref: u32,
    pub dialog: Option<Dialog>,
    pub loading: bool,
    /// Bumped on every cross-document navigation of the main frame.
    pub nav_seq: u64,
    /// Loader id of the main-frame document counted last in `nav_seq`.
    pub nav_loader: String,
    /// Loader ids whose `load` lifecycle event has fired (most recent last).
    pub loaded: Vec<String>,
    /// Show `@N` badges on the page (shared by the human and the agent).
    pub badges: bool,
    /// Layout viewport width / pane pixel width while a viewer shows the tab;
    /// overlay labels are enlarged by it so they stay legible when zoomed out.
    pub view_scale: f64,
    pub overlay_gen: u64,
    /// Overlay must be rebuilt (state changed, new snapshot, zoom changed).
    pub overlay_dirty: bool,
    pub overlay_at: Instant,
    /// (url, label count) of the last overlay sync.
    pub overlay_last: (String, usize),
}

impl Tab {
    /// Stable ref for a node: same node, same number, across snapshots.
    pub fn ref_for(&mut self, e: RefEntry) -> u32 {
        let key = (e.frame.clone(), e.backend);
        if let Some(n) = self.ref_ix.get(&key) {
            self.refs.insert(*n, e);
            return *n;
        }
        let n = self.next_ref;
        self.next_ref += 1;
        self.ref_ix.insert(key, n);
        self.refs.insert(n, e);
        n
    }
    fn drop_refs(&mut self, frame: Option<&str>) {
        match frame {
            None => {
                self.refs.clear();
                self.ref_ix.clear();
            }
            Some(f) => {
                self.refs.retain(|_, e| e.frame != f);
                self.ref_ix.retain(|k, _| k.0 != f);
            }
        }
    }
}

/// One screencast frame: base64 PNG exactly as Chrome produced it.
#[derive(Clone)]
pub struct Frame {
    data: Arc<String>,
    /// CSS viewport size the frame shows.
    w: i64,
    h: i64,
    session: String,
    ack: i64,
    acked: Arc<AtomicBool>,
}

struct PaneSub {
    tab: u32,
    /// Latest undelivered frame; a newer frame replaces an unsent older one.
    slot: Option<Frame>,
}

#[derive(Default)]
struct ViewInner {
    panes: HashMap<u32, PaneSub>,
    events: Vec<Value>,
    closed: bool,
}

/// A connected `kb view`: frames and events are pushed to it by a writer thread.
#[derive(Default)]
pub struct ViewConn {
    inner: Mutex<ViewInner>,
    cv: Condvar,
}

impl ViewConn {
    fn push_event(&self, ev: Value) {
        self.inner.lock().unwrap().events.push(ev);
        self.cv.notify_all();
    }
}

#[derive(Default)]
pub struct State {
    pub tabs: Vec<Tab>,
    pub next_tab: u32,
    pub current: u32,
    pub views: Vec<Arc<ViewConn>>,
    pub pins: Vec<Pin>,
    pub next_pin: u32,
}

impl State {
    pub fn tab(&mut self, id: u32) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }
    fn by_session(&mut self, session: &str) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.session == session)
    }
}

pub struct Shared {
    pub st: Mutex<State>,
    pub cv: Condvar,
    pub home: PathBuf,
    pub overlay_lock: Mutex<()>,
}

impl Shared {
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap()
    }
    /// Block until `f` yields a value or the timeout passes.
    pub fn wait<T>(&self, timeout: Duration, mut f: impl FnMut(&mut State) -> Option<T>) -> Option<T> {
        let end = Instant::now() + timeout;
        let mut g = self.lock();
        loop {
            if let Some(v) = f(&mut g) {
                return Some(v);
            }
            let now = Instant::now();
            if now >= end {
                return None;
            }
            g = self.cv.wait_timeout(g, end - now).unwrap().0;
        }
    }
}

pub struct Browser {
    pub cdp: Arc<Cdp>,
    child: Mutex<Child>,
    pub pid: u32,
    pub debug_port: u16,
}

pub struct Daemon {
    pub shared: Arc<Shared>,
    browser: Mutex<Option<Arc<Browser>>>,
    cmd_lock: Mutex<()>,
    home: PathBuf,
    token: String,
}

/// Everything one tab-scoped command needs.
pub struct Ctx {
    pub shared: Arc<Shared>,
    pub cdp: Arc<Cdp>,
    pub tab: u32,
    pub session: String,
    pub target: String,
    pub timeout: Duration,
    pub want_snapshot: bool,
    /// CDP object group of this request. Releasing a group frees all its
    /// objects, so concurrent requests must each have their own.
    pub group: String,
}

/// A fresh object-group name for one request.
pub fn object_group() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("kb-{}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

impl Ctx {
    pub fn with_tab<T>(&self, f: impl FnOnce(&mut Tab) -> T) -> Res<T> {
        match self.shared.lock().tab(self.tab) {
            Some(t) => Ok(f(t)),
            None => Err(KbErr::new("no_such_tab", format!("tab {} was closed", self.tab)).hint("run: kb tabs")),
        }
    }
    pub fn dialog_err(&self) -> KbErr {
        let d = self
            .with_tab(|t| t.dialog.as_ref().map(|d| format!("{} {}", d.kind, crate::json::quote(&d.message))))
            .ok()
            .flatten()
            .unwrap_or_default();
        KbErr::new("dialog", format!("dialog open on tab {}: {}", self.tab, d)).hint("run: kb dialog accept|dismiss")
    }
    fn map_err(&self, method: &str, e: CdpErr) -> KbErr {
        match e {
            CdpErr::Timeout => KbErr::new("timeout", format!("{} timed out", method)),
            CdpErr::Cancelled => self.dialog_err(),
            CdpErr::Dead => KbErr::new("chrome", "browser connection lost").hint("retry: the daemon relaunches Chrome"),
            CdpErr::Remote(m) => KbErr::new("chrome", m),
        }
    }
    /// CDP call on this tab's session. Stops waiting if a dialog opens.
    pub fn call(&self, method: &str, params: Value) -> Res<Value> {
        let cancel = || self.shared.lock().tab(self.tab).map(|t| t.dialog.is_some()).unwrap_or(true);
        self.cdp
            .call_c(&self.session, method, params, self.timeout, &cancel)
            .map_err(|e| self.map_err(method, e))
    }
    /// Browser-level CDP call (never blocked by page dialogs).
    pub fn bcall(&self, method: &str, params: Value) -> Res<Value> {
        self.cdp.call("", method, params, self.timeout).map_err(|e| self.map_err(method, e))
    }
}

fn ack_frame(cdp: &Cdp, f: &Frame) {
    if !f.acked.swap(true, Ordering::SeqCst) {
        cdp.send(&f.session, "Page.screencastFrameAck", obj! { "sessionId" => f.ack });
    }
}

/// Hand a screencast frame to every pane showing that tab. Chrome sends the
/// next frame only after an ack, and the ack is sent once a viewer has the
/// frame on the wire, so a slow terminal throttles the stream by itself.
fn on_frame(shared: &Shared, cdp: &Cdp, mut ev: Event) {
    let data = match ev.params.take("data") {
        Value::Str(s) => Arc::new(s),
        _ => return,
    };
    let frame = Frame {
        data,
        w: ev.params.at(&["metadata", "deviceWidth"]).as_i64().unwrap_or(0),
        h: ev.params.at(&["metadata", "deviceHeight"]).as_i64().unwrap_or(0),
        ack: ev.params.get("sessionId").as_i64().unwrap_or(0),
        session: ev.session,
        acked: Arc::new(AtomicBool::new(false)),
    };
    let mut delivered = false;
    let mut st = shared.lock();
    if let Some(tab) = st.by_session(&frame.session).map(|t| t.id) {
        for view in &st.views {
            let mut inner = view.inner.lock().unwrap();
            for pane in inner.panes.values_mut().filter(|p| p.tab == tab) {
                if let Some(old) = pane.slot.replace(frame.clone()) {
                    ack_frame(cdp, &old);
                }
                delivered = true;
            }
            drop(inner);
            view.cv.notify_all();
        }
    }
    if !delivered {
        ack_frame(cdp, &frame);
    }
}

fn on_event(shared: &Shared, cdp: &Cdp, ev: Event) {
    if ev.method == "Page.screencastFrame" {
        return on_frame(shared, cdp, ev);
    }
    let p = &ev.params;
    if std::env::var_os("KB_DEBUG").is_some() {
        let detail: String = p.to_string().chars().take(300).collect();
        log(&format!("event {} {}", ev.method, detail));
    }
    let mut st = shared.lock();
    match ev.method.as_str() {
        "Target.attachedToTarget" => {
            let info = p.get("targetInfo");
            let session = p.get("sessionId").str().to_string();
            if info.get("type").str() == "page" {
                let target = info.get("targetId").str().to_string();
                if !st.tabs.iter().any(|t| t.target == target) {
                    st.next_tab += 1;
                    let id = st.next_tab;
                    st.tabs.push(Tab {
                        id,
                        target,
                        session: session.clone(),
                        url: info.get("url").str().to_string(),
                        title: info.get("title").str().to_string(),
                        refs: HashMap::new(),
                        ref_ix: HashMap::new(),
                        next_ref: 1,
                        dialog: None,
                        loading: false,
                        nav_seq: 0,
                        nav_loader: String::new(),
                        loaded: Vec::new(),
                        badges: false,
                        view_scale: 1.0,
                        overlay_gen: 0,
                        overlay_dirty: false,
                        overlay_at: Instant::now(),
                        overlay_last: (String::new(), 0),
                    });
                    if st.current == 0 {
                        st.current = id;
                    }
                    cdp.send(&session, "Page.enable", obj! {});
                    cdp.send(&session, "Page.setLifecycleEventsEnabled", obj! { "enabled" => true });
                }
                cdp.send(&session, "Runtime.runIfWaitingForDebugger", obj! {});
            } else {
                // Workers etc.: let them run, and stop listening.
                cdp.send(&session, "Runtime.runIfWaitingForDebugger", obj! {});
                cdp.send("", "Target.detachFromTarget", obj! { "sessionId" => session });
            }
        }
        "Target.detachedFromTarget" => {
            let session = p.get("sessionId").str();
            st.tabs.retain(|t| t.session != session);
            if !st.tabs.iter().any(|t| t.id == st.current) {
                st.current = st.tabs.last().map(|t| t.id).unwrap_or(0);
            }
        }
        "Target.targetInfoChanged" => {
            let info = p.get("targetInfo");
            let target = info.get("targetId").str();
            if let Some(t) = st.tabs.iter_mut().find(|t| t.target == target) {
                t.url = info.get("url").str().to_string();
                t.title = info.get("title").str().to_string();
            }
        }
        "Page.frameNavigated" => {
            let mut left = None;
            if let Some(t) = st.by_session(&ev.session) {
                let frame = p.get("frame");
                if frame.get("parentId").is_null() {
                    t.url = frame.get("url").str().to_string();
                    // Already counted when its "commit" lifecycle event came first.
                    let loader = frame.get("loaderId").str();
                    if loader.is_empty() || loader != t.nav_loader {
                        t.nav_loader = loader.to_string();
                        t.drop_refs(None);
                        t.nav_seq += 1;
                        left = Some(t.id);
                    }
                } else {
                    t.drop_refs(Some(frame.get("id").str()));
                }
            }
            // The document is gone: its pins detach until found again.
            if let Some(tab) = left {
                for pin in st.pins.iter_mut().filter(|pin| pin.tab == tab) {
                    pin.node = None;
                    pin.ref_n = None;
                }
            }
        }
        "Page.frameDetached" => {
            if let Some(t) = st.by_session(&ev.session) {
                t.drop_refs(Some(p.get("frameId").str()));
            }
        }
        "Page.lifecycleEvent" => {
            let mut left = None;
            if let Some(t) = st.by_session(&ev.session) {
                let (main, loader) = (p.get("frameId").str() == t.target, p.get("loaderId").str());
                if main && p.get("name").str() == "load" {
                    t.loaded.push(loader.to_string());
                    if t.loaded.len() > 32 {
                        t.loaded.remove(0);
                    }
                }
                // Chrome sometimes sends no frameNavigated for a navigation
                // issued right after attach; the commit of a new document is
                // the same fact.
                if main && p.get("name").str() == "commit" && !loader.is_empty() && loader != t.nav_loader {
                    t.nav_loader = loader.to_string();
                    t.drop_refs(None);
                    t.nav_seq += 1;
                    left = Some(t.id);
                }
            }
            if let Some(tab) = left {
                for pin in st.pins.iter_mut().filter(|pin| pin.tab == tab) {
                    pin.node = None;
                    pin.ref_n = None;
                }
            }
        }
        "Page.frameStartedLoading" | "Page.frameStoppedLoading" => {
            if let Some(t) = st.by_session(&ev.session) {
                if p.get("frameId").str() == t.target {
                    t.loading = ev.method.ends_with("StartedLoading");
                }
            }
        }
        "Page.javascriptDialogOpening" => {
            if let Some(t) = st.by_session(&ev.session) {
                t.dialog = Some(Dialog {
                    kind: p.get("type").str().to_string(),
                    message: p.get("message").str().to_string(),
                });
            }
        }
        "Page.javascriptDialogClosed" => {
            if let Some(t) = st.by_session(&ev.session) {
                t.dialog = None;
            }
        }
        _ => return,
    }
    drop(st);
    shared.cv.notify_all();
}

impl Daemon {
    fn launch(&self) -> Res<Arc<Browser>> {
        let fail = |m: String| KbErr::new("chrome", m);
        let exe = platform::find_chrome()
            .ok_or_else(|| fail("Chrome not found".into()).hint("set KB_CHROME to a Chrome/Chromium/Edge executable"))?;
        let profile = self.home.join("profile");
        fs::create_dir_all(&profile).map_err(|e| fail(format!("cannot create profile dir: {}", e)))?;
        let port_file = profile.join("DevToolsActivePort");
        let _ = fs::remove_file(&port_file);

        let mut cmd = Command::new(&exe);
        cmd.arg(format!("--user-data-dir={}", profile.display()))
            .arg("--remote-debugging-port=0")
            .args(["--no-first-run", "--no-default-browser-check", "--disable-search-engine-choice-screen"])
            .args(["--password-store=basic", "--use-mock-keychain", "--window-size=1280,800"]);
        if std::env::var_os("KB_HEADED").is_none() {
            cmd.arg("--headless=new");
        }
        cmd.arg("about:blank").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        platform::configure_chrome(&mut cmd);
        let mut child = platform::spawn_chrome(cmd).map_err(|e| fail(format!("cannot start {}: {}", exe.display(), e)))?;
        log(&format!("chrome started pid={} exe={}", child.id(), exe.display()));

        // Chrome writes "<port>\n<browser ws path>" once the debugger listens.
        let end = Instant::now() + Duration::from_secs(30);
        let (port, path) = loop {
            if let Ok(text) = fs::read_to_string(&port_file) {
                let mut lines = text.lines();
                if let (Some(Ok(port)), Some(path)) = (lines.next().map(|l| l.trim().parse::<u16>()), lines.next()) {
                    break (port, path.trim().to_string());
                }
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(fail(format!("Chrome exited during startup ({})", status)));
            }
            if Instant::now() > end {
                let _ = child.kill();
                return Err(fail("Chrome did not open its debugging port in 30s".into()));
            }
            std::thread::sleep(Duration::from_millis(25));
        };

        {
            let mut st = self.shared.lock();
            st.tabs.clear();
            st.current = 0;
        }
        let shared = self.shared.clone();
        let cdp = Cdp::connect(port, &path, Box::new(move |cdp, ev| on_event(&shared, cdp, ev)))
            .map_err(|e| fail(format!("cannot connect to Chrome debugger: {}", e)))?;
        let t = Duration::from_secs(10);
        let call = |m: &str, p: Value| cdp.call("", m, p, t).map_err(|e| fail(format!("{}: {:?}", m, e)));
        call("Target.setDiscoverTargets", obj! { "discover" => true })?;
        call(
            "Target.setAutoAttach",
            obj! { "autoAttach" => true, "waitForDebuggerOnStart" => true, "flatten" => true },
        )?;
        let has_tab = |s: &mut State| if s.tabs.is_empty() { None } else { Some(()) };
        if self.shared.wait(Duration::from_secs(2), has_tab).is_none() {
            // Not auto-attached to the initial page: attach by hand, or make one.
            let targets = call("Target.getTargets", obj! {})?;
            let pages: Vec<&Value> =
                targets.get("targetInfos").arr().iter().filter(|t| t.get("type").str() == "page").collect();
            if pages.is_empty() {
                call("Target.createTarget", obj! { "url" => "about:blank" })?;
            }
            for page in pages {
                call("Target.attachToTarget", obj! { "targetId" => page.get("targetId").str(), "flatten" => true })?;
            }
            if self.shared.wait(Duration::from_secs(5), has_tab).is_none() {
                return Err(fail("no page target available".into()));
            }
        }
        Ok(Arc::new(Browser { cdp, pid: child.id(), child: Mutex::new(child), debug_port: port }))
    }

    /// Current browser, relaunching if Chrome died.
    fn browser(&self) -> Res<Arc<Browser>> {
        let mut slot = self.browser.lock().unwrap();
        if let Some(b) = slot.as_ref() {
            if !b.cdp.is_dead() {
                return Ok(b.clone());
            }
            log("chrome connection is dead; relaunching");
            let mut child = b.child.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
        }
        *slot = None;
        let b = self.launch()?;
        *slot = Some(b.clone());
        Ok(b)
    }

    fn shutdown(&self) -> ! {
        log("shutting down");
        if let Some(b) = self.browser.lock().unwrap().take() {
            let _ = b.cdp.call("", "Browser.close", obj! {}, Duration::from_secs(3));
            let mut child = b.child.lock().unwrap();
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
            b.cdp.close();
        }
        let _ = fs::remove_file(self.home.join("daemon.json"));
        std::process::exit(0);
    }

    fn ctx(&self, b: &Browser, req: &Value) -> Res<Ctx> {
        let st = self.shared.lock();
        let id = match req.get("tab").as_i64() {
            Some(n) => n as u32,
            None => st.current,
        };
        let tab = st
            .tabs
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| KbErr::new("no_such_tab", format!("no tab {}", id)).hint("run: kb tabs"))?;
        Ok(Ctx {
            shared: self.shared.clone(),
            cdp: b.cdp.clone(),
            tab: id,
            session: tab.session.clone(),
            target: tab.target.clone(),
            timeout: Duration::from_millis(req.get("timeout").as_i64().unwrap_or(30_000).max(1) as u64),
            group: object_group(),
            want_snapshot: req.get("snapshot").as_bool().unwrap_or(false),
        })
    }

    fn tabs_value(&self, b: &Browser) -> Value {
        // Title/url change events can lag; ask Chrome for the current values.
        let fresh = b.cdp.call("", "Target.getTargets", obj! {}, Duration::from_secs(5));
        let mut st = self.shared.lock();
        if let Ok(fresh) = fresh {
            for info in fresh.get("targetInfos").arr() {
                if let Some(t) = st.tabs.iter_mut().find(|t| t.target == info.get("targetId").str()) {
                    t.url = info.get("url").str().to_string();
                    t.title = info.get("title").str().to_string();
                }
            }
        }
        let mut text = String::new();
        let mut list = Vec::new();
        for t in &st.tabs {
            let cur = t.id == st.current;
            text.push_str(&format!("{} {} {} - {}\n", if cur { "*" } else { " " }, t.id, t.title, t.url));
            let mut item = obj! { "tab" => t.id, "current" => cur, "title" => &t.title, "url" => &t.url };
            if let Some(d) = &t.dialog {
                item.set("dialog", format!("{}: {}", d.kind, d.message));
            }
            list.push(item);
        }
        obj! { "text" => text.trim_end(), "tabs" => list, "current" => st.current }
    }

    fn tab_cmd(&self, b: &Browser, req: &Value, args: &[String]) -> Res<Value> {
        let t = Duration::from_secs(10);
        let bcall = |m: &str, p: Value| {
            b.cdp.call("", m, p, t).map_err(|e| KbErr::new("chrome", format!("{}: {:?}", m, e)))
        };
        let parse_id = |s: &str| {
            s.parse::<u32>().map_err(|_| KbErr::usage(format!("bad tab id: {}", s)))
        };
        match args.first().map(String::as_str) {
            Some("new") => {
                // Own window per tab: a background tab of a shared window is
                // not composited, so it could not be streamed to the viewer.
                let r = bcall("Target.createTarget", obj! { "url" => "about:blank", "newWindow" => true })?;
                let target = r.get("targetId").str().to_string();
                let id = self
                    .shared
                    .wait(t, |s| s.tabs.iter().find(|x| x.target == target).map(|x| x.id))
                    .ok_or_else(|| KbErr::new("timeout", "new tab did not attach"))?;
                // The viewer opens tabs for its panes without moving the
                // agent's current tab.
                if req.get("keep_current").as_bool() != Some(true) {
                    self.shared.lock().current = id;
                }
                if let Some(url) = args.get(1) {
                    let mut req2 = req.clone();
                    req2.set("tab", id);
                    return self.ctx(b, &req2)?.open(url);
                }
                Ok(obj! { "text" => format!("ok tab new | tab={}", id), "tab" => id })
            }
            Some("close") => {
                let id = match args.get(1) {
                    Some(s) => parse_id(s)?,
                    None => req.get("tab").as_i64().map(|n| n as u32).unwrap_or(self.shared.lock().current),
                };
                let target = self
                    .shared
                    .lock()
                    .tab(id)
                    .map(|x| x.target.clone())
                    .ok_or_else(|| KbErr::new("no_such_tab", format!("no tab {}", id)).hint("run: kb tabs"))?;
                bcall("Target.closeTarget", obj! { "targetId" => target })?;
                self.shared.wait(t, |s| if s.tab(id).is_none() { Some(()) } else { None });
                let cur = self.shared.lock().current;
                Ok(obj! { "text" => format!("ok tab close {} | current={}", id, cur), "closed" => id, "current" => cur })
            }
            Some(s) => {
                let id = parse_id(s)?;
                let target = {
                    let mut st = self.shared.lock();
                    let target = st
                        .tab(id)
                        .map(|x| x.target.clone())
                        .ok_or_else(|| KbErr::new("no_such_tab", format!("no tab {}", id)).hint("run: kb tabs"))?;
                    st.current = id;
                    target
                };
                let _ = bcall("Target.activateTarget", obj! { "targetId" => target });
                Ok(obj! { "text" => format!("ok tab {} is current", id), "tab" => id })
            }
            None => Err(KbErr::usage("usage: kb tab new [url] | kb tab <id> | kb tab close [id]")),
        }
    }

    fn handle(&self, req: &Value) -> Res<Value> {
        let cmd = req.get("cmd").str();
        let args: Vec<String> = req.get("args").arr().iter().map(|a| a.str().to_string()).collect();
        let arg = |i: usize, what: &str| -> Res<&str> {
            args.get(i).map(String::as_str).ok_or_else(|| KbErr::usage(format!("{}: missing {}", cmd, what)))
        };
        match cmd {
            "status" => {
                let b = self.browser.lock().unwrap().clone();
                let (chrome_pid, alive, port) = match &b {
                    Some(b) => (b.pid, !b.cdp.is_dead(), b.debug_port),
                    None => (0, false, 0),
                };
                let (tabs, viewers) = {
                    let st = self.shared.lock();
                    (st.tabs.len(), st.views.len())
                };
                let pid = std::process::id();
                return Ok(obj! {
                    "text" => format!(
                        "daemon pid={} chrome pid={} ({}) tabs={} viewers={} home={}",
                        pid, chrome_pid, if alive { "alive" } else { "dead" }, tabs, viewers, self.home.display()
                    ),
                    "pid" => pid, "chrome_pid" => chrome_pid, "chrome_alive" => alive,
                    "debug_port" => port as u32, "tabs" => tabs, "viewers" => viewers,
                    "home" => self.home.display().to_string(),
                    "version" => env!("CARGO_PKG_VERSION"),
                });
            }
            "stop" => return Ok(obj! { "text" => "ok stopped" }),
            _ => {}
        }
        if cmd == "tabs" {
            // Read-only: must stay responsive while another command waits.
            let b = self.browser()?;
            return Ok(self.tabs_value(&b));
        }
        // One browser-mutating command at a time.
        let _guard = self.cmd_lock.lock().unwrap();
        let b = self.browser()?;
        if cmd == "tab" {
            return self.tab_cmd(&b, req, &args);
        }
        let mark_dirty = |st: &mut State, tab: u32| {
            for t in st.tabs.iter_mut().filter(|t| tab == 0 || t.id == tab) {
                t.overlay_dirty = true;
            }
        };
        if cmd == "pins" {
            if args.first().map(String::as_str) == Some("clear") {
                let mut st = self.shared.lock();
                let n = st.pins.len();
                st.pins.clear();
                pins::save(&self.home, &st);
                mark_dirty(&mut st, 0);
                return Ok(obj! { "text" => format!("ok cleared {} pins", n), "pins" => Vec::<Value>::new() });
            }
            // Refresh attachment against what is open right now.
            let ids: Vec<u32> = self.shared.lock().tabs.iter().filter(|t| t.dialog.is_none()).map(|t| t.id).collect();
            for id in ids {
                if let Some(ctx) = self.ctx_for(&b, id, Duration::from_secs(5)) {
                    let _ = ctx.pins_reattach();
                }
            }
            return Ok(pins::list_value(&self.shared.lock()));
        }
        if cmd == "pin" && matches!(arg(0, "add|rm|note")?, "rm" | "note" | "draft" | "cancel") {
            let id: u32 = arg(1, "pin number")?
                .trim_start_matches('#')
                .parse()
                .map_err(|_| KbErr::usage("pin number must be a number, e.g. kb pin rm 2"))?;
            let note = args.get(2..).map(|a| a.join(" ")).unwrap_or_default();
            let mut st = self.shared.lock();
            let Some(at) = st.pins.iter().position(|p| p.id == id) else {
                return Err(KbErr::new("not_actionable", format!("no pin #{}", id)).hint("run: kb pins"));
            };
            let tab = st.pins[at].tab;
            // Viewer-only: the note as typed so far, and abandoning the edit.
            if args[0] == "draft" || (args[0] == "cancel" && !st.pins[at].draft) {
                st.pins[at].editing = if args[0] == "draft" { Some(note) } else { None };
                drop(st);
                if let Some(ctx) = self.ctx_for(&b, tab, Duration::from_secs(5)) {
                    let _ = overlay::sync(&ctx);
                }
                return Ok(obj! { "text" => "ok", "pin" => id });
            }
            let text = if args[0] != "note" || note.trim().is_empty() {
                st.pins.remove(at);
                format!("ok pin #{} removed", id)
            } else {
                st.pins[at].note = note.trim().to_string();
                st.pins[at].draft = false;
                st.pins[at].editing = None;
                format!("ok pin #{} saved", id)
            };
            pins::save(&self.home, &st);
            mark_dirty(&mut st, tab);
            return Ok(obj! { "text" => text, "pin" => id });
        }
        let ctx = self.ctx(&b, req)?;
        if cmd == "dialog" {
            return ctx.dialog(arg(0, "accept|dismiss")?, args.get(1).map(String::as_str));
        }
        if ctx.with_tab(|t| t.dialog.is_some())? {
            return Err(ctx.dialog_err());
        }
        match cmd {
            "open" => ctx.open(arg(0, "url")?),
            "badges" => {
                let on = match args.first().map(String::as_str) {
                    Some("on") => true,
                    Some("off") => false,
                    None | Some("toggle") => !ctx.with_tab(|t| t.badges)?,
                    Some(other) => return Err(KbErr::usage(format!("badges: expected on|off|toggle, got {}", other))),
                };
                ctx.with_tab(|t| t.badges = on)?;
                let labels = overlay::sync(&ctx)?;
                Ok(obj! {
                    "text" => format!("ok badges {} | tab={} labels={}", if on { "on" } else { "off" }, ctx.tab, labels),
                    "badges" => on, "labels" => labels, "tab" => ctx.tab,
                })
            }
            "pin" => match arg(0, "add|rm|note")? {
                "add" => {
                    let out = ctx.pin_add(parse_ref(arg(1, "ref")?)?, arg(2, "note")?)?;
                    overlay::sync(&ctx)?;
                    Ok(out)
                }
                "at" => {
                    let num = |i: usize| arg(i, "coordinate").and_then(|s| s.parse::<i64>().map_err(|_| KbErr::usage("pin at: bad coordinate")));
                    let out = ctx.pin_at(num(1)?, num(2)?, req.get("placeholder").str())?;
                    let _ = overlay::sync(&ctx); // show the input bubble at once
                    Ok(out)
                }
                other => Err(KbErr::usage(format!("pin: expected add|rm|note, got {}", other))),
            },
            "snapshot" => {
                // Badges must show the numbers the agent just received.
                ctx.with_tab(|t| t.overlay_dirty = t.badges)?;
                snapshot::snapshot(
                    &ctx,
                    &snapshot::Opts {
                        interactive: req.get("interactive").as_bool().unwrap_or(false),
                        scope: match req.get("scope").as_str() {
                            Some(s) => Some(parse_ref(s)?),
                            None => None,
                        },
                        max_nodes: req.get("max_nodes").as_i64().unwrap_or(2000).max(1) as usize,
                    },
                )
            }
            "click" => ctx.click(parse_ref(arg(0, "ref")?)?),
            "fill" => ctx.fill(parse_ref(arg(0, "ref")?)?, arg(1, "text (or --stdin)")?),
            "press" => ctx.press(arg(0, "key")?),
            "eval" => ctx.eval(arg(0, "javascript (or --stdin)")?),
            "text" => ctx.text(match args.first() {
                Some(s) => Some(parse_ref(s)?),
                None => None,
            }),
            "wait" => ctx.wait(
                arg(0, "text or ref")?,
                req.get("gone").as_bool().unwrap_or(false),
                req.get("as_text").as_bool().unwrap_or(false),
                !req.get("timeout").is_null(),
            ),
            "screenshot" => ctx.screenshot(arg(0, "output path")?),
            "back" => ctx.history(-1),
            "forward" => ctx.history(1),
            "reload" => ctx.reload(),
            _ => Err(KbErr::usage(format!("unknown command: {}", cmd))),
        }
    }

    fn peek_browser(&self) -> Option<Arc<Browser>> {
        self.browser.lock().unwrap().clone().filter(|b| !b.cdp.is_dead())
    }

    fn session_of(&self, tab: u32) -> Res<String> {
        self.shared
            .lock()
            .tab(tab)
            .map(|t| t.session.clone())
            .ok_or_else(|| KbErr::new("no_such_tab", format!("no tab {}", tab)))
    }

    /// Stop streaming a tab and give it its natural viewport back, unless
    /// some pane of some viewer still shows it.
    fn view_release(&self, tab: u32) {
        let in_use = self
            .shared
            .lock()
            .views
            .iter()
            .any(|v| v.inner.lock().unwrap().panes.values().any(|p| p.tab == tab));
        if in_use {
            return;
        }
        if let (Some(b), Ok(session)) = (self.peek_browser(), self.session_of(tab)) {
            b.cdp.send(&session, "Page.stopScreencast", obj! {});
            b.cdp.send(&session, "Emulation.clearDeviceMetricsOverride", obj! {});
        }
        if let Some(t) = self.shared.lock().tab(tab) {
            t.view_scale = 1.0;
            t.overlay_dirty = true;
        }
    }

    fn ctx_for(&self, b: &Browser, tab: u32, timeout: Duration) -> Option<Ctx> {
        let mut st = self.shared.lock();
        let t = st.tab(tab)?;
        Some(Ctx {
            shared: self.shared.clone(),
            cdp: b.cdp.clone(),
            tab,
            session: t.session.clone(),
            target: t.target.clone(),
            timeout,
            group: object_group(),
            want_snapshot: false,
        })
    }

    /// Keep overlays current without anyone asking: after navigations, DOM
    /// changes, new snapshots, zoom changes and pin edits.
    fn overlay_loop(self: Arc<Self>) {
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let Some(b) = self.peek_browser() else { continue };
            let tabs: Vec<(u32, bool, bool, bool)> = {
                let st = self.shared.lock();
                st.tabs
                    .iter()
                    .filter(|t| t.dialog.is_none())
                    .map(|t| {
                        let pinned = st.pins.iter().any(|p| !p.draft && (p.url == t.url || (p.tab == t.id && p.node.is_some())));
                        (t.id, t.badges || pinned, t.overlay_dirty, t.overlay_last.1 > 0)
                    })
                    .collect()
            };
            for (id, want, forced, shown) in tabs {
                if !want && !forced && !shown {
                    continue; // never touch pages that have no overlay business
                }
                let Some(ctx) = self.ctx_for(&b, id, Duration::from_secs(5)) else { continue };
                let state = ctx
                    .call("Runtime.evaluate", obj! { "expression" => overlay::JS_STATE, "returnByValue" => true })
                    .map(|v| v.at(&["result", "value"]).str().to_string())
                    .unwrap_or_default();
                let (recent, settled) = ctx
                    .with_tab(|t| (t.overlay_at.elapsed() < Duration::from_millis(900), t.overlay_last == (t.url.clone(), 0)))
                    .unwrap_or((true, true));
                let needed = match state.as_str() {
                    // Nothing to show and nothing shown: leave the page alone.
                    "missing" => want && !settled,
                    "dirty" => !recent,
                    "ok" => !want,
                    _ => false,
                };
                if forced || needed {
                    if let Err(e) = overlay::sync(&ctx) {
                        log(&format!("overlay sync tab {}: [{}] {}", id, e.code, e.msg));
                    }
                }
            }
        }
    }

    /// Bind a pane to a tab: set the layout viewport (this is the zoom; no
    /// CSS zoom involved) and stream frames scaled to the pane's pixel size.
    fn view_attach(&self, conn: &Arc<ViewConn>, req: &Value) -> Res<()> {
        let num = |k: &str| req.get(k).as_i64().unwrap_or(0);
        let (pane, tab) = (num("pane") as u32, num("tab") as u32);
        let (px_w, px_h) = (num("px_w").max(16), num("px_h").max(16));
        let b = self.browser()?;
        let session = self.session_of(tab)?;
        let previous = conn.inner.lock().unwrap().panes.insert(pane, PaneSub { tab, slot: None }).map(|p| p.tab);
        if let Some(old) = previous.filter(|old| *old != tab) {
            self.view_release(old);
        }
        let vw = if num("viewport_w") > 0 { num("viewport_w") } else { px_w };
        let vh = ((vw as f64) * (px_h as f64) / (px_w as f64)).round().max(1.0) as i64;
        if let Some(t) = self.shared.lock().tab(tab) {
            // Labels keep their on-screen size when the page is shrunk into the
            // pane, and grow with a large pane (a 4K terminal) so they stay
            // readable next to the page: 1280 px of viewport is the baseline.
            t.view_scale = (vw as f64 / px_w as f64).max(vw as f64 / 1280.0);
            t.overlay_dirty = true;
        }
        let t = Duration::from_secs(5);
        let call = |m: &str, p: Value| {
            b.cdp.call(&session, m, p, t).map_err(|e| KbErr::new("chrome", format!("{}: {:?}", m, e)))
        };
        call(
            "Emulation.setDeviceMetricsOverride",
            obj! { "width" => vw, "height" => vh, "deviceScaleFactor" => 1, "mobile" => false },
        )?;
        // A tab hidden behind a sibling in the same window (a popup and its
        // opener) is not composited and would never produce a frame.
        let hidden = b
            .cdp
            .call(&session, "Runtime.evaluate", obj! { "expression" => "document.visibilityState", "returnByValue" => true }, Duration::from_secs(2))
            .map(|v| v.at(&["result", "value"]).str() == "hidden")
            .unwrap_or(false);
        if hidden {
            if let Some(target) = self.shared.lock().tab(tab).map(|t| t.target.clone()) {
                let _ = b.cdp.call("", "Target.activateTarget", obj! { "targetId" => target }, t);
            }
        }
        let _ = call("Page.stopScreencast", obj! {});
        call(
            "Page.startScreencast",
            obj! { "format" => "png", "maxWidth" => px_w, "maxHeight" => px_h, "everyNthFrame" => 1 },
        )?;
        Ok(())
    }

    /// Forward a human input event straight to the page. Deliberately not
    /// under `cmd_lock`: an agent's long-running command must not freeze input.
    fn view_input(&self, req: &Value) -> Res<()> {
        let method = req.get("method").str();
        if !matches!(method, "Input.dispatchMouseEvent" | "Input.dispatchKeyEvent" | "Input.insertText") {
            return Err(KbErr::usage(format!("view.input: method not allowed: {}", method)));
        }
        let session = self.session_of(req.get("tab").as_i64().unwrap_or(0) as u32)?;
        let b = self.peek_browser().ok_or_else(|| KbErr::new("chrome", "browser is not running"))?;
        b.cdp.send(&session, method, req.get("params").clone());
        Ok(())
    }

    fn view_cmd(&self, conn: &Arc<ViewConn>, req: &Value) {
        let result = match req.get("cmd").str() {
            "view.attach" => self.view_attach(conn, req),
            "view.input" => self.view_input(req),
            "view.detach" => {
                let pane = req.get("pane").as_i64().unwrap_or(0) as u32;
                let old = conn.inner.lock().unwrap().panes.remove(&pane);
                if let Some(old) = old {
                    self.view_release(old.tab);
                }
                Ok(())
            }
            other => Err(KbErr::usage(format!("not a view command: {}", other))),
        };
        if let Err(e) = result {
            conn.push_event(obj! { "type" => "error", "code" => e.code, "message" => e.msg });
        }
    }

    /// Writer side of a viewer connection: pushes events and the newest frame per pane.
    fn view_writer(self: Arc<Self>, conn: Arc<ViewConn>, mut s: TcpStream) {
        loop {
            let (events, frames) = {
                let mut g = conn.inner.lock().unwrap();
                loop {
                    if g.closed {
                        return;
                    }
                    if !g.events.is_empty() || g.panes.values().any(|p| p.slot.is_some()) {
                        break;
                    }
                    g = conn.cv.wait(g).unwrap();
                }
                let events = std::mem::take(&mut g.events);
                let frames: Vec<(u32, u32, Frame)> =
                    g.panes.iter_mut().filter_map(|(id, p)| p.slot.take().map(|f| (*id, p.tab, f))).collect();
                (events, frames)
            };
            let mut ok = true;
            for e in events {
                ok &= ipc::write_json(&mut s, ipc::T_EVENT, &e).is_ok();
            }
            for (pane, tab, f) in frames {
                // [u32 meta length][meta JSON][base64 PNG]
                let meta = obj! { "pane" => pane, "tab" => tab, "w" => f.w, "h" => f.h }.to_string();
                let mut body = Vec::with_capacity(4 + meta.len() + f.data.len());
                body.extend_from_slice(&(meta.len() as u32).to_le_bytes());
                body.extend_from_slice(meta.as_bytes());
                body.extend_from_slice(f.data.as_bytes());
                ok &= ipc::write_frame(&mut s, ipc::T_BINARY, &body).is_ok();
                if let Some(b) = self.peek_browser() {
                    ack_frame(&b.cdp, &f);
                }
            }
            if !ok {
                conn.inner.lock().unwrap().closed = true;
                let _ = s.shutdown(std::net::Shutdown::Both);
                return;
            }
        }
    }

    fn view_close(&self, conn: &Arc<ViewConn>) {
        let tabs: Vec<u32> = {
            let mut g = conn.inner.lock().unwrap();
            g.closed = true;
            g.panes.drain().map(|(_, p)| p.tab).collect()
        };
        conn.cv.notify_all();
        self.shared.lock().views.retain(|v| !Arc::ptr_eq(v, conn));
        for tab in tabs {
            self.view_release(tab);
        }
        log("viewer disconnected");
    }

    fn serve(self: &Arc<Self>, mut s: TcpStream) {
        s.set_nodelay(true).ok();
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        s.set_write_timeout(Some(Duration::from_secs(30))).ok();
        // Nothing is executed before the token matches.
        let hello = match ipc::read_json(&mut s, ipc::T_REQUEST) {
            Ok(h) => h,
            Err(_) => return,
        };
        if hello.get("cmd").str() != "hello" || !util::ct_eq(hello.get("token").str(), &self.token) {
            let _ = ipc::write_json(&mut s, ipc::T_RESPONSE, &obj! { "ok" => false, "error" => "bad token" });
            return;
        }
        let ack = obj! { "ok" => true, "protocol" => ipc::PROTOCOL_VERSION, "pid" => std::process::id() };
        if ipc::write_json(&mut s, ipc::T_RESPONSE, &ack).is_err() {
            return;
        }
        s.set_read_timeout(None).ok();
        let mut view: Option<Arc<ViewConn>> = None;
        while let Ok(req) = ipc::read_json(&mut s, ipc::T_REQUEST) {
            let cmd = req.get("cmd").str().to_string();
            if cmd.starts_with("view.") {
                // First view command turns this into a streaming connection:
                // from here on only the writer thread writes to the socket.
                if view.is_none() {
                    let Ok(out) = s.try_clone() else { break };
                    let conn = Arc::new(ViewConn::default());
                    self.shared.lock().views.push(conn.clone());
                    let (d, c) = (self.clone(), conn.clone());
                    let _ = std::thread::Builder::new().name("view-writer".into()).spawn(move || d.view_writer(c, out));
                    log("viewer connected");
                    view = Some(conn);
                }
                self.view_cmd(view.as_ref().unwrap(), &req);
                continue;
            }
            if view.is_some() {
                continue;
            }
            let started = Instant::now();
            let mut resp = match self.handle(&req) {
                Ok(result) => obj! { "ok" => true, "result" => result },
                Err(e) => {
                    log(&format!("{} failed [{}]: {}", cmd, e.code, e.msg));
                    obj! { "ok" => false, "error" => obj! { "code" => e.code, "message" => e.msg, "hint" => e.hint } }
                }
            };
            resp.set("id", req.get("id").clone());
            // Arguments are not logged: fill text may be a password.
            log(&format!("{} -> {}ms", cmd, started.elapsed().as_millis()));
            let sent = ipc::write_json(&mut s, ipc::T_RESPONSE, &resp);
            if cmd == "stop" {
                let _ = s.flush();
                self.shutdown();
            }
            if sent.is_err() {
                break;
            }
        }
        if let Some(conn) = view {
            self.view_close(&conn);
        }
    }
}

/// Accepts `3`, `@3` and `e3` (PowerShell eats a bare `@3` argument).
pub fn parse_ref(s: &str) -> Res<u32> {
    let digits = s.strip_prefix('@').or_else(|| s.strip_prefix('e')).unwrap_or(s);
    digits
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| KbErr::usage(format!("bad ref: {:?} (use the number from kb snapshot, e.g. 3)", s)))
}

pub fn run() -> i32 {
    let home = platform::home_dir();
    if fs::create_dir_all(&home).is_err() {
        return 3;
    }
    // Exactly one daemon per home: the loser of a startup race exits quietly.
    let lock = match fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("daemon.lock")) {
        Ok(f) => f,
        Err(_) => return 3,
    };
    if lock.try_lock().is_err() {
        return 0;
    }
    util::log_init(&home.join("daemon.log"));
    log(&format!("daemon starting pid={} home={}", std::process::id(), home.display()));
    let _ = fs::remove_file(home.join("daemon.json"));
    let err_file = home.join("daemon.err");
    let _ = fs::remove_file(&err_file);
    platform::kill_children_on_exit();

    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            let _ = fs::write(&err_file, format!("cannot bind loopback socket: {}", e));
            return 3;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let (saved_pins, next_pin) = pins::load(&home);
    let state = State { pins: saved_pins, next_pin, ..State::default() };
    let daemon = Arc::new(Daemon {
        shared: Arc::new(Shared { st: Mutex::new(state), cv: Condvar::new(), home: home.clone(), overlay_lock: Mutex::new(()) }),
        browser: Mutex::new(None),
        cmd_lock: Mutex::new(()),
        home: home.clone(),
        token: util::hex(&util::random_bytes(16)),
    });
    if let Err(e) = daemon.browser() {
        log(&format!("startup failed: {}", e.msg));
        let _ = fs::write(&err_file, format!("{}\n{}", e.msg, e.hint));
        return 3;
    }
    // Publish the endpoint only once we can serve; write-then-rename so a
    // reader never sees a partial file.
    let info = obj! { "port" => port as u32, "token" => &daemon.token, "pid" => std::process::id() };
    let tmp = home.join("daemon.json.tmp");
    if fs::write(&tmp, info.to_string()).and_then(|_| fs::rename(&tmp, home.join("daemon.json"))).is_err() {
        let _ = fs::write(&err_file, "cannot write daemon.json");
        return 3;
    }
    log(&format!("listening on 127.0.0.1:{}", port));
    {
        let d = daemon.clone();
        let _ = std::thread::Builder::new().name("overlay".into()).spawn(move || d.overlay_loop());
    }
    for stream in listener.incoming().flatten() {
        let d = daemon.clone();
        let _ = std::thread::Builder::new().name("ipc".into()).spawn(move || d.serve(stream));
    }
    drop(lock);
    0
}

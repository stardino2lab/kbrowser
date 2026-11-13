//! Tab-scoped commands: navigation, real input events, eval, wait, screenshot.

use crate::daemon::{Ctx, KbErr, RefEntry, Res};
use crate::json::{quote, Value};
use crate::snapshot::{self, lookup, stale};
use std::time::{Duration, Instant};

const GRACE: Duration = Duration::from_millis(150);

/// Hit test inside the element's own frame: is the topmost element at its
/// visible centre the element itself, or something inside it? Done in page
/// JS because `elementFromPoint` has unambiguous viewport coordinates
/// (DOM.getNodeForLocation misjudges scrolled pages).
const JS_HIT: &str = r#"function(){
  if (!this.getClientRects || !this.ownerDocument || !this.ownerDocument.defaultView) return {ok: true};
  const w = this.ownerDocument.defaultView;
  for (const r of this.getClientRects()) {
    const x0 = Math.max(r.left, 0), x1 = Math.min(r.right, w.innerWidth);
    const y0 = Math.max(r.top, 0), y1 = Math.min(r.bottom, w.innerHeight);
    if (x1 - x0 < 1 || y1 - y0 < 1) continue;
    const x = (x0 + x1) / 2, y = (y0 + y1) / 2;
    let h = this.ownerDocument.elementFromPoint(x, y);
    while (h && h.shadowRoot) {
      const d = h.shadowRoot.elementFromPoint(x, y);
      if (!d || d === h) break;
      h = d;
    }
    if (!h) return {ok: true};
    for (let n = h; n; n = n.parentNode || n.host) { if (n === this) return {ok: true}; }
    const l = h.closest('label');
    if (l && l.control === this) return {ok: true};
    let d = h.nodeName.toLowerCase();
    if (h.id) d += '#' + h.id;
    else if (typeof h.className === 'string' && h.className.trim()) d += '.' + h.className.trim().split(/\s+/).join('.');
    return {ok: false, desc: d};
  }
  return {ok: true};
}"#;

const JS_KIND: &str = r#"function(){
  const t = this.tagName ? this.tagName.toLowerCase() : '', ty = (this.type || '').toLowerCase();
  if (this.disabled) return 'disabled';
  if (this.readOnly) return 'readonly';
  if (t === 'select') return 'select';
  if (t === 'input') {
    if (ty === 'checkbox' || ty === 'radio') return 'check';
    if (ty === 'file') return 'file';
    if (['date','time','color','range','month','week','datetime-local'].includes(ty)) return 'setter';
    if (['button','submit','reset','image','hidden'].includes(ty)) return 'other';
    return 'text';
  }
  if (t === 'textarea' || this.isContentEditable) return 'text';
  return 'other';
}"#;

const JS_SELECT_ALL: &str = r#"function(){
  if (typeof this.select === 'function') { this.select(); return; }
  const r = document.createRange(); r.selectNodeContents(this);
  const s = getSelection(); s.removeAllRanges(); s.addRange(r);
}"#;

const JS_SET_VALUE: &str = r#"function(v){
  const d = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(this), 'value');
  d.set.call(this, v);
  this.dispatchEvent(new Event('input', {bubbles: true}));
  this.dispatchEvent(new Event('change', {bubbles: true}));
  return this.value;
}"#;

const JS_SELECT_OPTION: &str = r#"function(v){
  const o = [...this.options];
  const m = o.find(x => x.value === v) || o.find(x => x.label.trim() === v || x.text.trim() === v);
  if (!m) return {ok: false, options: o.map(x => x.label.trim() || x.value)};
  this.value = m.value;
  this.dispatchEvent(new Event('input', {bubbles: true}));
  this.dispatchEvent(new Event('change', {bubbles: true}));
  return {ok: true};
}"#;

const JS_TEXT: &str = r#"function(){
  if (typeof this.value === 'string' && this.tagName !== 'BUTTON') return this.value;
  return this.innerText !== undefined ? this.innerText : (this.textContent || '');
}"#;

/// Snapshot of tab state taken before an action, to report what it caused.
pub struct Mark {
    nav_seq: u64,
    tabs: Vec<u32>,
}

fn not_actionable(r: u32, ent: &RefEntry, why: &str) -> KbErr {
    KbErr::new("not_actionable", format!("@{} ({} {}) {}", r, ent.role, quote(&ent.name), why))
}

fn key_def(name: &str) -> Option<(String, String, i64, String)> {
    // (key, code, windows virtual key, text)
    let named = |key: &str, code: &str, vk: i64, text: &str| Some((key.to_string(), code.to_string(), vk, text.to_string()));
    match name {
        "Enter" | "Return" => named("Enter", "Enter", 13, "\r"),
        "Tab" => named("Tab", "Tab", 9, ""),
        "Escape" | "Esc" => named("Escape", "Escape", 27, ""),
        "Backspace" => named("Backspace", "Backspace", 8, ""),
        "Delete" => named("Delete", "Delete", 46, ""),
        "Insert" => named("Insert", "Insert", 45, ""),
        "Space" | " " => named(" ", "Space", 32, " "),
        "ArrowLeft" | "Left" => named("ArrowLeft", "ArrowLeft", 37, ""),
        "ArrowUp" | "Up" => named("ArrowUp", "ArrowUp", 38, ""),
        "ArrowRight" | "Right" => named("ArrowRight", "ArrowRight", 39, ""),
        "ArrowDown" | "Down" => named("ArrowDown", "ArrowDown", 40, ""),
        "Home" => named("Home", "Home", 36, ""),
        "End" => named("End", "End", 35, ""),
        "PageUp" => named("PageUp", "PageUp", 33, ""),
        "PageDown" => named("PageDown", "PageDown", 34, ""),
        _ => {
            if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<i64>().ok()).filter(|n| (1..=12).contains(n)) {
                return named(name, name, 111 + n, "");
            }
            let mut chars = name.chars();
            let (c, rest) = (chars.next()?, chars.next());
            if rest.is_some() {
                return None;
            }
            let up = c.to_ascii_uppercase();
            let (code, vk) = if up.is_ascii_alphabetic() {
                (format!("Key{}", up), up as i64)
            } else if c.is_ascii_digit() {
                (format!("Digit{}", c), c as i64)
            } else {
                (String::new(), 0)
            };
            Some((c.to_string(), code, vk, c.to_string()))
        }
    }
}

/// `Input.dispatchKeyEvent` params (down, up) for a key name plus CDP
/// modifier bits (alt 1, ctrl 2, meta 4, shift 8).
pub fn key_params(name: &str, mods: i64) -> Option<(Value, Value)> {
    let (key, code, vk, text) = key_def(name)?;
    let text = if mods & 7 != 0 { String::new() } else { text };
    let base = obj! {
        "key" => &key, "code" => &code, "modifiers" => mods,
        "windowsVirtualKeyCode" => vk, "nativeVirtualKeyCode" => vk,
    };
    let mut down = base.clone();
    down.set("type", if text.is_empty() { "rawKeyDown" } else { "keyDown" });
    if !text.is_empty() {
        down.set("text", &text);
    }
    let mut up = base;
    up.set("type", "keyUp");
    Some((down, up))
}

impl Ctx {
    /// Current (url, title). Read from the page itself: the browser-side
    /// target info lags behind right after a load. Falls back to target info
    /// when the page cannot run script (dialog open, crashed).
    pub fn page_info(&self) -> Res<(String, String)> {
        let blocked = self.with_tab(|t| t.dialog.is_some())?;
        if !blocked {
            if let Ok(v) = self.evaluate("[location.href, document.title]") {
                if let [url, title] = v.get("value").arr() {
                    return Ok((url.str().to_string(), title.str().to_string()));
                }
            }
        }
        let info = self.bcall("Target.getTargetInfo", obj! { "targetId" => &self.target })?;
        Ok((info.at(&["targetInfo", "url"]).str().to_string(), info.at(&["targetInfo", "title"]).str().to_string()))
    }

    fn mark(&self) -> Res<Mark> {
        let mut st = self.shared.lock();
        let tabs = st.tabs.iter().map(|t| t.id).collect();
        let nav_seq = st.tab(self.tab).map(|t| t.nav_seq).unwrap_or(0);
        Ok(Mark { nav_seq, tabs })
    }

    /// Give an action a moment to start a navigation, then wait for it to finish.
    fn settle(&self, mark: &Mark, grace: Duration) -> bool {
        let seq0 = mark.nav_seq;
        let started = self.shared.wait(grace, |st| match st.tab(self.tab) {
            Some(t) if t.nav_seq == seq0 && !t.loading && t.dialog.is_none() => None,
            _ => Some(()),
        });
        if started.is_none() {
            return false;
        }
        self.shared.wait(self.timeout, |st| match st.tab(self.tab) {
            Some(t) if t.loading && t.dialog.is_none() => None,
            _ => Some(()),
        });
        // A back/forward-cache restore reports "stopped loading" just before
        // "navigated"; give the commit event a moment to arrive.
        self.shared
            .wait(Duration::from_millis(250), |st| match st.tab(self.tab) {
                Some(t) if t.nav_seq == seq0 && t.dialog.is_none() => None,
                Some(_) => Some(true),
                None => Some(false),
            })
            .unwrap_or(false)
    }

    /// One-line action report: what happened, where the tab is now.
    fn finish(&self, desc: String, mark: &Mark, grace: Duration) -> Res<Value> {
        let navigated = self.settle(mark, grace);
        let (new_tabs, alive, dialog) = {
            let mut st = self.shared.lock();
            let new_tabs: Vec<u32> = st.tabs.iter().map(|t| t.id).filter(|id| !mark.tabs.contains(id)).collect();
            let dialog = st.tab(self.tab).and_then(|t| t.dialog.as_ref().map(|d| (d.kind.clone(), d.message.clone())));
            (new_tabs, st.tab(self.tab).is_some(), dialog)
        };
        if !alive {
            return Ok(obj! { "text" => format!("ok {} | tab={} closed", desc, self.tab), "tab" => self.tab, "closed" => true });
        }
        let (url, title) = self.page_info()?;
        let mut text = format!(
            "ok {} | tab={} navigated={} url={} title={}",
            desc, self.tab, if navigated { "yes" } else { "no" }, url, quote(&title)
        );
        let mut out = obj! { "tab" => self.tab, "navigated" => navigated, "url" => &url, "title" => &title };
        if let Some((kind, message)) = &dialog {
            text.push_str(&format!(" dialog={} {}", kind, quote(message)));
            out.set("dialog", obj! { "type" => kind, "message" => message });
        }
        if let Some(id) = new_tabs.first() {
            text.push_str(&format!(" new_tab={}", id));
            out.set("new_tab", *id);
        }
        if navigated {
            text.push_str(" (refs invalidated)");
        }
        if self.want_snapshot && dialog.is_none() {
            let snap = snapshot::snapshot(self, &snapshot::Opts { interactive: false, scope: None, max_nodes: 2000 })?;
            text.push('\n');
            text.push_str(snap.get("text").str());
            out.set("snapshot", snap.get("nodes").clone());
        }
        out.set("text", text);
        Ok(out)
    }

    /// Ref -> (entry, JS object id), verifying the node is still in the document.
    fn resolve(&self, r: u32) -> Res<(RefEntry, String)> {
        let ent = lookup(self, r)?;
        let obj = match self.call("DOM.resolveNode", obj! { "backendNodeId" => ent.backend, "objectGroup" => &self.group }) {
            Ok(v) => v.at(&["object", "objectId"]).str().to_string(),
            Err(e) if e.code == "chrome" => return Err(stale(r, &ent)),
            Err(e) => return Err(e),
        };
        let connected = self.func(&obj, "function(){ return this.isConnected; }", vec![])?;
        if connected.as_bool() != Some(true) {
            return Err(stale(r, &ent));
        }
        Ok((ent, obj))
    }

    /// Call a JS function with `this` = the object; returns its value.
    fn func(&self, object_id: &str, decl: &str, args: Vec<Value>) -> Res<Value> {
        let r = self.call(
            "Runtime.callFunctionOn",
            obj! {
                "objectId" => object_id, "functionDeclaration" => decl, "arguments" => args,
                "returnByValue" => true, "awaitPromise" => true, "userGesture" => true,
            },
        )?;
        let ex = r.get("exceptionDetails");
        if !ex.is_null() {
            let d = ex.at(&["exception", "description"]).as_str().unwrap_or(ex.get("text").str());
            return Err(KbErr::new("eval_error", d));
        }
        Ok(r.at(&["result", "value"]).clone())
    }

    fn release(&self) {
        self.cdp.send(&self.session, "Runtime.releaseObjectGroup", obj! { "objectGroup" => &self.group });
    }

    /// Scroll the node into view and return a point that actually hits it.
    fn click_point(&self, r: u32, ent: &RefEntry, object_id: &str) -> Res<(f64, f64)> {
        let node = obj! { "backendNodeId" => ent.backend };
        self.call("DOM.scrollIntoViewIfNeeded", node.clone())
            .map_err(|e| if e.code == "chrome" { not_actionable(r, ent, "is not rendered") } else { e })?;
        let quads = self
            .call("DOM.getContentQuads", node)
            .map_err(|e| if e.code == "chrome" { not_actionable(r, ent, "has no visible box") } else { e })?;
        let vp = self.call("Page.getLayoutMetrics", obj! {})?;
        let vw = vp.at(&["cssVisualViewport", "clientWidth"]).as_f64().unwrap_or(1280.0);
        let vh = vp.at(&["cssVisualViewport", "clientHeight"]).as_f64().unwrap_or(800.0);
        let mut point = None;
        for q in quads.get("quads").arr() {
            let c: Vec<f64> = q.arr().iter().filter_map(|v| v.as_f64()).collect();
            if c.len() != 8 {
                continue;
            }
            // Bounding box of the quad, clipped to the viewport.
            let xs = [c[0], c[2], c[4], c[6]];
            let ys = [c[1], c[3], c[5], c[7]];
            let x0 = xs.iter().cloned().fold(f64::MAX, f64::min).max(0.0);
            let x1 = xs.iter().cloned().fold(f64::MIN, f64::max).min(vw);
            let y0 = ys.iter().cloned().fold(f64::MAX, f64::min).max(0.0);
            let y1 = ys.iter().cloned().fold(f64::MIN, f64::max).min(vh);
            if x1 - x0 >= 1.0 && y1 - y0 >= 1.0 {
                point = Some(((x0 + x1) / 2.0, (y0 + y1) / 2.0));
                break;
            }
        }
        let (x, y) = point.ok_or_else(|| not_actionable(r, ent, "is not visible in the viewport"))?;

        // Hit test: refuse to click through something covering the target.
        let v = self.func(object_id, JS_HIT, vec![])?;
        if v.get("ok").as_bool() == Some(false) {
            return Err(not_actionable(r, ent, &format!("is obscured by <{}>", v.get("desc").str()))
                .hint("close or scroll away the covering element, then retry"));
        }
        Ok((x, y))
    }

    /// Trusted mouse click. A dialog opened by the click is not an error.
    fn mouse_click(&self, x: f64, y: f64) -> Res<()> {
        let ev = |kind: &str, button: &str, buttons: i64| {
            obj! { "type" => kind, "x" => x, "y" => y, "button" => button, "buttons" => buttons, "clickCount" => 1 }
        };
        for p in [ev("mouseMoved", "none", 0), ev("mousePressed", "left", 1), ev("mouseReleased", "left", 0)] {
            match self.call("Input.dispatchMouseEvent", p) {
                Err(e) if e.code == "dialog" => return Ok(()),
                other => other?,
            };
        }
        Ok(())
    }

    fn key(&self, combo: &str) -> Res<()> {
        let mut parts: Vec<&str> = combo.split('+').collect();
        // "Control++" or a lone "+" means the plus key itself.
        let name = if combo.ends_with('+') {
            parts.truncate(parts.len().saturating_sub(2));
            "+"
        } else {
            parts.pop().unwrap_or("")
        };
        let mut mods = 0i64;
        for m in &parts {
            mods |= match m.to_ascii_lowercase().as_str() {
                "alt" => 1,
                "control" | "ctrl" => 2,
                "meta" | "cmd" | "win" => 4,
                "shift" => 8,
                _ => return Err(KbErr::usage(format!("unknown modifier: {}", m))),
            };
        }
        let (down, up) = key_params(name, mods)
            .ok_or_else(|| KbErr::usage(format!("unknown key: {}", name)).hint("examples: Enter, Tab, Escape, ArrowDown, a, Control+a"))?;
        for p in [down, up] {
            match self.call("Input.dispatchKeyEvent", p) {
                Err(e) if e.code == "dialog" => return Ok(()),
                other => other?,
            };
        }
        Ok(())
    }

    pub fn open(&self, url: &str) -> Res<Value> {
        let has_scheme = url.contains("://") || ["about:", "data:", "file:", "chrome:"].iter().any(|p| url.starts_with(p));
        let url = if has_scheme { url.to_string() } else { format!("https://{}", url) };
        let mark = self.mark()?;
        let r = self.call("Page.navigate", obj! { "url" => &url })?;
        let err = r.get("errorText").str();
        if !err.is_empty() {
            return Err(KbErr::new("chrome", format!("navigation to {} failed: {}", url, err)));
        }
        // No loaderId means a same-document navigation: nothing to wait for.
        if let Some(loader) = r.get("loaderId").as_str() {
            let done = self.shared.wait(self.timeout, |st| match st.tab(self.tab) {
                Some(t) if !t.loaded.iter().any(|l| l == loader) && t.dialog.is_none() => None,
                _ => Some(()),
            });
            if done.is_none() {
                return Err(KbErr::new("timeout", format!("{} did not finish loading in {}ms", url, self.timeout.as_millis()))
                    .hint("the page may still be usable: try kb snapshot, or raise --timeout"));
            }
        }
        self.finish("open".into(), &mark, Duration::ZERO)
    }

    pub fn click(&self, r: u32) -> Res<Value> {
        let (ent, obj) = self.resolve(r)?;
        let point = self.click_point(r, &ent, &obj);
        self.release();
        let (x, y) = point?;
        let mark = self.mark()?;
        self.mouse_click(x, y)?;
        self.finish(format!("click @{} {} {}", r, ent.role, quote(&ent.name)), &mark, GRACE)
    }

    pub fn fill(&self, r: u32, text: &str) -> Res<Value> {
        let (ent, obj) = self.resolve(r)?;
        let mark = self.mark()?;
        let done = self.fill_inner(r, &ent, &obj, text);
        self.release();
        done?;
        self.finish(format!("fill @{} {} {}", r, ent.role, quote(&ent.name)), &mark, Duration::ZERO)
    }

    fn fill_inner(&self, r: u32, ent: &RefEntry, obj: &str, text: &str) -> Res<()> {
        let kind = self.func(obj, JS_KIND, vec![])?;
        match kind.str() {
            "disabled" => Err(not_actionable(r, ent, "is disabled")),
            "readonly" => Err(not_actionable(r, ent, "is read-only")),
            "text" => {
                self.call("DOM.scrollIntoViewIfNeeded", obj! { "backendNodeId" => ent.backend })
                    .map_err(|e| if e.code == "chrome" { not_actionable(r, ent, "is not rendered") } else { e })?;
                self.call("DOM.focus", obj! { "backendNodeId" => ent.backend })
                    .map_err(|e| if e.code == "chrome" { not_actionable(r, ent, "cannot be focused") } else { e })?;
                self.func(obj, JS_SELECT_ALL, vec![])?;
                if text.is_empty() {
                    self.key("Backspace")
                } else {
                    // Replaces the selection and fires trusted beforeinput/input events.
                    self.call("Input.insertText", obj! { "text" => text }).map(|_| ())
                }
            }
            "setter" => self.func(obj, JS_SET_VALUE, vec![obj! { "value" => text }]).map(|_| ()),
            "select" => {
                let v = self.func(obj, JS_SELECT_OPTION, vec![obj! { "value" => text }])?;
                if v.get("ok").as_bool() == Some(true) {
                    return Ok(());
                }
                let options: Vec<String> = v.get("options").arr().iter().map(|o| quote(o.str())).collect();
                Err(not_actionable(r, ent, &format!("has no option {}", quote(text)))
                    .hint(format!("options: {}", options.join(", "))))
            }
            "check" => {
                let want = match text.to_ascii_lowercase().as_str() {
                    "true" | "on" | "1" | "yes" | "checked" => true,
                    "false" | "off" | "0" | "no" | "unchecked" => false,
                    _ => return Err(KbErr::usage("fill on a checkbox/radio takes true or false")),
                };
                let now = self.func(obj, "function(){ return this.checked; }", vec![])?;
                if now.as_bool() != Some(want) {
                    let (x, y) = self.click_point(r, ent, obj)?;
                    self.mouse_click(x, y)?;
                }
                Ok(())
            }
            "file" => self
                .call("DOM.setFileInputFiles", obj! { "files" => vec![Value::from(text)], "backendNodeId" => ent.backend })
                .map(|_| ()),
            _ => Err(not_actionable(r, ent, "is not a fillable field")),
        }
    }

    pub fn press(&self, combo: &str) -> Res<Value> {
        let mark = self.mark()?;
        self.key(combo)?;
        self.finish(format!("press {}", combo), &mark, GRACE)
    }

    fn evaluate(&self, expr: &str) -> Res<Value> {
        let r = self.call(
            "Runtime.evaluate",
            obj! { "expression" => expr, "returnByValue" => true, "awaitPromise" => true, "userGesture" => true },
        )?;
        let ex = r.get("exceptionDetails");
        if !ex.is_null() {
            let d = ex.at(&["exception", "description"]).as_str().unwrap_or(ex.get("text").str());
            return Err(KbErr::new("eval_error", d));
        }
        Ok(r.get("result").clone())
    }

    pub fn eval(&self, expr: &str) -> Res<Value> {
        let res = self.evaluate(expr)?;
        let value = res.get("value").clone();
        let text = match &value {
            Value::Str(s) => s.clone(),
            Value::Null if res.get("type").str() == "undefined" => "undefined".to_string(),
            v => v.to_string(),
        };
        Ok(obj! { "text" => text, "value" => value, "tab" => self.tab })
    }

    pub fn text(&self, r: Option<u32>) -> Res<Value> {
        let value = match r {
            None => self.evaluate("document.body ? document.body.innerText : ''")?.get("value").clone(),
            Some(r) => {
                let (_, obj) = self.resolve(r)?;
                let v = self.func(&obj, JS_TEXT, vec![]);
                self.release();
                v?
            }
        };
        Ok(obj! { "text" => value.str(), "tab" => self.tab })
    }

    pub fn wait(&self, what: &str, gone: bool, as_text: bool, explicit_timeout: bool) -> Res<Value> {
        let timeout = if explicit_timeout { self.timeout } else { Duration::from_secs(10) };
        let is_ref = !as_text && (what.starts_with('@') || what.chars().all(|c| c.is_ascii_digit()));
        let end = Instant::now() + timeout;
        let probe = format!(
            "(function(t){{ function has(w){{ try {{ if (w.document.body && w.document.body.innerText.includes(t)) return true; \
             for (let i = 0; i < w.frames.length; i++) if (has(w.frames[i])) return true; }} catch (e) {{}} return false; }} \
             return has(window); }})({})",
            quote(what)
        );
        loop {
            let present = if is_ref {
                match self.resolve(crate::daemon::parse_ref(what)?) {
                    Ok(_) => {
                        self.release();
                        true
                    }
                    Err(e) if e.code == "stale_ref" => false,
                    Err(e) => return Err(e),
                }
            } else {
                // A navigation mid-poll destroys the context; just poll again.
                match self.evaluate(&probe) {
                    Ok(v) => v.get("value").as_bool() == Some(true),
                    Err(e) if e.code == "chrome" || e.code == "eval_error" => false,
                    Err(e) => return Err(e),
                }
            };
            if present != gone {
                let state = if gone { "gone" } else { "present" };
                return Ok(obj! { "text" => format!("ok wait {} {} | tab={}", quote(what), state, self.tab), "tab" => self.tab });
            }
            if Instant::now() >= end {
                let want = if gone { "disappear" } else { "appear" };
                return Err(KbErr::new("timeout", format!("{} did not {} in {}ms", quote(what), want, timeout.as_millis())));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn screenshot(&self, path: &str) -> Res<Value> {
        let r = self.call("Page.captureScreenshot", obj! { "format" => "png" })?;
        let bytes = crate::util::base64_decode(r.get("data").str()).map_err(|e| KbErr::new("chrome", e))?;
        std::fs::write(path, &bytes).map_err(|e| KbErr::new("usage", format!("cannot write {}: {}", path, e)))?;
        Ok(obj! { "text" => format!("ok screenshot {} ({} bytes) | tab={}", path, bytes.len(), self.tab), "path" => path, "bytes" => bytes.len() })
    }

    pub fn history(&self, delta: i64) -> Res<Value> {
        let h = self.call("Page.getNavigationHistory", obj! {})?;
        let want = h.get("currentIndex").as_i64().unwrap_or(0) + delta;
        let name = if delta < 0 { "back" } else { "forward" };
        let entry = usize::try_from(want)
            .ok()
            .and_then(|i| h.get("entries").arr().get(i))
            .ok_or_else(|| KbErr::new("not_actionable", format!("no history entry to go {}", name)))?;
        let mark = self.mark()?;
        self.call("Page.navigateToHistoryEntry", obj! { "entryId" => entry.get("id").as_i64().unwrap_or(0) })?;
        self.finish(name.into(), &mark, Duration::from_millis(500))
    }

    pub fn reload(&self) -> Res<Value> {
        let mark = self.mark()?;
        self.call("Page.reload", obj! {})?;
        self.finish("reload".into(), &mark, Duration::from_millis(1000))
    }

    pub fn dialog(&self, action: &str, prompt: Option<&str>) -> Res<Value> {
        let accept = match action {
            "accept" => true,
            "dismiss" => false,
            _ => return Err(KbErr::usage("usage: kb dialog accept [prompt text] | kb dialog dismiss")),
        };
        if !self.with_tab(|t| t.dialog.is_some())? {
            return Err(KbErr::new("not_actionable", format!("no dialog is open on tab {}", self.tab)));
        }
        let mut p = obj! { "accept" => accept };
        if let Some(text) = prompt {
            p.set("promptText", text);
        }
        // Browser-independent of the blocked page: must not be dialog-cancelled.
        self.cdp
            .call(&self.session, "Page.handleJavaScriptDialog", p, self.timeout)
            .map_err(|e| KbErr::new("chrome", format!("Page.handleJavaScriptDialog: {:?}", e)))?;
        self.shared.wait(Duration::from_secs(2), |st| match st.tab(self.tab) {
            Some(t) if t.dialog.is_some() => None,
            _ => Some(()),
        });
        Ok(obj! { "text" => format!("ok dialog {} | tab={}", action, self.tab), "tab" => self.tab })
    }
}

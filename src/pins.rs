//! Pins: human (or agent) notes attached to page elements, numbered `#N`.
//!
//! A pin keeps a durable locator (URL + CSS selector + role/name/text), so
//! unlike a ref it survives reloads and daemon restarts. While its element
//! is present it is "attached" and also carries a live ref.

use crate::daemon::{Ctx, KbErr, RefEntry, Res, State};
use crate::json::{self, quote, Value};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Default)]
pub struct Pin {
    pub id: u32,
    pub note: String,
    /// Created by a right-click whose note is still being typed.
    pub draft: bool,
    pub url: String,
    pub title: String,
    pub tab: u32,
    pub selector: String,
    pub role: String,
    pub name: String,
    pub text: String,
    pub tag: String,
    pub rect: [i64; 4],
    pub viewport: [i64; 2],
    /// Where inside the element the human clicked, in permille of its box.
    /// Large elements show their marker there instead of at the corner.
    pub anchor: Option<[i64; 2]>,
    /// Note text being typed in the viewer right now; shown on the page in
    /// place of the saved note. Not persisted.
    pub editing: Option<String>,
    /// Placeholder for the empty input bubble, in the viewer's language.
    pub placeholder: String,
    pub frame_url: String,
    pub created: u64,
    /// Live attachment: (frame id, backend node id). Not persisted.
    pub node: Option<(String, i64)>,
    pub ref_n: Option<u32>,
}

/// Element under a viewport point, descending into same-origin iframes and
/// open shadow roots, then lifted to the nearest interactive ancestor.
const JS_AT: &str = r#"(function(x, y){
  let d = document, el = null, ox = 0, oy = 0;
  for (;;) {
    let e = d.elementFromPoint(x - ox, y - oy);
    if (!e) break;
    while (e.shadowRoot) {
      const s = e.shadowRoot.elementFromPoint(x - ox, y - oy);
      if (!s || s === e) break;
      e = s;
    }
    el = e;
    if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
      let cd = null;
      try { cd = e.contentDocument; } catch (_) {}
      if (!cd) break;
      const r = e.getBoundingClientRect();
      ox += r.left + e.clientLeft; oy += r.top + e.clientTop;
      d = cd;
      continue;
    }
    break;
  }
  if (!el) return null;
  return el.closest('a,button,input,select,textarea,summary,label,[role=button],[role=link],[role=tab],[role=menuitem],[role=checkbox],[role=radio],[role=switch],[role=option],[contenteditable=""],[contenteditable=true]') || el;
})"#;

const JS_DESCRIBE: &str = r#"function(){
  const el = this, doc = el.ownerDocument, w = doc.defaultView;
  const unique = function(e){ return e.id && doc.querySelectorAll('#' + CSS.escape(e.id)).length === 1; };
  const parts = [];
  let n = el, anchored = false;
  while (n && n.nodeType === 1 && n !== doc.documentElement) {
    if (unique(n)) { parts.unshift('#' + CSS.escape(n.id)); anchored = true; break; }
    let p = n.localName;
    const par = n.parentElement;
    if (par) {
      const same = Array.prototype.filter.call(par.children, function(c){ return c.localName === n.localName; });
      if (same.length > 1) p += ':nth-of-type(' + (same.indexOf(n) + 1) + ')';
    }
    parts.unshift(p);
    n = par;
  }
  const selector = (anchored ? '' : 'html > ') + parts.join(' > ') || 'html';
  const r = el.getBoundingClientRect();
  const text = String(el.innerText || el.value || el.getAttribute('aria-label') || el.alt || '').trim().replace(/\s+/g, ' ').slice(0, 120);
  return {selector: selector, text: text, tag: el.localName,
    rect: [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height)],
    viewport: [w.innerWidth, w.innerHeight], frame_url: w.location.href};
}"#;

/// Find a pinned element again by selector in the frame it was pinned in.
const JS_FIND: &str = r#"(function(sel, furl){
  function walk(w){
    try {
      if (w.location.href === furl) { const e = w.document.querySelector(sel); if (e) return e; }
      for (let i = 0; i < w.frames.length; i++) { const r = walk(w.frames[i]); if (r) return r; }
    } catch (e) {}
    return null;
  }
  return walk(window);
})"#;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Unix seconds -> `2026-11-13T05:02:11Z`.
pub fn iso(secs: u64) -> String {
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

impl Pin {
    pub fn to_json(&self) -> Value {
        let nums = |a: &[i64]| Value::Arr(a.iter().map(|n| Value::from(*n)).collect());
        let mut v = obj! {
            "id" => self.id, "note" => &self.note, "url" => &self.url, "title" => &self.title,
            "tab" => self.tab, "attached" => self.node.is_some(),
            "role" => &self.role, "name" => &self.name, "selector" => &self.selector,
            "text" => &self.text, "tag" => &self.tag, "rect" => nums(&self.rect), "viewport" => nums(&self.viewport),
            "frame_url" => &self.frame_url, "created" => iso(self.created), "created_unix" => self.created,
        };
        if let Some(r) = self.ref_n.filter(|_| self.node.is_some()) {
            v.set("ref", r);
        }
        if let Some(a) = &self.anchor {
            v.set("anchor", nums(a));
        }
        v
    }

    fn from_json(v: &Value) -> Option<Pin> {
        let s = |k: &str| v.get(k).str().to_string();
        let nums = |k: &str| v.get(k).arr().iter().map(|n| n.as_i64().unwrap_or(0)).collect::<Vec<i64>>();
        Some(Pin {
            id: v.get("id").as_i64()? as u32,
            note: s("note"),
            url: s("url"),
            title: s("title"),
            tab: 0,
            selector: s("selector"),
            role: s("role"),
            name: s("name"),
            text: s("text"),
            tag: s("tag"),
            rect: nums("rect").try_into().unwrap_or([0; 4]),
            viewport: nums("viewport").try_into().unwrap_or([0; 2]),
            anchor: nums("anchor").try_into().ok(),
            frame_url: s("frame_url"),
            created: v.get("created_unix").as_i64().unwrap_or(0) as u64,
            ..Pin::default()
        })
    }

    fn element(&self) -> String {
        let what = if self.role.is_empty() { self.tag.clone() } else { self.role.clone() };
        if self.name.is_empty() { what } else { format!("{} {}", what, quote(&self.name)) }
    }
}

/// The hand-off document for an agent.
pub fn markdown(pins: &[Pin]) -> String {
    let mut out = format!("# kb pins ({})\n", pins.len());
    if pins.is_empty() {
        out.push_str("\nNo pins.\n");
    }
    for p in pins {
        let live = match (p.node.is_some(), p.ref_n) {
            (true, Some(r)) => format!("ref @{}, tab {}", r, p.tab),
            _ => "detached: element not on any open page".to_string(),
        };
        out.push_str(&format!("\n## #{} {}\n", p.id, p.note.replace('\n', " ")));
        out.push_str(&format!("- page: {} - {}\n", p.title, p.url));
        out.push_str(&format!("- element: {} ({})\n", p.element(), live));
        out.push_str(&format!("- selector: `{}`\n", p.selector));
        if !p.text.is_empty() {
            out.push_str(&format!("- text: {}\n", quote(&p.text)));
        }
        if p.frame_url != p.url {
            out.push_str(&format!("- frame: {}\n", p.frame_url));
        }
        out.push_str(&format!(
            "- box: {},{} {}x{} in {}x{}\n",
            p.rect[0], p.rect[1], p.rect[2], p.rect[3], p.viewport[0], p.viewport[1]
        ));
        out.push_str(&format!("- created: {}\n", iso(p.created)));
    }
    out
}

pub fn list_value(st: &State) -> Value {
    let pins: Vec<Pin> = st.pins.iter().filter(|p| !p.draft).cloned().collect();
    obj! {
        "text" => markdown(&pins).trim_end(),
        "pins" => pins.iter().map(Pin::to_json).collect::<Vec<_>>(),
    }
}

pub fn load(home: &Path) -> (Vec<Pin>, u32) {
    let Some(v) = std::fs::read_to_string(home.join("pins.json")).ok().and_then(|t| json::parse(&t).ok()) else {
        return (Vec::new(), 0);
    };
    let pins: Vec<Pin> = v.get("pins").arr().iter().filter_map(Pin::from_json).collect();
    let next = v.get("next").as_i64().unwrap_or(0).max(pins.iter().map(|p| p.id as i64).max().unwrap_or(0)) as u32;
    (pins, next)
}

/// Persist pins (drafts excluded). Write-then-rename so the file is never partial.
pub fn save(home: &Path, st: &State) {
    let pins: Vec<Value> = st.pins.iter().filter(|p| !p.draft).map(Pin::to_json).collect();
    let tmp = home.join("pins.json.tmp");
    let body = obj! { "next" => st.next_pin, "pins" => pins }.to_string();
    if std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, home.join("pins.json"))).is_err() {
        crate::util::log("cannot write pins.json");
    }
}

use crate::daemon::object_group;

impl Ctx {
    /// Backend node id of a remote object.
    fn backend_of(&self, object_id: &str) -> Res<i64> {
        let d = self.call("DOM.describeNode", obj! { "objectId" => object_id })?;
        d.at(&["node", "backendNodeId"]).as_i64().ok_or_else(|| KbErr::new("chrome", "node has no backend id"))
    }

    /// Give a node a ref (reusing the one a snapshot already issued).
    fn ref_of_node(&self, backend: i64, role: &str, name: &str) -> Res<(String, u32)> {
        let target = self.target.clone();
        self.with_tab(|t| {
            let frame = t.ref_ix.keys().find(|k| k.1 == backend).map(|k| k.0.clone()).unwrap_or(target);
            let n = t.ref_for(RefEntry { frame: frame.clone(), backend, role: role.to_string(), name: name.to_string() });
            (frame, n)
        })
    }

    /// Pin the element behind `object_id`, or return the pin already on it.
    fn pin_object(&self, object_id: &str, note: &str, draft: bool) -> Res<Value> {
        let backend = self.backend_of(object_id)?;
        let existing = {
            let st = self.shared.lock();
            st.pins.iter().find(|p| p.tab == self.tab && p.node.as_ref().map(|n| n.1) == Some(backend)).cloned()
        };
        if let Some(p) = existing {
            if !draft && !note.is_empty() {
                let mut st = self.shared.lock();
                if let Some(q) = st.pins.iter_mut().find(|q| q.id == p.id) {
                    q.note = note.to_string();
                    q.draft = false;
                }
                save(&self.shared.home, &st);
            }
            return Ok(obj! { "pin" => p.id, "existing" => !p.draft, "note" => &p.note, "element" => p.element() });
        }
        let r = self.call(
            "Runtime.callFunctionOn",
            obj! { "objectId" => object_id, "functionDeclaration" => JS_DESCRIBE, "returnByValue" => true },
        )?;
        let d = r.at(&["result", "value"]);
        if d.get("selector").str().is_empty() {
            return Err(KbErr::new("not_actionable", "cannot describe that element"));
        }
        // Role and name as the accessibility tree (and so `kb snapshot`) reports them.
        let ax = self.call("Accessibility.getPartialAXTree", obj! { "backendNodeId" => backend, "fetchRelatives" => false });
        let (mut role, mut name) = (String::new(), String::new());
        if let Ok(ax) = ax {
            if let Some(n) = ax.get("nodes").arr().first().filter(|n| n.get("ignored").as_bool() != Some(true)) {
                role = n.at(&["role", "value"]).str().to_string();
                name = n.at(&["name", "value"]).str().chars().take(120).collect();
            }
        }
        if role.is_empty() || role == "generic" || role == "none" {
            role = d.get("tag").str().to_string();
        }
        let (frame, ref_n) = self.ref_of_node(backend, &role, &name)?;
        let (url, title) = self.page_info()?;
        let nums = |k: &str| d.get(k).arr().iter().map(|n| n.as_i64().unwrap_or(0)).collect::<Vec<i64>>();
        let mut st = self.shared.lock();
        st.pins.retain(|p| !p.draft); // an abandoned draft from a crashed viewer
        st.next_pin += 1;
        let pin = Pin {
            id: st.next_pin,
            note: note.to_string(),
            draft,
            url,
            title,
            tab: self.tab,
            selector: d.get("selector").str().to_string(),
            role,
            name,
            text: d.get("text").str().to_string(),
            tag: d.get("tag").str().to_string(),
            rect: nums("rect").try_into().unwrap_or([0; 4]),
            viewport: nums("viewport").try_into().unwrap_or([0; 2]),
            anchor: None,
            editing: None,
            placeholder: String::new(),
            frame_url: d.get("frame_url").str().to_string(),
            created: now(),
            node: Some((frame, backend)),
            ref_n: Some(ref_n),
        };
        let out = obj! { "pin" => pin.id, "existing" => false, "note" => "", "element" => pin.element(), "ref" => ref_n };
        st.pins.push(pin);
        if !draft {
            save(&self.shared.home, &st);
        }
        Ok(out)
    }

    /// Viewer right-click: start (or reopen) a pin on the element at a viewport point.
    pub fn pin_at(&self, x: i64, y: i64, placeholder: &str) -> Res<Value> {
        let group = object_group();
        let r = self.call(
            "Runtime.evaluate",
            obj! { "expression" => format!("{}({}, {})", JS_AT, x, y), "objectGroup" => &group },
        )?;
        let object_id = r.at(&["result", "objectId"]).str().to_string();
        if object_id.is_empty() {
            return Err(KbErr::new("not_actionable", format!("no element at {},{}", x, y)));
        }
        let out = self.pin_object(&object_id, "", true);
        self.cdp.send(&self.session, "Runtime.releaseObjectGroup", obj! { "objectGroup" => &group });
        // Remember where in the element the click landed.
        if let Ok(v) = &out {
            let id = v.get("pin").as_i64().unwrap_or(0) as u32;
            let mut st = self.shared.lock();
            if let Some(p) = st.pins.iter_mut().find(|p| p.id == id) {
                p.editing = Some(p.note.clone());
                p.placeholder = placeholder.to_string();
                let [l, t, w, h] = p.rect;
                if w > 0 && h > 0 && (l..=l + w).contains(&x) && (t..=t + h).contains(&y) {
                    p.anchor = Some([(x - l) * 1000 / w, (y - t) * 1000 / h]);
                }
            }
        }
        out
    }

    /// `kb pin add <ref> <note>`.
    pub fn pin_add(&self, r: u32, note: &str) -> Res<Value> {
        let ent = crate::snapshot::lookup(self, r)?;
        let group = object_group();
        let v = self
            .call("DOM.resolveNode", obj! { "backendNodeId" => ent.backend, "objectGroup" => &group })
            .map_err(|e| if e.code == "chrome" { crate::snapshot::stale(r, &ent) } else { e })?;
        let out = self.pin_object(v.at(&["object", "objectId"]).str(), note, false);
        self.cdp.send(&self.session, "Runtime.releaseObjectGroup", obj! { "objectGroup" => &group });
        let mut out = out?;
        let id = out.get("pin").as_i64().unwrap_or(0);
        out.set("text", format!("ok pin #{} on @{} {} | tab={}", id, r, out.get("element").str(), self.tab));
        Ok(out)
    }

    /// Re-check this tab's pins: detach those whose element is gone, attach
    /// those whose page is open here and whose selector resolves.
    pub fn pins_reattach(&self) -> Res<()> {
        let group = object_group();
        let url = self.with_tab(|t| t.url.clone())?;
        let pins: Vec<Pin> = self.shared.lock().pins.iter().filter(|p| !p.draft).cloned().collect();
        for p in pins {
            let mut node = p.node.clone().filter(|_| p.tab == self.tab);
            if let Some((_, backend)) = &node {
                let alive = self
                    .call("DOM.resolveNode", obj! { "backendNodeId" => *backend, "objectGroup" => &group })
                    .ok()
                    .and_then(|v| {
                        self.call(
                            "Runtime.callFunctionOn",
                            obj! {
                                "objectId" => v.at(&["object", "objectId"]).str(),
                                "functionDeclaration" => "function(){ return this.isConnected; }", "returnByValue" => true,
                            },
                        )
                        .ok()
                    })
                    .map(|v| v.at(&["result", "value"]).as_bool() == Some(true))
                    .unwrap_or(false);
                if !alive {
                    node = None;
                }
            }
            let elsewhere = p.node.is_some() && p.tab != self.tab;
            let mut ref_n = p.ref_n;
            if node.is_none() && !elsewhere && p.url == url {
                let expr = format!("{}({}, {})", JS_FIND, quote(&p.selector), quote(&p.frame_url));
                let found = self.call("Runtime.evaluate", obj! { "expression" => expr, "objectGroup" => &group });
                let object_id = found.map(|v| v.at(&["result", "objectId"]).str().to_string()).unwrap_or_default();
                if !object_id.is_empty() {
                    if let Ok(backend) = self.backend_of(&object_id) {
                        let (frame, n) = self.ref_of_node(backend, &p.role, &p.name)?;
                        node = Some((frame, backend));
                        ref_n = Some(n);
                    }
                }
            }
            if elsewhere {
                continue;
            }
            let mut st = self.shared.lock();
            if let Some(q) = st.pins.iter_mut().find(|q| q.id == p.id) {
                if q.node != node {
                    q.node = node.clone();
                    q.ref_n = if node.is_some() { ref_n } else { None };
                    if node.is_some() {
                        q.tab = self.tab;
                    }
                }
            }
        }
        self.cdp.send(&self.session, "Runtime.releaseObjectGroup", obj! { "objectGroup" => &group });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso(1_794_537_374), "2026-11-13T02:36:14Z");
    }

    #[test]
    fn markdown_and_json_roundtrip() {
        let pin = Pin {
            id: 2,
            note: "색 바꿔줘".into(),
            url: "http://x/login".into(),
            title: "Login".into(),
            tab: 1,
            selector: "#go".into(),
            role: "button".into(),
            name: "Log in".into(),
            text: "Log in".into(),
            tag: "button".into(),
            rect: [10, 20, 96, 32],
            viewport: [1920, 1075],
            frame_url: "http://x/login".into(),
            created: 0,
            node: Some(("F".into(), 7)),
            ref_n: Some(3),
            ..Pin::default()
        };
        let md = markdown(&[pin.clone()]);
        for want in ["# kb pins (1)", "## #2 색 바꿔줘", "- page: Login - http://x/login", "- element: button \"Log in\" (ref @3, tab 1)", "- selector: `#go`", "- box: 10,20 96x32 in 1920x1075"] {
            assert!(md.contains(want), "missing {:?} in:\n{}", want, md);
        }
        let back = Pin::from_json(&json::parse(&pin.to_json().to_string()).unwrap()).unwrap();
        assert_eq!((back.id, back.note.as_str(), back.selector.as_str(), back.rect), (2, "색 바꿔줘", "#go", [10, 20, 96, 32]));
        assert!(back.node.is_none(), "live attachment is not persisted");
        let detached = Pin { node: None, ..pin };
        assert!(markdown(&[detached]).contains("detached"));
    }
}

//! On-page overlay: `@N` ref badges and `#N` pin markers.
//!
//! One host element per document, a direct child of `<html>` (outside
//! `<body>`, so `innerText` is unaffected), `aria-hidden` (invisible to the
//! accessibility snapshot) and `pointer-events:none` (invisible to clicks and
//! hit testing). Labels live in its shadow root and follow their elements.
//! Because the overlay is part of the page it shows up in the terminal
//! stream, in `kb screenshot` and in a headed window alike.

use crate::daemon::{Ctx, Res};
use crate::json::Value;
use crate::snapshot;

const MAX_BADGES: usize = 400;
/// Characters of a pin note shown on the page; the full note is in `kb pins`.
const MAX_NOTE: usize = 40;

/// `this` = any element of the target document; arguments = spec JSON, then
/// the labelled elements in spec order.
const JS_INSTALL: &str = r#"function(specJson){
  const spec = JSON.parse(specJson), els = Array.prototype.slice.call(arguments, 1);
  const doc = this.ownerDocument, win = doc.defaultView, root = doc.documentElement;
  let host = root.querySelector(':scope > [data-kb-overlay]');
  if (!host || !host._kb) {
    if (host) host.remove();
    host = doc.createElement('div');
    host.setAttribute('data-kb-overlay', '');
    host.setAttribute('aria-hidden', 'true');
    host.style.cssText = 'position:fixed;left:0;top:0;width:0;height:0;pointer-events:none;z-index:2147483647;';
    host.attachShadow({mode: 'open'});
    const st = host._kb = {items: [], dirty: false, gen: 0};
    st.place = function(){
      const vw = win.innerWidth, vh = win.innerHeight;
      for (const it of st.items) {
        let show = it.el.isConnected;
        if (show) {
          const r = it.el.getBoundingClientRect();
          show = r.width > 0 && r.height > 0 && r.bottom > 0 && r.right > 0 && r.top < vh && r.left < vw;
          if (show) {
            let x = it.kind === 'ref' ? r.left : r.right, y = r.top;
            // A pin on a big element sits where the human clicked, not at a
            // corner that may be far away or off screen.
            if (it.ax >= 0 && (r.width > 240 || r.height > 80)) {
              x = r.left + r.width * it.ax / 1000;
              y = r.top + r.height * it.ay / 1000;
            }
            x = Math.max(0, x); y = Math.max(0, y);
            it.tag.style.display = 'block';
            if (it.kind === 'note' || it.kind === 'edit') {
              // The note sits right of the element; near the right edge it
              // moves under the marker so it stays on screen.
              const w = it.tag.offsetWidth;
              if (x + w > vw) { x = Math.max(0, vw - w); y += it.tag.offsetHeight; }
            }
            it.tag.style.left = x + 'px';
            it.tag.style.top = y + 'px';
          }
        }
        if (!show) it.tag.style.display = 'none';
      }
    };
    st.timer = win.setInterval(function(){
      if (!host.isConnected) {
        win.clearInterval(st.timer);
        st.mo.disconnect();
        win.removeEventListener('scroll', st.place, true);
        return;
      }
      st.place();
    }, 200);
    win.addEventListener('scroll', st.place, {capture: true, passive: true});
    // Page changes mark the overlay dirty; the daemon then re-snapshots.
    st.mo = new win.MutationObserver(function(recs){
      for (const r of recs) {
        if (r.type === 'childList') {
          let own = r.addedNodes.length + r.removedNodes.length > 0;
          for (const n of r.addedNodes) if (n !== host) own = false;
          for (const n of r.removedNodes) if (n !== host) own = false;
          if (own) continue;
        }
        st.dirty = true;
        return;
      }
    });
    st.mo.observe(root, {childList: true, subtree: true, attributes: true,
      attributeFilter: ['hidden', 'disabled', 'aria-label', 'aria-hidden', 'href', 'type', 'role']});
    root.appendChild(host);
  }
  const st = host._kb, shadow = host.shadowRoot;
  st.gen = spec.gen;
  shadow.textContent = '';
  const s = spec.scale, px = function(n){ return Math.max(1, Math.round(n * s)) + 'px'; };
  st.items = els.map(function(el, i){
    const kind = spec.items[i][1], tag = doc.createElement(kind === 'pin' || kind === 'ref' ? 'span' : 'div');
    tag.textContent = spec.items[i][0];
    tag.dataset.kind = kind;
    tag.style.cssText = 'position:fixed;display:none;font:bold ' + px(11) + '/1.2 monospace;padding:0 ' + px(3) +
      ';border-radius:' + px(3) + ';white-space:nowrap;box-shadow:0 0 0 ' + px(1) + ' rgba(0,0,0,.65);' +
      (kind === 'pin' ? 'background:#e5006a;color:#fff;transform:translateX(-100%);' :
       kind === 'edit' ? 'background:#fff;color:#000;font-size:' + px(16) + ';padding:' + px(3) + ' ' + px(6) +
         ';outline:' + px(3) + ' solid #e5006a;' :
       kind === 'note' ? 'background:#fff;color:#000;font-size:' + px(14) + ';' : 'background:#ffd400;color:#000;');
    shadow.appendChild(tag);
    return {el: el, tag: tag, kind: kind, ax: spec.items[i][2], ay: spec.items[i][3]};
  });
  st.place();
  return st.items.length;
}"#;

/// Remove overlays of an older generation from every same-origin frame.
const JS_CLEAN: &str = r#"(function(gen){
  function walk(w){
    try {
      const h = w.document.documentElement.querySelector(':scope > [data-kb-overlay]');
      if (h && (!h._kb || h._kb.gen !== gen)) h.remove();
      for (let i = 0; i < w.frames.length; i++) walk(w.frames[i]);
    } catch (e) {}
  }
  walk(window);
})"#;

/// Forget page changes seen so far. Run *before* reading the page, so a
/// change that lands while the sync is under way is not lost: it marks the
/// overlay dirty again and the next pass picks it up.
const JS_MARK_CLEAN: &str = r#"(function(){
  const r = document.documentElement;
  const h = r && r.querySelector(':scope > [data-kb-overlay]');
  if (h && h._kb) h._kb.dirty = false;
})()"#;

/// "missing" | "dirty" | "ok" for the main document's overlay.
pub const JS_STATE: &str = r#"(function(){
  const r = document.documentElement;
  const h = r && r.querySelector(':scope > [data-kb-overlay]');
  if (!h || !h._kb) return 'missing';
  return h._kb.dirty ? 'dirty' : 'ok';
})()"#;

/// Bring the tab's overlay in line with its state: badges for every
/// interactive ref when badges are on, plus a marker per attached pin.
/// Returns the number of labels installed.
pub fn sync(ctx: &Ctx) -> Res<usize> {
    // One sync at a time: two running concurrently (a `kb badges` command and
    // the background watcher) could finish out of order, the slower one
    // overwriting the page with the state it read earlier.
    let _one = ctx.shared.overlay_lock.lock().unwrap_or_else(|e| e.into_inner());
    ctx.call("Runtime.evaluate", obj! { "expression" => JS_MARK_CLEAN })?;
    ctx.pins_reattach()?;
    let (badges, scale, gen, url) = ctx.with_tab(|t| {
        t.overlay_gen += 1;
        t.overlay_dirty = false;
        t.overlay_at = std::time::Instant::now();
        (t.badges, t.view_scale.max(1.0), t.overlay_gen, t.url.clone())
    })?;

    // (frame id, backend node id, label, kind)
    let mut items: Vec<(String, i64, String, &str, [i64; 2])> = Vec::new();
    if badges {
        let snap = snapshot::snapshot(ctx, &snapshot::Opts { interactive: true, scope: None, max_nodes: MAX_BADGES })?;
        let refs: Vec<u32> = snap.get("nodes").arr().iter().filter_map(|n| n.get("ref").as_i64()).map(|n| n as u32).collect();
        ctx.with_tab(|t| {
            for r in refs {
                if let Some(e) = t.refs.get(&r) {
                    items.push((e.frame.clone(), e.backend, format!("@{}", r), "ref", [-1, -1]));
                }
            }
        })?;
    }
    {
        let st = ctx.shared.lock();
        for p in st.pins.iter().filter(|p| p.tab == ctx.tab) {
            if let Some((frame, backend)) = &p.node {
                let at = p.anchor.unwrap_or([-1, -1]);
                items.push((frame.clone(), *backend, format!("#{}", p.id), "pin", at));
                // The note itself, next to the marker, so the human sees what
                // they wrote without leaving the page.
                if let Some(text) = p.editing.as_ref().or(p.draft.then_some(&p.note)) {
                    // Being typed in the viewer: an input bubble on the spot,
                    // showing the end of the text and a caret.
                    let all: Vec<char> = text.chars().filter(|c| !c.is_control()).collect();
                    let tail: String = all[all.len().saturating_sub(MAX_NOTE)..].iter().collect();
                    let hint = if p.placeholder.is_empty() { "type a note, Enter saves, Esc cancels" } else { &p.placeholder };
                    let label = if all.is_empty() { hint.to_string() } else { format!("{}\u{258f}", tail) };
                    items.push((frame.clone(), *backend, label, "edit", at));
                    continue;
                }
                let mut note: String = p.note.chars().filter(|c| !c.is_control()).take(MAX_NOTE).collect();
                if p.note.chars().count() > MAX_NOTE {
                    note.push('…');
                }
                if !note.is_empty() {
                    items.push((frame.clone(), *backend, note, "note", at));
                }
            }
        }
    }

    let mut installed = 0;
    let mut frames: Vec<String> = Vec::new();
    for (frame, ..) in &items {
        if !frames.contains(frame) {
            frames.push(frame.clone());
        }
    }
    for frame in frames {
        let mut labels: Vec<Value> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        for (_, backend, label, kind, at) in items.iter().filter(|i| i.0 == frame) {
            // Nodes that vanished since the snapshot are simply skipped.
            if let Ok(v) = ctx.call("DOM.resolveNode", obj! { "backendNodeId" => *backend, "objectGroup" => "kb-overlay" }) {
                labels.push(Value::Arr(vec![Value::from(label), Value::from(*kind), Value::from(at[0]), Value::from(at[1])]));
                args.push(obj! { "objectId" => v.at(&["object", "objectId"]).str() });
            }
        }
        let Some(first) = args.first().map(|a| a.get("objectId").str().to_string()) else { continue };
        let spec = obj! { "gen" => gen, "scale" => scale, "items" => labels }.to_string();
        args.insert(0, obj! { "value" => spec });
        let r = ctx.call(
            "Runtime.callFunctionOn",
            obj! { "objectId" => first, "functionDeclaration" => JS_INSTALL, "arguments" => args, "returnByValue" => true },
        )?;
        installed += r.at(&["result", "value"]).as_i64().unwrap_or(0) as usize;
    }
    ctx.call("Runtime.evaluate", obj! { "expression" => format!("{}({})", JS_CLEAN, gen) })?;
    ctx.cdp.send(&ctx.session, "Runtime.releaseObjectGroup", obj! { "objectGroup" => "kb-overlay" });
    ctx.with_tab(|t| t.overlay_last = (url, installed))?;
    Ok(installed)
}

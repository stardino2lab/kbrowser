//! Accessibility-tree snapshot with stable `@N` refs on interactive nodes.

use crate::daemon::{Ctx, KbErr, RefEntry, Res};
use crate::json::{quote, Value};
use std::collections::HashMap;

pub struct Opts {
    pub interactive: bool,
    pub scope: Option<u32>,
    pub max_nodes: usize,
}

#[derive(Default)]
struct Node {
    role: String,
    name: String,
    value: String,
    ignored: bool,
    backend: i64,
    frame: usize,
    kids: Vec<usize>,
    focusable: bool,
    attrs: Vec<String>,
}

const INTERACTIVE: &[&str] = &[
    "button", "link", "textbox", "searchbox", "combobox", "checkbox", "radio", "switch", "slider",
    "spinbutton", "menuitem", "menuitemcheckbox", "menuitemradio", "tab", "option", "listbox",
    "treeitem", "PopUpButton", "ColorWell", "Date", "DateTime", "InputTime", "DisclosureTriangle",
];
/// Containers that are shown only through their children.
const TRANSPARENT: &[&str] = &["generic", "none", "RootWebArea", "paragraph", "LayoutTable", "LayoutTableRow", "LayoutTableCell"];
const SKIP_SUBTREE: &[&str] = &["InlineTextBox", "ListMarker", "LineBreak"];
const NAME_MAX: usize = 120;

fn clip(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > NAME_MAX {
        format!("{}...", s.chars().take(NAME_MAX).collect::<String>())
    } else {
        s
    }
}

fn frames_of(tree: &Value, parent: Option<usize>, out: &mut Vec<(String, Option<usize>)>) {
    let me = out.len();
    out.push((tree.at(&["frame", "id"]).str().to_string(), parent));
    for child in tree.get("childFrames").arr() {
        frames_of(child, Some(me), out);
    }
}

fn load_frame(nodes: &mut Vec<Node>, frame: usize, ax: &Value) -> Option<usize> {
    let base = nodes.len();
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    let list = ax.get("nodes").arr();
    for (i, n) in list.iter().enumerate() {
        by_id.insert(n.get("nodeId").str(), base + i);
        let mut node = Node {
            role: n.at(&["role", "value"]).str().to_string(),
            name: clip(n.at(&["name", "value"]).str()),
            ignored: n.get("ignored").as_bool().unwrap_or(false),
            backend: n.get("backendDOMNodeId").as_i64().unwrap_or(0),
            frame,
            ..Node::default()
        };
        node.value = match n.at(&["value", "value"]) {
            Value::Str(s) => clip(s),
            Value::Num(x) => Value::Num(*x).to_string(),
            _ => String::new(),
        };
        for p in n.get("properties").arr() {
            let v = p.at(&["value", "value"]);
            let on = v.as_bool() == Some(true);
            match p.get("name").str() {
                "focusable" => node.focusable = on,
                "disabled" if on => node.attrs.push("disabled".into()),
                "required" if on => node.attrs.push("required".into()),
                "checked" => match v.str() {
                    "true" => node.attrs.push("checked".into()),
                    "mixed" => node.attrs.push("checked=mixed".into()),
                    _ => {}
                },
                "expanded" => node.attrs.push(if on { "expanded" } else { "collapsed" }.into()),
                "selected" if on => node.attrs.push("selected".into()),
                _ => {}
            }
        }
        nodes.push(node);
    }
    let mut root = None;
    for (i, n) in list.iter().enumerate() {
        if n.get("parentId").is_null() && root.is_none() {
            root = Some(base + i);
        }
        let kids: Vec<usize> = n.get("childIds").arr().iter().filter_map(|c| by_id.get(c.str()).copied()).collect();
        nodes[base + i].kids = kids;
    }
    root
}

struct Line {
    depth: usize,
    node: usize,
    interactive: bool,
}

pub fn snapshot(ctx: &Ctx, o: &Opts) -> Res<Value> {
    // Frames, parents first.
    let tree = ctx.call("Page.getFrameTree", obj! {})?;
    let mut frames = Vec::new();
    frames_of(tree.get("frameTree"), None, &mut frames);

    let mut nodes: Vec<Node> = Vec::new();
    let mut main_root = None;
    for (fi, (frame_id, parent)) in frames.iter().enumerate() {
        let ax = ctx.call("Accessibility.getFullAXTree", obj! { "frameId" => frame_id });
        let root = match (ax, parent) {
            (Ok(ax), _) => load_frame(&mut nodes, fi, &ax),
            (Err(e), None) => return Err(e),
            (Err(_), Some(_)) => {
                // Out-of-process iframe: its tree lives in another session.
                nodes.push(Node {
                    role: "note".into(),
                    name: "cross-origin iframe content is not supported yet".into(),
                    frame: fi,
                    ..Node::default()
                });
                Some(nodes.len() - 1)
            }
        };
        let (Some(root), Some(parent)) = (root, parent) else {
            if parent.is_none() {
                main_root = root;
            }
            continue;
        };
        // Hang the child document under its <iframe> element.
        let owner = ctx
            .call("DOM.getFrameOwner", obj! { "frameId" => frame_id })
            .map(|r| r.get("backendNodeId").as_i64().unwrap_or(0))
            .unwrap_or(0);
        let slot = nodes.iter().position(|n| n.frame == *parent && n.backend == owner && owner != 0);
        match slot.or_else(|| nodes.iter().position(|n| n.frame == *parent)) {
            Some(i) => nodes[i].kids.push(root),
            None => {}
        }
    }
    let main_root = main_root.ok_or_else(|| KbErr::new("chrome", "empty accessibility tree"))?;

    let start = match o.scope {
        None => main_root,
        Some(r) => {
            let ent = lookup(ctx, r)?;
            let fi = frames.iter().position(|f| f.0 == ent.frame);
            nodes
                .iter()
                .position(|n| Some(n.frame) == fi && n.backend == ent.backend)
                .ok_or_else(|| stale(r, &ent))?
        }
    };

    // Iterative walk: (node, depth, name of nearest printed ancestor).
    let mut lines: Vec<Line> = Vec::new();
    let mut hidden = 0usize;
    let mut stack: Vec<(usize, usize, usize)> = vec![(start, 0, usize::MAX)];
    while let Some((i, depth, anc)) = stack.pop() {
        let n = &nodes[i];
        if SKIP_SUBTREE.contains(&n.role.as_str()) {
            continue;
        }
        let interactive = !n.ignored
            && n.role != "RootWebArea"
            && n.role != "Iframe"
            && n.backend != 0
            && (INTERACTIVE.contains(&n.role.as_str()) || n.focusable);
        let show = interactive
            || (!n.ignored
                && !n.name.is_empty()
                && !TRANSPARENT.contains(&n.role.as_str())
                && !(n.role == "StaticText" && anc != usize::MAX && nodes[anc].name.contains(&n.name)));
        let (kid_depth, kid_anc) = if show {
            if !o.interactive || interactive {
                if lines.len() < o.max_nodes {
                    lines.push(Line { depth, node: i, interactive });
                } else {
                    hidden += 1;
                }
            }
            (depth + 1, if n.name.is_empty() { anc } else { i })
        } else {
            (depth, anc)
        };
        for k in n.kids.iter().rev() {
            stack.push((*k, kid_depth, kid_anc));
        }
    }

    // Assign refs under one lock so numbering is consistent.
    let refs: Vec<Option<u32>> = ctx.with_tab(|t| {
        lines
            .iter()
            .map(|l| {
                let n = &nodes[l.node];
                l.interactive.then(|| {
                    t.ref_for(RefEntry {
                        frame: frames[n.frame].0.clone(),
                        backend: n.backend,
                        role: n.role.clone(),
                        name: n.name.clone(),
                    })
                })
            })
            .collect()
    })?;

    let (url, title) = ctx.page_info()?;
    let (url, title) = (url.as_str(), title.as_str());
    let mut text = format!("[tab {}] {} - {}\n", ctx.tab, title, url);
    let mut list = Vec::new();
    for (l, r) in lines.iter().zip(&refs) {
        let n = &nodes[l.node];
        let role = if n.role == "StaticText" { "text" } else { n.role.as_str() };
        let indent = if o.interactive { 1 } else { l.depth + 1 };
        text.push_str(&"  ".repeat(indent));
        if let Some(r) = r {
            text.push_str(&format!("@{} ", r));
        }
        text.push_str(role);
        if !n.name.is_empty() {
            text.push(' ');
            text.push_str(&quote(&n.name));
        }
        let mut item = obj! { "role" => role, "name" => &n.name, "depth" => l.depth };
        if let Some(r) = r {
            item.set("ref", *r);
        }
        if !n.value.is_empty() && n.value != n.name {
            text.push_str(&format!(" value={}", quote(&n.value)));
            item.set("value", &n.value);
        }
        for a in &n.attrs {
            text.push(' ');
            text.push_str(a);
        }
        if !n.attrs.is_empty() {
            item.set("attrs", n.attrs.iter().map(|a| Value::from(a)).collect::<Vec<_>>());
        }
        text.push('\n');
        list.push(item);
    }
    if hidden > 0 {
        text.push_str(&format!(
            "  ... truncated: {} more nodes (narrow with --scope <ref>, -i, or raise --max-nodes)\n",
            hidden
        ));
    }
    Ok(obj! {
        "text" => text.trim_end(), "tab" => ctx.tab, "url" => url, "title" => title,
        "nodes" => list, "truncated" => hidden,
    })
}

pub fn stale(r: u32, ent: &RefEntry) -> KbErr {
    KbErr::new("stale_ref", format!("stale ref @{} ({} {}): element is gone", r, ent.role, quote(&ent.name)))
        .hint("run: kb snapshot")
}

/// Ref table lookup. A number that was issued but dropped by a navigation is stale.
pub fn lookup(ctx: &Ctx, r: u32) -> Res<RefEntry> {
    ctx.with_tab(|t| match t.refs.get(&r) {
        Some(e) => Ok(e.clone()),
        None if r < t.next_ref => Err(KbErr::new(
            "stale_ref",
            format!("stale ref @{}: the page navigated since it was issued", r),
        )
        .hint("run: kb snapshot")),
        None => Err(KbErr::usage(format!("unknown ref @{}: not in any snapshot of tab {}", r, t.id))
            .hint("run: kb snapshot")),
    })?
}

// kb browser bar: an address bar for `kb`, the terminal browser that agents
// and people share (https://github.com/stardino2lab/kbrowser).
//
// Type a URL and press Enter: the page opens in a `kb view` pane to the right
// of the focused terminal. If a viewer is already showing, the page opens in
// it instead. Agents in the other panes drive the same browser with
// `kb snapshot`, `kb click 3`, ... so the person watches what they do.
//
// Runs the `kb` program (props.kb, default "kb" on PATH). Reaches no network
// itself and reads no files.
import { View, div } from "gpui-kit";
import { h_flex, InputState, Input } from "gpui-base";
import { exec, run, theme, props, focused, notify } from "herdr";

export default class KbBrowser extends View {
  init(_props, cx) {
    this.url = "";
    this.state = null; // null = not asked yet, {ok, tabs, viewers} or {missing}
    this.busy = "";
    this.inputFocused = false;
    this.input = InputState.new({ value: "", placeholder: "URL, then Enter" });
    this.input.on("change", (_e, cx) => { this.url = this.input.value(); cx.notify(); });
    this.input.on("submit", (_e, cx) => this.open(cx));
    this.input.on("focus", (_e, cx) => { this.inputFocused = true; cx.notify(); });
    this.input.on("blur", (_e, cx) => { this.inputFocused = false; cx.notify(); });
    // `kb status` never starts a daemon, so asking is free of side effects.
    cx.timer.every(5000, (cx) => this.refresh(cx));
    this.refresh(cx);
  }

  kb() {
    return props()?.kb || "kb";
  }

  refresh(cx) {
    cx.spawn(async (cx) => {
      let next;
      try {
        const r = await exec(this.kb(), ["status", "--json"], { timeout: 4000 });
        if (r.code === 0) {
          const s = JSON.parse(r.stdout);
          next = { ok: true, tabs: s.tabs ?? 0, viewers: s.viewers ?? 0 };
        } else {
          next = { ok: false };
        }
      } catch (_e) {
        next = { missing: true };
      }
      if (JSON.stringify(next) !== JSON.stringify(this.state)) {
        this.state = next;
        cx.notify();
      }
    });
  }

  // Run one kb command, show what it said if it failed.
  act(cx, label, args, done) {
    if (this.busy) return;
    this.busy = label;
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const r = await exec(this.kb(), args, { cwd: focused()?.cwd ?? undefined, timeout: 60000 });
        if (r.code !== 0) notify({ title: `kb ${args[0]}`, body: (r.stderr || r.stdout).trim().split("\n")[0], level: "error" });
        else if (done) done(r);
      } catch (e) {
        notify({ title: "kb not found", body: `Could not run "${this.kb()}". Install kb and put it on PATH.`, level: "error" });
      }
      this.busy = "";
      this.refresh(cx);
      cx.notify();
    });
  }

  open(cx) {
    const url = (this.url || props()?.home || "about:blank").trim();
    if (this.state?.missing) return this.act(cx, "open", ["status"]);
    if (this.state?.viewers > 0) {
      // A viewer is on screen: load the page into the tab it shares with agents.
      this.act(cx, "open", ["open", url]);
    } else {
      // No viewer yet: put one beside the focused terminal.
      run("pane.split", { direction: "right", argv: [this.kb(), "view", url] });
      cx.timer.after(1500, (cx) => this.refresh(cx));
    }
  }

  render(cx) {
    const t = theme();
    const s = this.state;
    const running = Boolean(s?.ok);
    const button = (id, text, enabled, action) => h_flex().id(id).h(26).px(10).items_center().rounded(7)
      .border_1().border_color(t.border).bg(t.raised).text_size(12).font_medium()
      .text_color(enabled ? t.foreground : t.dim)
      .when(enabled, (el) => el.cursor_pointer().hover((st) => st.bg(t.hover)).on_click((_e, cx) => action(cx)))
      .child(this.busy === id ? "…" : text);

    const dot = s?.missing ? t.blocked : running ? t.working : t.dim;
    const status = s == null ? ""
      : s.missing ? "kb not installed"
      : !running ? "browser stopped"
      : `${s.tabs} ${s.tabs === 1 ? "tab" : "tabs"} · ${s.viewers > 0 ? "viewing" : "headless"}`;

    return h_flex().size_full().px(10).gap(8).items_center().bg(t.chrome).border_b(1).border_color(t.border)
      .child(div().text_size(12).font_semibold().text_color(t.accent).child("kb"))
      .child(Input.new(this.input).flex_1().h(26).px(9).rounded(7).border_1()
        .border_color(this.inputFocused ? t.accent : t.input).bg(t.background)
        .text_size(12.5).text_color(t.foreground))
      .child(button("open", s?.viewers > 0 ? "Go" : "Open", true, (cx) => this.open(cx)))
      .child(button("badges", "@N", running, (cx) => this.act(cx, "badges", ["badges", "toggle"])))
      .child(button("pins", "Pins", running, (cx) => this.act(cx, "pins", ["pins", "--save", "kb-pins.md"], (r) => {
        const n = Number((r.stdout.match(/saved (\d+) pins/) || [0, 0])[1]);
        notify({ title: `${n} ${n === 1 ? "pin" : "pins"} saved`, body: `kb-pins.md in ${focused()?.cwd ?? "the current folder"}`, level: "info" });
      })))
      .child(button("stop", "Stop", running, (cx) => this.act(cx, "stop", ["stop"])))
      .child(h_flex().gap(6).items_center().text_size(11.5).text_color(t.muted)
        .child(div().size(6).rounded(3).bg(dot))
        .child(status));
  }
}

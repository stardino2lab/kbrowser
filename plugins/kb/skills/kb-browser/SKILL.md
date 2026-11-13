---
name: kb-browser
description: >
  Open, read and operate real web pages from the shell with the `kb` CLI: a
  page snapshot with numbered element refs, then click / fill / press on those
  refs with real browser input. Use when a task needs a live page: checking a
  running web app or a deployed site, filling a form, following a multi-step
  flow, reading content that needs JavaScript, taking a screenshot, or reading
  notes the user pinned on page elements in `kb view`. Use it when the user
  mentions kb, `kb view`, pins, or `@N` refs. Not for plain HTTP fetches of
  static text or JSON, where curl or a fetch tool is simpler.
---

# kb: a browser you share with the user

`kb` drives a real Chrome through a background daemon. You use the CLI. The
user may be watching the same tabs live in `kb view` and can point at things
for you by pinning notes on elements.

## Before the first command

Run `kb status`. Exit code 0, or exit code 3 with `error[daemon]: daemon is
not running`, both mean kb is installed (the daemon starts by itself on the
first real command).
"command not found" means it is not installed: tell the user, and offer the
two ways to get it. Do not download or install it without their go-ahead.

- A build for Windows or Linux:
  https://github.com/stardino2lab/kbrowser/releases/latest (unpack, put `kb` on PATH)
- With Rust: `cargo install --git https://github.com/stardino2lab/kbrowser`

kb needs Chrome, Chromium or Edge on the machine.

## The loop

```
kb open https://example.com      # navigate, wait for load
kb snapshot -i                   # interactive elements, each with a ref
kb click 3                       # act on a ref
kb snapshot -i                   # look again
```

A snapshot looks like this:

```
[tab 1] Sign in - https://app.example.com/login
  @1 textbox "Email"
  @2 textbox "Password"
  @3 button "Log in"
```

- Write refs as plain numbers: `kb click 3`. (`@3` works too, but PowerShell
  swallows a bare `@3`.)
- A ref keeps its number for as long as its element exists. After a navigation
  or when the element is removed it is stale: the command fails with exit code
  2 instead of acting on something else. Take a new snapshot and use the new
  refs.
- Add `--snapshot` to `open`, `click`, `fill` or `press` to get the fresh
  snapshot in the same call. Prefer this over a separate `kb snapshot`.

## Reading

| Command | Use |
|---|---|
| `kb snapshot -i` | what can be acted on. Start here. |
| `kb snapshot` | the whole accessibility tree, headings and text included |
| `kb snapshot --scope 12` | only the subtree under ref 12 |
| `kb snapshot --max-nodes 300` | cap the output (default 2000; truncation is marked) |
| `kb text` / `kb text 12` | visible text of the page, or of one element |
| `kb eval "document.title"` | run JavaScript, print the result. `--stdin` for long scripts |
| `kb screenshot page.png` | PNG of the viewport, then read the file to look at it |

Keep output small: `-i` first, `--scope` once you know the region, `kb text`
for prose. Use `kb eval` for facts the tree does not show (a computed style, a
value in `localStorage`), not to click things.

## Acting

| Command | Notes |
|---|---|
| `kb click <ref>` | scrolls into view; fails (exit 5) if covered, hidden or disabled |
| `kb fill <ref> <text>` | text fields; `<select>` by value or label; checkbox with `true`/`false`; file inputs with a path. `--stdin` for long text |
| `kb press <key>` | `Enter`, `Tab`, `Escape`, `ArrowDown`, `a`, `Control+a` ... goes to the focused element |
| `kb wait <text\|ref>` | until text or an element appears. `--gone` waits for it to disappear. `--timeout <ms>` |
| `kb back` / `forward` / `reload` | history |
| `kb dialog accept [text]` / `kb dialog dismiss` | answer alert / confirm / prompt |

Input is real: the page receives trusted events, so controlled inputs,
validation and key handlers behave as for a person. Setting `.value` through
`kb eval` does not do that; use `fill`.

When a click reports `navigated=no` and the page was expected to change, the
change may be in-page: use `kb wait "some new text"` and then snapshot.

## Tabs

```
kb tabs                 # list, * marks the current tab
kb tab new [url]        # open and switch
kb tab 2                # switch
kb tab close [id]
kb click 3 --tab 2      # act on a tab without switching
```

Links that open a new tab are reported on the action's result line
(`new_tab=2`).

## Errors

Errors go to stderr as `error[code]: message` and usually a `hint:` line that
says what to do next. Follow the hint. With `--json` the same comes as JSON.

| Exit | Code | What to do |
|---|---|---|
| 1 | usage | fix the command; `kb help` |
| 2 | stale_ref | `kb snapshot` again, use the new ref |
| 3 | daemon / chrome | retry once; the daemon relaunches Chrome by itself |
| 4 | timeout | the page may still be usable: snapshot, or raise `--timeout` |
| 5 | not_actionable | something covers or disables the element: read the message, close the overlay or scroll, snapshot |
| 6 | no_such_tab | `kb tabs` |
| 7 | eval_error | your JavaScript threw; the message has the exception |
| 8 | dialog | a dialog is open: `kb dialog accept` or `dismiss`, then retry |

## Working with the user

The user can run `kb view` in a terminal that supports kitty graphics and see
the same tabs. Three things connect you:

- **Pins.** The user pins a note on an element; you read it with its ref,
  selector, role, text and position:

  ```
  $ kb pins
  # kb pins (1)

  ## #1 this should open in a new tab
  - page: Example Domain - https://example.com/
  - element: link "Learn more" (ref @1, tab 1)
  - selector: `html > body > a`
  ```

  When the user says "the thing I pinned", "see my notes" or "I marked it",
  run `kb pins` first. A pin whose page is not open is listed as detached;
  open its URL to get a live ref. `kb pins --json` gives the same as JSON.
  You can leave notes too: `kb pin add <ref> <note>`; remove with
  `kb pin rm <N>`.

- **Badges.** `kb badges on` draws every ref's `@N` on the page, so the user
  can say "click 7". They also appear in `kb screenshot`. `kb badges off`
  when done.

- **Same tabs.** What you open, the user sees; what they click, changes what
  you snapshot. If a ref goes stale for no reason of yours, the user probably
  navigated: snapshot again rather than assuming an error.

To let the user watch, suggest `kb view` in their own terminal. Do not start
it yourself: it needs an interactive terminal.

## Finishing

`kb stop` ends the daemon and Chrome. Leave it running while the user is in
`kb view` or more browser work is likely; stop it when the task is over and
nobody is watching. The browser keeps cookies in its own profile between runs,
separate from the user's everyday browser.

## Limits

- Content inside cross-origin iframes is not in the snapshot.
- Elements that are clickable only through a script handler (a `div` with
  `onclick` and no role) get no ref. Find them with `kb eval`, or click a
  labelled ancestor or descendant.
- The window is 1280x800 (the page area a little less) unless the viewer
  changed it. Check with `kb eval "innerWidth + 'x' + innerHeight"`.
- Output and error codes are always English, whatever language the user set
  for the interface.
- `kb help` is the full reference.

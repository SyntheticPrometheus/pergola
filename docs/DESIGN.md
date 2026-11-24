# Design

## Shape of the system

```
pergola (binary)                 daemon ── unix socket ── CLI ── hotkeys
   │                                │
   ├── pergola-x11   EWMH: client list, moves, grabs, overlay
   └── pergola-core  pure: tree, geometry, camera, commands, document
```

`pergola-core` has no I/O at all — no X11, no filesystem, no clock — so the
entire model is unit-testable and can be replayed headlessly (`pergola sim`).
`pergola-x11` owns the only unsafe-ish surface: an X connection in safe Rust via
`x11rb`. The binary is the daemon plus a thin client that forwards argv over a
Unix socket.

## Three decisions that matter

### 1. A layer, not a window manager

Pergola never selects `SubstructureRedirect` on the root window, so the running
desktop keeps the display. It reads `_NET_CLIENT_LIST` and asks the WM to move
windows with `_NET_MOVERESIZE_WINDOW` (gravity `NorthWest`, source `pager`) —
the same request `wmctrl` makes. Consequences:

* works inside xfwm4, mutter/X11, kwin_x11, i3, openbox, awesome, …;
* the WM keeps decorations, gravity, size hints and its own constraints;
* the frame rectangle is the unit of layout, so the code converts between frame
  and client coordinates using `_NET_FRAME_EXTENTS` at every move;
* a WM that ignores EWMH moves cannot be supported (`pergola doctor` says so).

Because it holds no redirect, the layer also cannot *see* a client's request to
activate a window by taking `SubstructureRedirect` — the desktop owns that. It
takes the shared `SubstructureNotify` bit on the root instead, which is enough:
EWMH clients send `_NET_ACTIVE_WINDOW` to the root with both masks, so a
taskbar's "click to focus" reaches the layer as well as the desktop. A request
for a hidden tab view switches the tab first; a request naming a window the
layer does not manage is ignored. The layer's own activation requests echo back
to it through the same selection, so they are tracked briefly and consumed
rather than re-handled, which would loop focus between the layer and the
desktop.

### 2. The camera is a transform, not a workspace switch

`framed` is a node id. Rendering the tree means:

```
rects = engine.rects(root, screen)          # world coordinates
if framed != root:                          # zoom
    for each rect: map (rect - framed) * (screen / framed.size)
```

The framed node therefore lands exactly on the screen and everything else moves
with it — a continuous zoom, matching the model. Windows outside the view are
moved off-screen (some WMs clamp them to keep a sliver reachable; documented).

### 3. The desktop is a space, not the screen

Exactly one `desktop` leaf exists in the tree. Floating windows store their
rects in *world* coordinates and are mapped into the desktop leaf's rectangle
whenever it is not the whole screen. Dragging a floating window is converted
back into world coordinates, so the layout learns the new position instead of
snapping the window back. When the desktop leaf moves (a dock, an arrange), the
windows are remapped with it.

The leaf is a floating layer, not a tile, and `inserting()` treats it that way:
a dock whose target is `desktop` retargets to the body beside it. Splitting the
leaf instead would divide its own share, and the default share is zero — the new
window would be zero pixels wide on the same axis, or take half the screen on
the other. Two engine tests pin both halves of that rule.

### 4. A tag is a set of views, not a second tree

A named workspace could own its own tree, which is what a full window manager
does. Here one tree stays the document of record and a tag is a *set of view
ids* over it: `active_tree()` prunes the panels that hold none of the active
tag's views, collapses a split left with one child, and keeps every surviving
node's id. That choice has consequences worth naming:

* A window keeps its panel, its split, its weights and its rectangle when its
tag is not shown, so switching back is exact and needs no saved layout — the
tile was never destroyed, only moved off-screen.
* Seats and the camera keep pointing at the same node ids, so `focus_seat`, a
  tab selection and a framed node survive a switch with no translation table.
* The pruning is pure and lives in `pergola-core`, so the rule is unit-tested
  without X: disjoint view sets, collapse of a one-child split, an empty tag
  showing only the desktop, and a document written with `tags` still parsing
  into a reader that only knows `root`.
* The cost is that `arrange` and `float-all` rebuild the shared tree and
  therefore act on every tag; the camera and the gap are global too. A per-tag
  tree would make those per-workspace at the price of a document model that can
describe the same window twice. See the limitations in the README.

A view on another tag is parked below the work area rather than unmapped: the
client list stays the desktop's, and `apply()` already parks windows outside a
zoomed view by the same rule.

### 4a. The workspace chords are a parameter, and the switch names itself

`binding_plan_with(overrides, tags)` takes the `tags` config key as an argument
rather than reading config inside the plan builder. The plan is also what
`pergola keys --check`, the settings rows and the palette display; a desktop that
never opted into workspaces must not see nine chords in a list it is supposed to
trust, and a test of the default plan must not depend on a file on disk. The
caller owns the decision, and `binding_plan()` stays the honest default.

A switch is announced by a desktop notification rather than a drawn indicator.
The x11 crate has no text drawing (bars and cursor fonts only), so the truthful
alternatives were a strip of unnamed rectangles or growing a text path for one
label. A notification carries the workspace's name and window count, disappears
by itself, and works with no compositor; a strip would still need the desktop's
notification or a status bar to say which rectangle is which. The overlay keeps
drawing geometry only, and `pergola subscribe` remains the interface for a bar
that wants a real workspace list.

## Data flow of one command

```
pergola dock left
  → CLI sends {"argv":["dock","left"]} to the socket
  → daemon parses, finds the focused window's seat, picks a target space
  → engine.inserting(panel, target, Left, root) -> new tree   (pure)
  → tree.validate()                                          (pure)
  → apply(): rects -> camera transform -> _NET_MOVERESIZE_WINDOW per window
  → save(): layout.json                                       (atomic-ish write)
  → CLI prints "docked p-… left of … (depth 2)"
```

`reconcile()` matches the tree against the live client list: seats whose window
is gone are removed (a split with one child collapses into that child), new
windows become floating panels or are auto-docked, and titles are refreshed. It
runs on demand and every two seconds in the event loop.

## Drawing the tree

The overlay is a *divider*, not a border: one line per split boundary, centred in the gap between
the two children, plus a 3 px amber frame around the work area when a node is framed. The line is
2 px when a compositor can dim the tiles behind it and 4 px when it cannot — with no dim layer the
line is the only thing that says "boundary", and 2 px in a 6 px gap all but disappears. Lines live
in the gap, so they never cover a title bar or a window border — the earlier design drew a border
around every tile and read as a debug artefact. It is off by default; `overlay on` (or `everything`)
shows it, and the lines are drawn as override-redirect windows raised above the tiles.

The dim layer is a separate click-through window whose opacity comes from the `dim` key
(`0.4` = 40% black by default, `0.9` at the darkest, `0` for no layer). It is only created when the
daemon sees a compositor owning `_NET_WM_CM_S0` *and* the SHAPE extension, since without SHAPE the
layer would swallow the pointer. `overlay on` reports which of the three outcomes happened
(`dim 40% 0x…`, `dim off (dim 0)`, `dim unavailable`), so a person can tell a missing cue from a
missing compositor.

## The human interface: pointer and palette

The engine has no idea whether a command came from a keyboard, a socket, or a
mouse. Two thin layers make that possible, and neither can change the tree on its
own:

### Keyboard

`binding_plan()` in `pergola-x11` maps chords to the same argv the CLI sends
(`"dock left"` is dispatched as `["dock", "left"]`), so a hotkey cannot bypass a
validation the CLI has. A chord the desktop already owns is skipped and the
`Super` alternative is tried; `pergola keys --check` reports the result.

### Pointer

* `LayoutEngine::gap_handles()` is the pointer-facing view of the tree: one hit
  rectangle per gap between adjacent, non-zero siblings, plus the split length a
  pixel delta is divided by. This is pure geometry and unit-tested with the
  engine.
* `X11::grab_bands_show()` turns those rectangles into *input-only*
  override-redirect windows — invisible, no pixels covered, each with the
  matching resize cursor. X's implicit pointer grab means a `ButtonPress` on the
  band keeps the `MotionNotify` stream even though the boundary moves under the
  cursor. The band set is rebuilt on every `apply()`, except during a drag, when
  replacing the window under the pointer would abort it.
* A drag re-derives the tree from the snapshot taken at `ButtonPress`, so the
  result is absolute and cannot accumulate rounding error.
* Dragging a *window* is not visible to an external process; the WM's synthetic
  `ConfigureNotify` (ICCCM, root coordinates) is. The daemon watches every
  managed window, filters its own moves (the rectangle it asked for, within a
  wake window), and waits for three geometry changes followed by 300 ms of quiet.
  A settle within 14 px of a work-area edge docks; anywhere else floats. This
  keeps an application opening at `0,51` from tiling itself.
* A docked tile that is dragged out of its own rectangle and settles near a
  *different* edge re-docks against that edge in one gesture: the same settle
  path calls `float_window` then `dock_window`, so the two-drag detour is gone.
  Requiring the midpoint to leave the old rectangle is what stops a full-width
  row (which touches both left and right) from reading as "already there".
* Two pointer hints answer "what will happen if I let go" without changing the
  tree: `Hint::Hover` draws a 3 px line down the long axis of the gap band under
  the pointer, and `Hint::Drop` draws a 4-bar frame around the rectangle
  `preview_dock()` computes for the edge the pointer is nearest. The preview
  runs the *same* three steps as `dock_window` — choose the target, take the
  seat out of the tree, insert it against the edge — so the frame cannot be
  drawn where the window will not land. Both hints are override-redirect
  windows with an empty SHAPE input region: a hint sits under the pointer by
  definition, so without pass-through the band would get a `LeaveNotify`, take
  the hint down and hand the pointer back in a storm. Without SHAPE the hints
  are skipped entirely and the bands still work. `settle_moves()` clears the
  drop hint (`hide_drop_hint`) but leaves the hover hint alone, because it runs
  on every loop pass and would otherwise erase the hover the band just set.

### Palette

`pergola palette` spawns a detached `palette-run` process (the daemon's event
loop and the CLI must both return immediately) which runs the desktop's menu
program and then calls `pergola <action>`, i.e. the same socket path a hotkey
uses. The action list is static and labelled for people, not for the grammar.

## Failure behaviour

| Situation | Behaviour |
|---|---|
| No `DISPLAY` / no EWMH | `pergola doctor` and the daemon fail with a one-line reason |
| WM ignores `_NET_MOVERESIZE_WINDOW` | window stays put; the CLI still reports success (the request was accepted) |
| Hotkey already owned by the desktop | try the `Super` fallback; report the ones with no free chord at startup |
| Malformed `layout.json` | start from a bare desktop, keep the file untouched |
| Window closes while docked | panel removed, split collapsed, layout re-applied |
| Two daemons | the second refuses: the socket accepts a connection, so it is a live daemon |
| A bar that stops reading | its queue is bounded and it is dropped after 10 s, so the event loop keeps running |
| Daemon restarts under a subscriber | the socket closes, the client exits 0 with a note, a supervisor restarts it |

## The event stream (`pergola subscribe`)

The socket is request/response for commands, and it becomes a push stream for a
status bar: a client sends `{"argv":["subscribe"]}` and the daemon keeps the
connection. The first record is a state snapshot — a bar that starts after the
session did would otherwise draw an empty tree until the next change (bspwm's
startup gap).

Events are *diffs of committed state*, not notifications fired from inside
mutators. The loop takes a signature (`EventSig`: the tree as JSON, each window's
placement and title, the active window) at the top of a pass and diffs it at the
bottom, so the several mutations of one command (detach, insert, apply) collapse
into one transition instead of a burst of phantom focus events (i3#3893's
failure) and no event is emitted from a half-applied tree. The signature is only
built when a subscriber exists, so the cost is zero for everyone else.

A slow bar must never stall the WM. That is why the writes are non-blocking and
per-client: records queue up to 256 per client, a `WouldBlock` keeps the queue
for the next pass, and `partial` remembers how many bytes of the front record
already went out — a plain `write_all` on a non-blocking socket can accept a
prefix and then fail, which would resend the record from the start and hand the
client a duplicated line. A client that has not drained anything for ten seconds
is dropped (i3's `kill_timeout`), so a hung bar cannot pin memory either.

## Testing

* `cargo test` — 131 unit tests over geometry, weights, docking, removal, gap
  clamping, ratios, neighbours, camera history, saved views, command URLs,
  document round-trips, named workspaces (disjoint view sets, pruning of the
  active tag's tree, an empty tag, a tag surviving its last window, a tagged
  document read by a tags-unaware reader), config parsing and repair of
  malformed weights, pointer-edge picking, hint geometry, activation-request
  targeting (a shown tab view, a hidden one, a floating window, an unmanaged
  id), the event stream's transition table (new window, dock, undock, close,
  layout, focus, tag, no-change), the notification invocation (default
  notifier, override, empty override), and the palette's state-aware rows and
  status/tree parsers.
* `scripts/e2e.sh` — starts Xvfb + a real WM (xfwm4 by default, openbox with
  `--wm openbox`) + three xterms, runs the real daemon, and asserts the geometry
  the X server reports against `pergola rects` after dock, zoom, arrange, gap
  changes, ratio, float, a daemon restart, keyboard docking, a gap drag, an edge
  drop, tab groups, config/binding errors, the overlay's dim layer, hover
  hints, the drop frame, one-gesture re-dock, the event stream, pager
  activation requests and named workspaces (disjoint tag sets, only the active
  tag laid out, a parked window mostly off the work area, the escape
  hatch, a restart and a `tag` record), settings help and a rejected value's
  notification, a ratio nudge chord, the workspace chords, a window on another
  host desktop left where the WM put it, and a failed hotkey's notification —
  219 assertions on xfwm4 (green) and 215–217 on openbox (green in 7 consecutive
  runs, 2026-11-24; `scripts/e2e-soak.sh --wm openbox` is the repetition the
  1.0.0 gate asks for, because the count moves with the tree rather than being a
  constant). Openbox runs the same suite and skips the drop-frame assertions
  because it coalesces a drag into one `ConfigureNotify` (a WM capability, not a
  tolerance), and its tree can hold a different number of tiles, so the geometry
  count is not identical. The event-stream block
  proves the state-first record, dock/undock/close records naming the window,
  valid JSON on every line, that a subscriber whose output goes to an undrained
  fifo does not delay `pergola status` (measured 47 ms at load 5.6), and that a
  client exits cleanly when the daemon stops.
  It never touches the live session.

  The suite uses fixed sleeps, so it assumes the machine is not CPU-saturated.
  Measured 2026-11-16 with a local model holding both cores (load 6): three
  checks flaked — an empty `P list` reply, an `arrange` tile left at 0,0, and a
  held chord that did not step back — and the same three flaked at the previous
  commit, so they are the suite's timing, not a product regression. Run the gate
  on an idle machine; a green run is 0 failed on both WMs.

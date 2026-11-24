# Compatibility

Pergola is a layer, not a window manager. It reads `_NET_CLIENT_LIST`, moves
windows with `_NET_MOVERESIZE_WINDOW` (gravity `NorthWest`, source `pager` — the
same request `wmctrl` makes), and draws its overlay and cues as
override-redirect windows. It never selects `SubstructureRedirect`, so the
running desktop keeps the display, the decorations, the size hints and its own
constraints. What follows is what has been measured, what is expected, and what
will not work — separately, because the three are not the same claim.

## Measured: tested and working

| Window manager | Session | Evidence | Notes |
|---|---|---|---|
| **xfwm4** (XFCE) | live, and Xvfb + xfwm4 in isolation | `scripts/e2e.sh`, 219 assertions, green | The default mode of the suite. Continuous `ConfigureNotify` during a drag, so the drop frame is asserted here. Compositor off in the suite; the overlay's dim layer is measured separately with compositing on |
| **Openbox** | live, and Xvfb + openbox in isolation | `scripts/e2e.sh --wm openbox`, 215–217 assertions, **green**: 0 failed in 7 consecutive runs (2026-11-24) | Skips the drop-frame assertions: it reports one `ConfigureNotify` per drag, so there is no mid-drag frame to assert. The drop itself is asserted. The assertion count is not 219 and not fixed: the two skipped drop-frame checks and a tree that holds a different number of tiles make it move with the tree, so read the summary line rather than comparing against a constant. Two Openbox habits read as user motion and are handled as the WM's placement: it reports the client at the position asked for, so the frame rect is 1 px off ours (`DECORATION_SLOP_PX`), and it pulls a parked window back so 44 px stay reachable (`parked_off_screen`). Both are pinned by unit tests in `crates/pergola/src/daemon.rs`; before that, every apply grew a phantom movement watch and a parked tile re-docked itself mid-suite — the churn behind the failures that used to move from run to run |

Both runs are the real daemon over a real X server with three real xterms, and
every assertion is made against geometry the server reports, not against the
daemon's own bookkeeping. Reproduction and the summary line (`e2e: N passed, M
failed (wm=…)`) are documented in [docs/DESIGN.md](DESIGN.md) and
[docs/SETUP.md](SETUP.md).

This is the whole of the measured surface. "Any EWMH window manager should work"
is a design intent, not a result.

## Expected, but untested

*Other EWMH X11 window managers* — `mutter` in an X11 session, `kwin_x11` under
X11: expected to work for the same reason xfwm4 and Openbox do (they honour
`_NET_MOVERESIZE_WINDOW`, keep their own decorations, and publish
`_NET_FRAME_EXTENTS`), but nobody has run the suite against them. Treat them as
untested, and read `pergola rects` against the screen before trusting a layout.

## Known incompatibilities: expected to fail or degrade, unmeasured

*Tiling window managers that lay themselves out* — i3, awesome, dwm and
relatives: expected to ignore or fight the move, or to undo it at the next
re-layout. Unmeasured; no claim either way beyond "do not expect the tiles to
stay where the tree says".

Why: a tiling window manager owns window geometry through the
`SubstructureRedirect` selection on the root window. Every map and configure
request from a client is redirected to it, and it decides the position and size.
An external `_NET_MOVERESIZE_WINDOW` asks the window manager to move a window,
and a tiling manager is entitled to answer "no" — or to accept it and then
re-tile everything on the next event. Pergola deliberately never selects
`SubstructureRedirect`, because taking it would displace the running desktop;
that is the price of being a layer. `pergola doctor` prints the WM name it
found, so at least the host is not a guess.

## Unsupported

* **Wayland-native sessions.** Wayland gives no protocol for one client to move
  another client's surfaces. There is no partial mode.
* **Non-EWMH window managers.** Without `_NET_CLIENT_LIST` there is no client
  list to adopt; `pergola doctor` fails with "no EWMH client list — this desktop
  does not advertise `_NET_CLIENT_LIST`".
* **Two daemons on one display.** One daemon owns the layout document and the
  window seats for a display; a second would fight it for the same windows.

## X11 session vs XWayland client

These are two different things and only the first is supported.

* An **X11 session** (Xorg, or Xvfb, or a nested X server): the window manager
  and Pergola are both X11 clients of the same server. The window manager owns
  geometry through `SubstructureRedirect`; Pergola asks it to move windows and it
  agrees. This is the measured configuration.
* A **Wayland session with XWayland**: X11 applications run against a nested X
  server, and Pergola is an X11 client, so it can see and manage *those* clients
  only. Their real geometry is owned by the Wayland compositor, which is free to
  ignore what XWayland is told; Pergola's moves can therefore be lost, and
  native Wayland windows are invisible to it entirely. "XWayland clients only"
  means exactly that, and no more.

## Per-host differences

**Key conflicts.** The default chords are a plan, not a promise: a desktop that
already owns a chord wins, and Pergola falls back to a secondary chord and says
so at startup. xfwm4 owns `Ctrl+Alt+arrows` and `Ctrl+Alt+D`, so the camera
chords land on `Super+…` there. Openbox binds fewer, so more of the `Ctrl+Alt`
plan survives. The truth for the live session is what was actually grabbed, so
read `pergola keys --check`: it asks the running daemon for the labels it bound
rather than echoing the plan. `pergola bind` and `pergola unbind` change one
chord; `bindings` in `~/.config/pergola/config.json` changes several.

**Dragging and visual feedback.** xfwm4 emits a stream of `ConfigureNotify`
events while a window is dragged, so the 70 px drop frame tracks the drag and
its rectangle is what the drop produces. Openbox coalesces the drag into a
single `ConfigureNotify` after release: no mid-drag frame, and the gesture is
recognised from that one event, which must move the window at least 40 px to
count as a drag. A desktop that sends no configure events at all cannot be
docked by mouse — use the keyboard chords, which do not depend on the desktop's
event stream.

**Parked-window clamping.** A window outside the framed view is moved below the
work area, not unmapped; desktops clamp off-screen windows so they stay
reachable. On a 900 px screen xfwm4 keeps the 29 px title bar visible at the
bottom edge; Openbox keeps 66 px of an 862 px client. A larger park offset does
not help — `xdotool windowmove` to y=900, 1200 or 3000 lands at the same clamped
y on the hosts measured. So a parked tile peeks at the edge, by an amount that
is the desktop's choice, not a Pergola setting.

**Work area.** The layout runs inside `_NET_WORKAREA`, so panels shrink the
usable rectangle and the tile coordinates are not screen coordinates when a panel
is present. xfwm4's own title bar and borders are reclaimed from
`_NET_FRAME_EXTENTS` at every move.

## Finding out where you are

* `pergola doctor` — display, screen size, `_NET_CLIENT_LIST` count, the running
  window manager's name, and whether SHAPE is present (without it the pointer
  cues are not drawn).
* `pergola keys --check` — the chords actually bound in this session.
* `pergola status` — what the daemon thinks: screen, panels, depth, gap.
* `pergola rects` — the rectangle each tile was given, to compare with the screen
  when a desktop is suspected of ignoring a move.

## What this document does not claim

Only the two rows in the measured table are test results. Everything else on
this page is either a design consequence of EWMH (expected) or a negative
(unsupported). If a desktop is not in the table, the honest answer is "not
measured", and the way to change that is a green `scripts/e2e.sh --wm <name>`
run recorded with its summary line, not an entry added here.

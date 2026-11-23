# Pergola

A **fractal window manager layer for X11 desktops** — windows nest inside windows
as deep as you like, and a camera lets you zoom into the part you need.

Pergola is a Rust reimplementation of the model popularised by
[TrellisWM](https://trelliswm.app) for macOS, rebuilt as a Linux window-manager
*layer* rather than a replacement: it never takes over the display, so it runs
inside xfwm4, mutter (X11), kwin_x11, i3, openbox, awesome, or any other
EWMH-compliant window manager.

**The name:** the model came from TrellisWM — a trellis is the lattice, and a
pergola is what you build out of one: an open structure overhead that splits the
sky into nested frames you walk under. That is what a nested desktop looks like
from inside — a lattice you see through, not a grid you sit in.

![Pergola: one tile on the left, a nested column of three on the right, and the desktop space](docs/screenshot.png)

```text
 root ── split(x) ─┬─ p-editor ────────────────────────┐
                   ├─ split(y) ─┬─ p-browser ──────────┤
                   │            └─ p-terminal ─────────┤
                   └─ desktop (floating windows) ──────┘
```

## Documentation

New to Pergola? Read in this order:

1. **[docs/FIRST-RUN.md](docs/FIRST-RUN.md)** — the five-minute walkthrough:
   three terminals, three docks, and the glossary every `pergola status` line uses.
2. **[docs/USER-GUIDE.md](docs/USER-GUIDE.md)** — the illustrated tour: what the
   nesting model is, how the camera moves over it, and every operation in
   keyboard, mouse and command form.
3. **[docs/KEYBINDINGS.md](docs/KEYBINDINGS.md)** — the default plan, why a
   chord may fall back from `Ctrl+Alt` to `Super`, how to remap one, and how to
   read `pergola keys --check` (the chords this session actually owns).
4. **Install** and **Quick start** below, then **Hotkeys**, **Mouse** and
   **Configuration** in this file, and the task-oriented guides in
   **[docs/tutorials/](docs/tutorials/README.md)**.

Reference and background:

* **Mouse** — [docs/MOUSE.md](docs/MOUSE.md): every gesture, the exact 6/14/70 px
  geometry, nested-layout targeting, and what the pointer does *not* do.
* **Configuration** — [docs/CONFIGURATION.md](docs/CONFIGURATION.md): every key
  with type, default, range and reload behaviour, plus complete example profiles.
* **Commands** — [docs/COMMANDS.md](docs/COMMANDS.md): syntax, arguments,
  scope, undoability and errors for every command, or `pergola help`.
* **Compatibility** — [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md): what is
  measured (xfwm4, Openbox), what is expected but unmeasured, and why tiling
  window managers may ignore Pergola's moves.
* **Install, upgrade, uninstall, configuration file, troubleshooting, host
  window managers** — [docs/SETUP.md](docs/SETUP.md).
* **Design and provenance** — [docs/DESIGN.md](docs/DESIGN.md),
  [docs/RESEARCH.md](docs/RESEARCH.md),
  [docs/HISTORY-LESSONS.md](docs/HISTORY-LESSONS.md), and
  [docs/UX-NOTES.md](docs/UX-NOTES.md).
* **Developing and releasing** — [Development](#development),
  [docs/VERSIONING.md](docs/VERSIONING.md), [BACKLOG.md](BACKLOG.md),
  [CHANGELOG.md](CHANGELOG.md).
* **Packaging** — [packaging/aur/README.md](packaging/aur/README.md).

The Markdown is checked: `scripts/doc-check.py` verifies links, JSON examples,
command names and that every documentation table lists exactly the configuration
keys the parser knows. Run it after editing a page; it exits non-zero on drift.

## What this is (and is not)

* **Is**: a real, installed, tested window manager *layer*. It docks, nests,
  resizes, zooms, saves views and restores layouts on a running X11 desktop.
* **Is not**: a fork, port or derivative of TrellisWM's code. TrellisWM is
  closed-source; Pergola is a clean-room implementation of the *observable
  model* (recursive split tree + camera), derived from public documentation and
  a public clean-room study. See [docs/RESEARCH.md](docs/RESEARCH.md) and
  [NOTICE](NOTICE) for provenance.
* **Is not**: a Wayland compositor. Wayland clients cannot be moved by an
  external process; Pergola needs X11 (or XWayland, where it can manage X11
  clients only).

## Install

```bash
git clone https://github.com/SyntheticPrometheus/pergola
cd pergola
./install.sh            # build, install to ~/.local/bin, autostart, start now
```

Requirements: Rust (1.75+), an X11 session, `rofi` (or `zenity`/`yad`/`dmenu`) only
for the action palette, `xdotool`/`wmctrl` only for the optional test script. No root, no daemon privileges, no Accessibility-style
permission prompt. `install.sh` installs `pergola` and `pergola-health` into
`~/.local/bin`, writes `~/.config/pergola/config.json` (only if absent), and
adds a systemd user unit plus an XDG autostart entry so the daemon starts with
your session. It also adds **Pergola Actions** to the desktop's application menu
as a visible way to open the action palette. `./install.sh --no-start` skips the start.

On Arch there is also a `PKGBUILD` in
[`packaging/aur/`](packaging/aur/README.md) (`pergola-git`) that installs to
system paths — `makepkg -si` from that directory. It is not published to the AUR
yet. Do not use both: the source install's unit and autostart entry shadow the
packaged ones.

Prebuilt tarballs and a generated `pergola-bin` AUR package are attached to
**stable** releases by `.github/workflows/release.yml`. There is no release yet:
per the versioning rules, dev builds are commits and only stable versions are
tagged, so the first release comes with 1.0.0.

```bash
pergola doctor          # display, EWMH client list, running WM
pergola status          # the daemon's view: screen, panels, depth, gap, overlay
pergola keys --check    # which chords are free on this desktop
pergola-health          # soak check; exits non-zero on any failure
```

New here? **[docs/FIRST-RUN.md](docs/FIRST-RUN.md)** is the five-minute
walkthrough — three terminals, three docks, and a glossary for every term the
`status` line prints. Full install, upgrade, uninstall and troubleshooting
reference: **[docs/SETUP.md](docs/SETUP.md)**.

## Quick start

```bash
pergola dock left        # dock the focused window to the left of its space
pergola dock right       # dock the next window beside it
pergola dock down        # nesting: a split inside a split
pergola frame-window     # frame the focused window (again to go back)
pergola zoom-out         # step one level out
pergola arrange          # tile every window into a balanced fractal tree
pergola float-all        # give every window back to the desktop layer
pergola tree             # the layout document, as JSON
pergola rects            # every panel's current frame rectangle
```

## Commands

| Command | Meaning |
|---|---|
| `everything` | show the whole tree (zoom all the way out) |
| `zoom-in` / `zoom-out` | step one level in / out |
| `back` / `forward` | browser-style camera history |
| `frame-window` / `frame-desktop` | frame the focused window / the desktop layer |
| `toggle-desktop` | hide or show floating windows |
| `neighbour <left\|right\|up\|down>` | move to the neighbouring space |
| `view <1-9>` / `save-view <1-9>` | saved views |
| `dock <edge> [--window ID] [--target ID]` | dock a window against another space |
| `float [--window ID]` | return a window to the desktop layer |
| `arrange` / `float-all` | tile everything / float everything |
| `split-terminal` | split the focused window with a new terminal |
| `gap <px>` | gap between tiles |
| `ratio <0.0-1.0>` / `ratio +0.05` | set the focused panel's share of its split, or nudge it |
| `palette` / `palette --list` | a menu of the tile actions for the focused window (rofi/zenity/dmenu); `--list` prints the rows |
| `overlay [on\|off\|toggle]` | draw split lines in the gaps, and dim the tiles when a compositor can render it (off by default) |
| `bind <command> [chord…]` / `unbind <command>` | change a chord without editing JSON |
| `tab` / `tab-select` / `tab-next` / `tab-prev` | fold a window into a panel as a tab, switch tabs |
| `undo` | put the layout back before the last dock, float, arrange, gap change or ratio |
| `tag-view <name>` / `window-tag <name>` / `tag-remove <name>` | switch a named workspace, move a window to one, delete one |
| `config [--check]` | the resolved config with each key's source, or what is wrong with the file |
| `settings` / `settings --list` / `settings <key> <value>` | change a setting from a menu whose rows explain themselves, list the rows, or set one key (a rejected value also raises a desktop notification) |
| `subscribe` | stream layout, focus, dock, undock and window-closed records as JSON lines (for a status bar) |
| `status` `tree` `list` `rects` `keys` `doctor` `sim` | inspection |
| `reload` `save` `stop` | daemon control |

Every command also has a URL form: `pergola://zoom-in`,
`pergola://neighbour?dir=left`, `pergola://view?n=3`.

### Named workspaces (tags)

Views are anonymous by default: everything is on one screen. A *tag* names a
set of views, and switching to it lays out only those windows — the others keep
their place in the tree and their rectangles, so switching back is exact.

```sh
pergola window-tag work     # move the focused window to a workspace (creates it)
pergola window-tag work --window 0x40000c
pergola tag-view work       # show it; tag-view all shows every view again
pergola tag-view            # list the workspaces, * on the active one
pergola tag-remove work     # delete a workspace; its windows stay open
```

A view belongs to at most one tag. A window that opens while a tag is shown
joins that tag; a window whose tag is not shown is moved below the work area
(parked), never closed. Each tag remembers the window that had the keyboard, so
returning to it puts you back where you were. The active tag is stored in the
layout document and survives a restart.

Workspaces are one key away if you want them: `{"tags": true}` in the config
binds `Super+1`…`9` to `tag-view 1`…`9` and `Super+Shift+1`…`9` to
`window-tag 1`…`9` (with `Ctrl+Alt` as the first choice, like every other
command). They are opt-in because pressing `Super+1` on a desktop that never
named a workspace would blank the screen, and a nine-chord surprise is not a
feature. A switch sends a desktop notification naming the workspace, so the
screen change explains itself; the palette lists a row per workspace the
document actually holds. The event stream carries a `{"type":"tag","name":…}`
record and the state snapshot has `tag` + `tags`, so a status bar can draw a
workspace switcher. Any chord can still be replaced: `pergola bind "tag-view 1"
Super+F1`.

### Status bars: `pergola subscribe`

A panel should not have to poll `pergola tree` and re-implement the diff. The
daemon pushes newline-delimited JSON instead, one object per line:

```bash
pergola subscribe
{"type":"state","tree":{…},"gap":6.0,"overlay":false,"active":10485772}
{"type":"dock","window":2097164,"title":"xterm","panel":"p-20000c"}
{"type":"layout","tree":{…}}
{"type":"focus","window":10485772,"title":""}
{"type":"undock","window":2097164,"title":"xterm","panel":"p-20000c"}
{"type":"window-closed","window":2097164,"title":"xterm"}
```

The first line is a state snapshot, so a client that starts after the session
did is not blind. After that the daemon diffs committed state between event-loop
passes: `dock` and `undock` are a window entering or leaving the tree,
`window-opened`/`window-closed` are appearance and disappearance, `layout` is
any other change to the tree (with the new tree), and `focus` is a change of the
active window — so a move that emits several intermediate mutations still
produces one record per transition, not a burst of phantom focus events.

The stream is non-blocking and per-client bounded: a bar that stops reading
never stalls the window manager, and it is dropped after ten seconds without
draining anything rather than pinning memory. When the daemon exits, the socket
closes: the client prints a line to stderr and exits 0, so a supervisor can
restart it.

## Hotkeys

Primary bindings mirror TrellisWM's `⌃⌥` shortcuts; when the desktop already
owns a chord (xfwm4 owns `Ctrl+Alt+arrows` and `Ctrl+Alt+D`), Pergola falls back
to `Super` for that command and reports it at startup.

| Keys | Command |
|---|---|
| `Ctrl+Alt+Space` | everything |
| `Ctrl+Alt+↑` / `↓` | zoom-out / zoom-in |
| `Ctrl+Alt+←` / `→` | neighbour left / right |
| `Ctrl+Alt+[` / `]` | back / forward |
| `Ctrl+Alt+Return` | frame-window |
| `Ctrl+Alt+Shift+←` / `→` | **dock the focused window left / right** |
| `Ctrl+Alt+Shift+↑` / `↓` | **dock it above / below** |
| `Ctrl+Alt+Shift+W` | **float the focused window** |
| `Ctrl+Alt+M` | **action palette** (mouse-friendly menu) |
| `Ctrl+Alt+D` | toggle-desktop |
| `Ctrl+Alt+N` | split-terminal |
| `Ctrl+Alt+A` | arrange |
| `Ctrl+Alt+F` | float-all |
| `Ctrl+Alt+O` | overlay |
| `Ctrl+Alt+Z` | **undo the last layout change** |
| `Ctrl+Alt+Page_Down` / `Page_Up` | next / previous tab in a group |
| `Ctrl+Alt+,` / `Ctrl+Alt+.` | narrow / widen the split around the focused window |

**You can tile with three keys and no terminal:** focus a window, `Ctrl+Alt+Shift+←`,
focus the next one, `Ctrl+Alt+Shift+→`. That is two tiles side by side.

`Super+…` is the automatic fallback for any chord the desktop already uses (xfwm4
owns `Ctrl+Alt+arrows`, so the camera keys fall back to `Super+arrows` there; the
daemon says so at startup). `pergola keys --check` prints the chord actually bound
for each command, so it never shows a chord the desktop intercepts; the palette
(`Ctrl+Alt+M`) lists the same actions by name.

## Mouse

The drag actions need no command line:

* **Hover a gap** — the band you are over lights up (a 3 px line down its long
  axis), so a 6 px strip is findable without hunting with the cursor. The hint
  is click-through: it never takes the pointer away from the band.
* **Drag a split boundary** — the gap between two tiles is a 6 px grab strip with
  a resize cursor. Drag it to change the split's weights (both neighbours move,
  never below `minimum_share`). It works while zoomed, and the pointer keeps the
  grab even though the boundary moves under it.
* **Drag a window to a screen edge** — let go within 14 px of the left, right, top
  or bottom of the work area and it docks to that edge. While the pointer is
  within 70 px of an edge, a 3 px frame shows the rectangle the window will get,
  so the landing spot is visible before you let go. Drop a second window on the
  opposite edge and you have two tiles.
* **Drag a docked tile to another edge** — a tile that leaves its own rectangle
  and settles near a different edge re-docks there in one gesture, no float step
  in between.
* **Drag a tile away from the edge** — it floats again where you dropped it, so
  the mouse can undo a tile without a command.
* **`Ctrl+Alt+M` or `pergola palette`** — a menu of the actions with human names
  ("Tile this window right", "Arrange all windows"); it needs `rofi` (recommended)
  or `zenity`/`yad`/`dmenu`, and is mouse-clickable as well as type-to-filter.
  Without a menu program the hotkey fails into the daemon log; run
  `pergola palette` in a terminal to see the error, or set `PERGOLA_MENU`.

Docking is resolved against the last focused space, so in a nested layout a drop
joins the tile you were last working in. `edge_dock: false` in the config turns
the edge behaviour off if a drag should never tile.

## Configuration

`~/.config/pergola/config.json` (all keys optional):

| Key | Default | Effect |
|---|---|---|
| `gap` | `6.0` | pixels between tiles and width of the overlay divider |
| `minimum_share` | `0.05` | smallest fraction a tile can be squeezed to |
| `auto_dock` | `"off"` | `off`, `new` or `all`: whether a new window is docked (`new` after startup, `all` including the session it found); the old `true`/`false` still parses |
| `overlay` | `false` | draw the split lines and viewport frame, plus a dim layer under them when a compositor is running |
| `dim` | `0.4` | strength of the overlay's dim layer, `0` (off) to `0.9`; only used with a compositor. At `0` the split lines are drawn 4 px wide instead of 2, so the boundary still reads without a dim layer |
| `tags` | `false` | bind `Super+1`…`9` (and `Ctrl+Alt+1`…`9`) to the nine named workspaces, plus `Super+Shift+n` to move the focused window onto one. `on`/`off` |
| `desktop_share` | `0.0` | desktop strip `arrange` keeps; `0` gives it none |
| `terminal` | `$TERMINAL`, else an installed terminal (`xfce4-terminal` first, `xterm` last) | program used by `split-terminal` |
| `hotkeys` | `true` | grab the default chords on the root window |
| `edge_dock` | `true` | dock a window dropped on a screen edge (mouse tiling) |
| `notify` | `true` | show a desktop notification when a hotkey or palette action fails; best effort, and the failure still reaches the journal |
| `bindings` | `{}` | per-command chord overrides, `command → [chord, …]` |

`pergola reload` re-reads the file and re-grabs the hotkeys, so a `bindings`
edit takes effect without a restart. An invalid config leaves the running
settings and keys in place and reports the error. `overlay` still applies on daemon restart.
The `gap` and `overlay` commands change the running session only. See
[docs/SETUP.md](docs/SETUP.md#configuration) for the details.

`pergola settings` opens a menu of every key and every command's chords, and
writes your choice back through the same validation — `pergola settings gap 12`
does it from a script, and `pergola settings --list` prints the rows. `pergola
config` prints every key with the value actually in force and where it came
from (`default` or `config.json:<line>`), so a typo cannot hide behind a
default. `pergola config --check` exits non-zero on an unknown key, an
out-of-range gap/share, a terminal that does not exist or a chord that does not
parse, and names the line — a misspelled key is otherwise ignored silently.

### Remap a key

One command, no JSON editing:

```sh
pergola bind "gap 12" Super+9   # bind a command to a chord and reload
pergola bind "gap 12"           # show what it is bound to and where it comes from
pergola bind "gap 12" --default # drop the override, back to the default chord
pergola unbind "gap 12"         # remove the chord entirely
```

These write `config.json` for you (preserving every other setting), reload the
daemon, and print any conflict the daemon reported — a chord the desktop owns,
for example — so you see it where you ran the command. A daemon that is not
running is not an error: the change is saved and applies at the next start.

For several changes at once, edit the `bindings` map directly. It maps a command
to the chords to try, in order. Naming a command
replaces its default chords entirely; an empty list unbinds it; commands you do
not mention keep their defaults. Any command the daemon understands can be
bound, not only the ones in the default plan:

```json
{
  "bindings": {
    "zoom-out": ["Super+Page_Up"],
    "back": ["Super+b", "Alt+b"],
    "view 3": ["Super+3"],
    "overlay": []
  }
}
```

After saving, run `pergola reload` and check the result with
`pergola keys --check`: it prints the chord the daemon actually grabbed, or
`off` for a command you unbound deliberately. `pergola bind <command>` with no
chord shows the same answer for one command.

## How it works

The window tree is the whole model (every term in this section is defined in
the [first-run glossary](docs/FIRST-RUN.md#glossary)):

```text
Node    := Desktop | Panel | Split
Desktop := { kind:"desktop", id:"desktop" }            # the floating layer
Panel   := { kind:"panel", id, views:[SeatID], selected, tabbed? }
Split   := { kind:"split", id, axis:"x"|"y", children:[Node], weights:[f64] }
```

* A `Split` may contain `Split`s. That recursion is the fractal part.
* Exactly one `Desktop` leaf exists; it is a *space*, not the screen, and
  floating windows live inside whatever rectangle it currently occupies.
* The camera is just a framed node id plus back/forward stacks and nine saved
  views. Framing a node maps it onto the screen and everything else moves with
  it — a real, continuous zoom, not a workspace switch.
* Geometry is applied through `_NET_MOVERESIZE_WINDOW`, the same EWMH request
  `wmctrl` uses, so the running WM keeps its decorations, gravity and
  constraints intact.
* The tree, camera and window fingerprints persist to
  `${XDG_STATE_HOME:-~/.local/state}/pergola/layout.json` (document `version: 2`,
  shape-compatible with the layout files described in the research note).

Architecture: `pergola-core` (pure engine, no X11 — tree, geometry, camera,
commands, document), `pergola-x11` (EWMH enumeration, moves, hotkey grabs,
overlay), `pergola` (daemon + CLI + Unix-socket IPC). See
[docs/DESIGN.md](docs/DESIGN.md).

Why each edge case is handled the way it is: [docs/HISTORY-LESSONS.md](docs/HISTORY-LESSONS.md)
classifies 700 issues filed against the seven tiling window managers that came
before Pergola and maps every failure class to the fix that answers it.

## Known limitations

* **X11 only.** No Wayland-native support; XWayland clients only.
* Verified inside **xfwm4** (XFCE) and **Openbox**, live and in isolation; any
  EWMH window manager should work, but only those two have been measured.
* Windows outside the framed view are moved off-screen. Some WMs (xfwm4) clamp
  them to keep a sliver reachable, so a docked tile can peek at the screen edge.
* Windows that set size hints (terminals with a character grid) are fitted to
  their own resize increment before the frame is placed, so the tile covers its
  slot without the WM clamping it short. A tile is therefore at most one
  character cell narrower than the slot, and `pergola rects` reports the slot.
* Dialogs and transient windows (`WM_TRANSIENT_FOR`) are not tiled: they keep the
  geometry their toolkit gave them, and a dock request for one fails with
  "not managed by pergola" rather than placing a dialog in a tile.
* Tab groups work from the command line and the default chords (`tab-next`,
  `tab-prev`); a floating window can be folded in with `pergola tab`. There is
  no drag-to-tab UI and no tab bar: the shown view is the one you interact with,
  and the hidden ones are only reachable by command.
* A split's ratio can be changed from the keyboard with `pergola ratio 0.3`,
  or by dragging its gap. With `gap: 0` there is no strip to grab, so the
  command is the only way there. The palette also has half/third ratio rows.
* The palette offers actions for the focused window, followed by layout actions.
  It hides "Float this window" when the focused window is already floating.
* Edge docking targets the last focused space; in a deep tree the landing spot may
  be nested rather than at the root, which reads as "it went where I was working".
  The drop frame shows that rectangle while you drag, so aim at the edge you want
  rather than at an empty region of the screen.
* The overlay is drawn with 2–3 px override-redirect rectangles inside the gaps
  and around the viewport. When a compositor owns `_NET_WM_CM_S0` it also draws a
  full-screen 40% black layer under those lines, so the tree reads at a glance;
  without a compositor, or without the SHAPE extension that makes the layer
  click-through, the overlay is borders only.
* The pointer hints (hovered gap, drop frame) are override-redirect windows with
  an empty SHAPE input region, so they never swallow a click. Without the SHAPE
  extension the hints are not drawn at all — the bands still work, the cue is
  simply missing.
* `pergola subscribe` is a stream, not a poll: the daemon pushes one record per
  committed transition (plus a state snapshot on connect). A subscriber that
  stops reading is dropped after ten seconds; when the daemon exits the client
  exits 0 so a supervisor can restart it. There is no per-event subscription
  filter yet — a client that wants only `focus` reads and discards the rest.
* A taskbar's click-to-focus works: the daemon reads `_NET_ACTIVE_WINDOW`
  requests from the root's shared `SubstructureNotify` selection, focuses the
  named window and shows a hidden tab view first. A request naming a window it
  does not manage is ignored. It does not raise a floating window over a docked
  one, and it does not act on the source indication (pager vs application):
  both are treated as "focus this".
* Only one instance per display (one Unix socket).
* A window on a workspace that is not shown is *parked* below the work area, not
  unmapped. The desktop clamps a window it must not lose, so a sliver stays at
  the bottom edge — measured on a 900 px-tall screen: xfwm4 leaves the 29 px
  title bar, openbox leaves 66 px of an 862 px client. Unmapping (or an ICCCM
  iconify) would be the fully clean answer, but the daemon must not fight the
  desktop's own client list to get it.
* Tags are sets of views over one tree, not one tree each. `arrange` and
  `float-all` act on the whole document (every tag), because they rebuild the
  shared tree; `dock`, `float`, `tab` and the commands that name one window act
  on that window's tag. The camera and the gap are global, so switching tags
  keeps the zoom instead of remembering one per workspace.

## Development

```bash
cargo test               # unit tests: geometry, tree, camera, commands, document, tags, settings, palette, event diff
scripts/e2e.sh           # assertions against Xvfb + a real WM + real xterms
scripts/e2e.sh --wm openbox   # the same suite under a second WM
scripts/health.sh        # soak check of a running daemon (cpu, layout, drift, orphans)
scripts/health.sh --e2e  # the above plus the regression suite (xfwm4)
scripts/health.sh --e2e --wm openbox   # ... under a second WM; the line names it
cargo build --release
```

`install.sh` also installs `pergola-health`, the same soak check under a stable name, so a
scheduler or a timer can run it unattended and act on the exit code. `BACKLOG.md` is the
development queue: every item names what is done when. Human-interface pain points — with a
severity and a proposed fix each — are recorded in [docs/UX-NOTES.md](docs/UX-NOTES.md).

The end-to-end script starts its own X server, window manager and windows, so it
does not touch your session. It asserts geometry as the X server reports it, not
just the daemon's own bookkeeping.

Versioning: [docs/VERSIONING.md](docs/VERSIONING.md). Current version is
`0.5.0-dev` — pre-stable, no compatibility promises.

## Support

Pergola is MIT-licensed, has no paid tier and no feature behind a payment. If it
saves you time, **[docs/FUNDING.md](docs/FUNDING.md)** names a USDC/Base address;
GitHub's Sponsor button points at the same page. Nothing is gated on it.

## License

MIT © SyntheticPrometheus. "TrellisWM" is a trademark of its owner and is used
here only to describe the model Pergola reimplements. No code from TrellisWM or
from the clean-room study is included. See [NOTICE](NOTICE).

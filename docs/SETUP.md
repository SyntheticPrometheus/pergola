# Setup

Everything Pergola installs is user-local. It never runs as root, never becomes
a system service, and never replaces the desktop's window manager — it attaches
to the running X11 session and asks the WM to move windows.

For what the pieces are and why, read [DESIGN.md](DESIGN.md); this page is the
operational side: install, configure, upgrade, remove, diagnose.

## Requirements

| Need | Why |
|---|---|
| Linux with an **X11** session | Wayland clients cannot be moved by an external process |
| An **EWMH-compliant window manager** | Pergola reads `_NET_CLIENT_LIST` and moves windows with `_NET_MOVERESIZE_WINDOW`; measured on **Xfwm4** and **Openbox** |
| **Rust 1.75+** | only to build; the installed binary links nothing but `libc` and `libgcc_s` |
| An XDG autostart directory **or** a systemd user manager | to start the daemon at login (both are written by `install.sh`) |
| `rofi` (recommended), or `zenity`/`yad`/`dmenu` | only for the action palette (`Ctrl+Alt+M`) |
| `Xvfb`, `xfwm4`, `xterm`, `xdotool`, `wmctrl` | optional, only for `scripts/e2e.sh` |

`pergola doctor` is the authoritative check for the first two: it prints the
display, the screen, the EWMH client list and the running WM, or the one reason
it cannot proceed.

## Install

```bash
git clone https://github.com/SyntheticPrometheus/pergola
cd pergola
./install.sh
```

`install.sh` does, in order:

1. `cargo build --release`.
2. Installs `pergola` and `pergola-health` into `~/.local/bin` (override with
   `PREFIX=/usr/local` or `BINDIR=/some/bin`).
3. Writes `~/.config/pergola/config.json` **only if it does not exist** — an
   upgrade never overwrites your settings.
4. Renders `systemd/pergola.service` with the real binary path and the display
   the install ran on, writes it to `~/.config/systemd/user/`, then
   `systemctl --user enable pergola.service`.
5. Renders `autostart/pergola.desktop` and a separate **Pergola Actions** entry
   for the desktop's application menu. The latter opens the action palette.
6. Starts the daemon and prints `pergola status` — unless `--no-start` is given
   or a daemon already answers on the socket.

Options: `./install.sh --no-start`, `PREFIX=…`, `BINDIR=…`.

### Arch Linux: the packaged install

`packaging/aur/PKGBUILD` (`pergola-git`) builds the same code and installs it to
system paths: `/usr/bin/pergola`, `/usr/bin/pergola-health`,
`/usr/lib/systemd/user/pergola.service`, the XDG autostart entry and the docs.
It is not published to the AUR yet; build it from the checkout:

```bash
cd packaging/aur
makepkg -si
systemctl --user enable --now pergola.service
```

Two differences from `install.sh`: the config ships as an example in
`/usr/share/doc/pergola-git/config.json` (pacman does not write into `$HOME`;
copy it when you want to change a default), and the packaged unit inherits
`DISPLAY` from the graphical session instead of baking in the display the
install ran on. **Use one or the other**, not both — the source install's unit
and autostart entry shadow the packaged ones. Details and the reason the package
is named `pergola-git` rather than `pergola-bin`: [packaging/aur/README.md](../packaging/aur/README.md).

### What lands where

| File | Purpose |
|---|---|
| `~/.local/bin/pergola` | the daemon and the CLI (one binary; `pergola daemon` is the daemon) |
| `~/.local/bin/pergola-health` | `scripts/health.sh` under a stable name, for timers and cron |
| `${XDG_CONFIG_HOME:-~/.config}/pergola/config.json` | user settings (below) |
| `${XDG_STATE_HOME:-~/.local/state}/pergola/layout.json` | the saved tree, camera and window fingerprints |
| `${XDG_STATE_HOME:-~/.local/state}/pergola/health.jsonl` | one JSON line per `pergola-health` run |
| `${XDG_RUNTIME_DIR:-<state dir>}/pergola.sock` | the Unix socket the CLI talks to |
| `~/.config/systemd/user/pergola.service` | start at login on systemd desktops |
| `~/.config/autostart/pergola.desktop` | start at login on non-systemd desktops |
| `${XDG_DATA_HOME:-~/.local/share}/applications/pergola-actions.desktop` | open the action palette from the application menu |

### Install by hand

`install.sh` is a convenience, not magic. The minimum is:

```bash
cargo build --release
install -Dm755 target/release/pergola ~/.local/bin/pergola
install -Dm755 scripts/health.sh ~/.local/bin/pergola-health
mkdir -p ~/.config/pergola && $EDITOR ~/.config/pergola/config.json
pergola daemon &          # or start it from your session's autostart
```

## Start at login

`install.sh` writes **both** a systemd user unit and an XDG autostart entry and
enables the unit. Use whichever your desktop honours; starting both is
harmless (the second daemon refuses the live socket and exits with a one-line
reason) but noisy in the logs. To pick one:

```bash
# systemd only
systemctl --user enable pergola.service
rm ~/.config/autostart/pergola.desktop
# autostart only
systemctl --user disable pergola.service
```

The XDG entry only runs in the desktops listed in `OnlyShowIn` (XFCE, GNOME on
X11, KDE, LXDE, LXQt, MATE, Cinnamon, Unity); elsewhere keep the systemd unit.

The systemd unit cannot inherit `DISPLAY` from your session, so `install.sh`
bakes in the display it was run under. If you change display (or the daemon
starts on the wrong screen), re-run `./install.sh --no-start`.

## Configuration

`~/.config/pergola/config.json`, all keys optional (missing keys fall back to
the defaults):

```json
{
  "gap": 6.0,
  "minimum_share": 0.05,
  "auto_dock": "off",
  "overlay": false,
  "desktop_share": 0.0,
  "hotkeys": true,
  "edge_dock": true,
  "notify": true,
  "bindings": {
    "zoom-out": ["Super+Page_Up"],
    "back": ["Super+b", "Alt+b"],
    "view 3": ["Super+3"],
    "overlay": []
  }
}
```

| Key | Default | Effect |
|---|---|---|
| `gap` | `6.0` | pixels between tiles, and width of the overlay divider |
| `minimum_share` | `0.05` | smallest fraction a tile can be squeezed to when weights are edited |
| `auto_dock` | `"off"` | `off`, `new` or `all`: whether a new window is docked (`new` after startup, `all` including the session it found); the old `true`/`false` still parses |
| `overlay` | `false` | draw split lines in the gaps and a frame around the viewport, plus a dim layer when a compositor can render it |
| `dim` | `0.4` | strength of the overlay's dim layer, `0` (off) to `0.9`; only used with a compositor. At `0` the split lines are 4 px wide instead of 2, so the boundary still reads without a dim layer |
| `tags` | `false` | bind `Super+1`…`9` (`Ctrl+Alt+1`…`9` first) to the nine named workspaces, and `Super+Shift+n` to move the focused window onto one. `on`/`off` |
| `desktop_share` | `0.0` | share of the screen `arrange` gives the desktop leaf; `0` means the desktop gets no strip |
| `terminal` | `$TERMINAL`, else an installed terminal (`xfce4-terminal` first, `xterm` last) | program launched by `split-terminal`; set this key to choose a specific one |
| `hotkeys` | `true` | grab the default chords on the root window |
| `edge_dock` | `true` | dock a floating window dropped on a screen edge (mouse tiling) |
| `notify` | `true` | show a desktop notification when a hotkey or palette action fails. A chord runs inside the daemon, where stderr goes to the journal — the one place a person does not look. Best effort: without a notifier the failure still reaches the journal |
| `bindings` | `{}` | per-command chord overrides: command → candidate chords, tried in order |

### Remapping keys

For one change, use the command line — it writes `config.json` for you, keeps
every other setting, reloads the daemon and prints any conflict it reported:

```sh
pergola bind "gap 12" Super+9   # bind a command to a chord and reload
pergola bind "gap 12"           # show what it is bound to and where it comes from
pergola bind "gap 12" --default # drop the override, back to the default chord
pergola unbind "gap 12"         # remove the chord entirely
```

`pergola bind` rejects an unknown command and an unparseable chord instead of
writing a binding that would never fire. If no daemon is running the change is
still saved and applies at the next start.

For several changes at once, edit the `bindings` map directly. It maps a command
to the chords to try, in order. Naming a
command replaces its default candidates entirely; an empty list unbinds it;
commands you do not mention keep the defaults. Any command the daemon
understands can be bound, not only the ones in the default plan, so `view 3`,
`gap 12` or `frame-desktop` can be put on a chord:

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

Chords are written `Modifier+…+Key`, with modifiers `Ctrl`, `Alt`, `Shift` and
`Super` (aliases `Control`, `Mod1`, `Mod4`, `Win`) and keys named as in
`xmodmap`: letters, digits, `Space`, `Return`, `Tab`, `Escape`, arrows,
`Page_Up`, `Page_Down`, `Home`, `End`, brackets and punctuation. A chord the
desktop already owns fails to grab and falls through to the next candidate.

Notes:

* `pergola reload` re-reads `gap`, `minimum_share`, `desktop_share`,
  `auto_dock`, `terminal` and `bindings`, and re-grabs the hotkeys, so a
  `bindings` or `hotkeys` edit takes effect without a restart. `overlay` is
  applied when the daemon starts — restart it to change that. If the edited
  config is invalid, reload reports the error and keeps the running settings
  and key bindings instead of silently switching to defaults.
* `pergola gap 12` changes the running layout only; put the value in
  `config.json` to keep it across restarts.
* `pergola overlay on` is likewise a session toggle; the config key is the
  default at startup. With a compositor (any program that owns
  `_NET_WM_CM_S0`: xfwm4, picom, kwin, mutter) the overlay also dims the tiles
  with a black layer under the split lines, which makes the tree readable when
  zoomed out; without one — or without the SHAPE extension that keeps the layer
  click-through — it draws the lines alone. `pergola overlay on` reports which
  of the two it did (`dim 40% 0x…`, `dim off (dim 0)` or `dim unavailable`).
  The layer's strength is the `dim` key: `0.4` (40% black) by default,
  `pergola settings dim 0.6` for a stronger dim, `0` to turn it off and keep the
  lines as the only cue. With no dim layer the split lines are drawn 4 px wide
  instead of 2, so a boundary still reads on a bare X server.
* A malformed file is reported on stderr and ignored (defaults are used); the
  daemon still starts.
* `pergola settings` opens a menu with every key and every command's chords;
  pick a row, type a value, and it is validated and written atomically, then the
  daemon reloads. Each row carries its own help — what the key controls, the
  range the validator enforces and the compiled default — so
  `gap = 6 — pixels between tiles; 0 to 400 (default 6)` is legible without the
  manual. `pergola settings gap 12` does the same from a script and
  `pergola settings --list` prints the rows without a menu program. A chord row
  accepts a comma-separated list (`Super+9, Super+8`) and binds each chord.
* A **rejected** value is also shown as a desktop notification, because the menu
  and the palette run detached and their stderr goes to the journal. The
  notifier is `notify-send`; set `PERGOLA_NOTIFY` to use another program (its
  own arguments stay, e.g. `PERGOLA_NOTIFY="dunstify --urgency=critical"`). With
  no notifier installed the message stays on stderr and the exit code is still
  non-zero — the value is never written.
* `pergola config` prints every key with the value in force and where it came
  from — `default` or `config.json:<line>` — so a typo cannot hide behind a
  default. `pergola config --check` exits non-zero on an unknown key, an
  out-of-range `gap`/`minimum_share`/`desktop_share`, a `terminal` that is not
  on `PATH`, or a `bindings` chord that does not parse, and names the line. It
  reads the file itself, so it works with no daemon running.
* `pergola keys --check` lists what was actually bound and which planned chords
  the desktop already owns, including your `bindings` overrides (`off` marks a
  command you unbound with an empty list). `pergola keys --probe <chord>…` tests
  individual chords without installing anything. `pergola bind <command>` with no
  chord shows the same answer for one command.
* `pergola bind` and `pergola unbind` never rewrite a malformed `config.json`: a
  typo in the file is reported and the file is left untouched, so your edits are
  not silently replaced by regenerated defaults.

### Tab groups

A panel can hold several windows as tabs. Every view stays mapped at the panel
rectangle and only the selected one is raised, so switching a tab is instant —
nothing is unmapped or re-laid-out.

```sh
pergola tab                     # fold the focused window into its panel as a tab
pergola tab --window 0x40000c   # fold a specific window
pergola tab --window 0x40000c --target p-3176783  # choose the panel
pergola tab-select 0x40000c     # show a named view (seat or window id)
pergola tab-next                # cycle to the next view in the focused group
pergola tab-prev                # cycle back
```

A floating window can be folded in too: `tab` takes a seat off the desktop
layer and docks it into the target panel as a view. Without `--target` it joins
the last-focused tile (or the first leaf when there is none), so a floating
window with nowhere obvious to go is docked rather than refused.

Switching a tab also gives the shown window the keyboard, so typing goes to the
view you can see. Closing the shown tab drops only that view — its siblings keep
the panel — and the selection is repaired to the neighbour that took its place;
the group collapses only when the last view goes.

`tab-next` and `tab-prev` are bound by default to `Ctrl+Alt+Page_Down`/`Page_Up`
(`Super+Page_Down`/`Page_Up` when the desktop owns the primary chord). Remap or
unbind them like any other command with `pergola bind tab-next <chord>`.

There is no tab bar and no drag-a-window-onto-a-tab gesture yet: the hidden
views are reachable by command or chord, not by clicking. See
[BACKLOG.md](../BACKLOG.md) for the planned UI.

### Named workspaces (tags)

Without tags every view is on one screen. A tag names a set of views; switching
to it lays out only those windows, and the rest are moved off the work area
while staying in the document, so switching back restores the layout exactly.

```sh
pergola window-tag work              # put the focused window on "work" (and on that tag only)
pergola window-tag work --window 0x40000c
pergola window-tag play --window 0x60000c
pergola tag-view work                # show "work" — "play"'s windows are parked
pergola tag-view                     # list the workspaces (* marks the active one)
pergola tag-view all                 # back to every view
pergola tag-remove play              # delete a workspace; its windows stay open
```

A view belongs to at most one tag, so `window-tag` takes it off the tag it was
on. A window that opens while a tag is shown joins that tag — it never appears
on the wrong workspace. Each tag remembers the window that had the keyboard, so
returning to it lands where you left off. An empty tag shows only the desktop
layer; the command says so and names the way back.

The active tag is part of the layout document, so a daemon restart keeps you on
the workspace you were on.

Making the workspaces one key away is opt-in:

```json
{"tags": true}
```

With it on, the default plan binds `Super+1`…`9` to `tag-view 1`…`9` and
`Super+Shift+1`…`9` to `window-tag 1`…`9` (the `Ctrl+Alt` chord is tried first,
like every other command). They are behind a key rather than always on because
`Super+1` on a desktop that never named a workspace would blank the screen, and
the first press is a surprise a default should not spring. `pergola reload`
takes the change without a restart, and any chord can be replaced by name:

```sh
pergola bind "tag-view 1" Super+F1   # a workspace chord of your own
pergola keys --check                 # confirm what really got bound
```

A switch also raises a desktop notification naming the workspace (and how many
windows it holds), because the screen itself cannot say where you are — the
overlay draws no text. For the same reason there is no persistent workspace
bar: a strip of unnamed rectangles would be a worse workspace list than the
notification and `pergola tag-view` already give you. A status bar can draw a
real one from `pergola subscribe`.

Two consequences worth knowing: `arrange` and `float-all` rebuild the shared
tree, so they act on every tag; and a parked window's client area is off-screen
but the desktop may keep its title bar reachable (xfwm4 does).

## Keyboard and mouse

Everything that tiles a window is reachable without a shell:

| To… | Do this |
|---|---|
| tile the focused window left/right | `Ctrl+Alt+Shift+←` / `→` |
| stack it above/below | `Ctrl+Alt+Shift+↑` / `↓` |
| float it again | `Ctrl+Alt+Shift+W` |
| choose from a menu | `Ctrl+Alt+M` (needs rofi/zenity/yad/dmenu) |
| find a split to resize | hover near the gap: the band lights up as a thin line |
| resize a split | drag the gap between the two tiles, `pergola ratio 0.3`, or nudge with `Ctrl+Alt+,` / `Ctrl+Alt+.` |
| tile by mouse | drag a window to a screen edge and let go — a frame shows the target while you drag |
| untile by mouse | drag the tile away from the edge |
| move a tile to another edge | drag the docked tile out and settle it near the edge you want — one gesture, no float step |
| switch tabs in a group | `Ctrl+Alt+Page_Down` / `Page_Up` |
| undo the last layout change | `Ctrl+Alt+Z`, or `pergola undo` |
| focus a window from a taskbar | click its entry — the click is an `_NET_ACTIVE_WINDOW` request, and a hidden tab view is shown first |
| switch workspace | `Super+1`…`9` with `tags: true`, or `pergola tag-view <name>` |
| move a window to a workspace | `Super+Shift+1`…`9` with `tags: true`, or `pergola window-tag <name>` |

The chords fall back to `Super+…` per command when the desktop owns the
`Ctrl+Alt` chord — `pergola keys --check` prints what was really bound. The full
list is in the [README](../README.md#hotkeys).

## Verify the installation

```bash
pergola doctor        # display, EWMH client list, running WM, version
pergola status        # the running daemon's view: screen, panels, depth, gap, overlay
pergola dock left      # dock the focused window — the real end-to-end test
pergola rects         # frame rectangles, as the daemon believes them
pergola subscribe     # live JSON records of layout/focus/dock/undock/close (Ctrl+C to stop)
pergola-health        # soak check; exits non-zero on any failure
```

`pergola subscribe` is what a status bar or panel reads instead of polling
`pergola tree` every second: one JSON object per line, starting with a state
snapshot. Feed it to `while read -r line; do …; done`, or point a bar at it.
The stream is non-blocking on the daemon's side, so a reader that stops reading
cannot slow the window manager, and the client exits 0 when the daemon stops so
a supervisor can restart it.

`pergola-health` checks daemon liveness, CPU use, layout invariants, geometry
drift against what the X server reports, docked windows that no longer exist,
and (with `--e2e`) runs the full regression suite. `--e2e --wm openbox` runs
that suite under a second window manager, and the summary line and the
`health.jsonl` record both name the one that ran (`"wm":"openbox"`). The suite
judges the tree's HEAD, so a binary built from another commit is rebuilt first
(`cargo build --release`, logged to `/tmp/pergola-health-build.log`);
`PERGOLA_NO_BUILD=1` or an explicit `PERGOLA_BIN` turns that off, for measuring
a release artifact as published. `--json`
prints one line, `--quiet` prints nothing when healthy; both keep the exit code,
so a timer can act on it. The suite uses fixed sleeps, so run it on an idle
machine — under CPU saturation a few checks flake.

## Upgrade

```bash
cd pergola
git pull
./install.sh --no-start     # never overwrites config.json
pergola stop
setsid pergola daemon >/dev/null 2>&1   # or: systemctl --user restart pergola
```

A running daemon keeps the old binary mapped until it is restarted, so the
`pergola stop` / start (or `systemctl --user restart`) step is what actually
upgrades the running layout engine. `layout.json` is additive: an older layout
still loads after an upgrade.

## Uninstall

```bash
pergola stop
systemctl --user disable --now pergola.service 2>/dev/null
rm -f ~/.local/bin/pergola ~/.local/bin/pergola-health
rm -f ~/.config/autostart/pergola.desktop
rm -f ~/.config/systemd/user/pergola.service
# optional: your settings and layout
rm -rf ~/.config/pergola ~/.local/state/pergola
```

Floating windows keep whatever geometry they had; a `pergola float-all` before
stopping returns every window to the desktop layer.

## Troubleshooting

| Symptom | Check |
|---|---|
| `pergola: daemon not running` | `pergola daemon` in a terminal; then `pergola status`. One daemon per display, one socket |
| `pergola doctor` cannot find a display | `echo $DISPLAY`; the daemon and the CLI need the same X display |
| `doctor` reports no EWMH client list | no window manager running, or one that does not implement EWMH |
| Windows do not move when docked | the WM ignores `_NET_MOVERESIZE_WINDOW` (tiling WMs that lay themselves out) — only Xfwm4 and Openbox are measured |
| Tiles sit under the top panel | that is the work-area class of bug: `pergola-health` asserts every tile is inside `_NET_WORKAREA`; upgrade if it fails |
| Hotkeys do nothing | `pergola keys --check`; the desktop may own the chord; Pergola falls back to `Super` per command and says which ones had no free chord at startup |
| `Ctrl+Alt+M` does nothing | no menu program: `sudo pacman -S rofi`, or set `PERGOLA_MENU=/path/to/menu` |
| Dragging a gap does nothing | there must be a gap: set `gap` above 0; the strip is only as wide as the gap. With `gap: 0`, use `pergola ratio 0.3` instead |
| A dropped window landed somewhere nested | edge drops dock against the last focused space by design — the frame shown during the drag is exactly where it will land; drop again on the edge you want, or use `Ctrl+Alt+Shift+arrows` after focusing the target tile |
| Hovering a gap draws nothing | the hints need the SHAPE extension; without it the bands still work but the cue is missing (`pergola doctor` reports the extension) |
| A window peeks at the screen edge when zoomed out | the WM clamps off-screen windows (xfwm4 does); documented limitation |
| Terminal tiles do not fill the tile exactly | the client's resize hints (a character grid) win; the layout is still the tile |
| Second daemon will not start | expected: one instance per display, guarded by the Unix socket |
| Daemon exits at login | `journalctl --user -u pergola -n 50`; usually `DISPLAY` — re-run `install.sh` from the right session |

Tuning knobs that answer the most common complaints: `gap` (thicker split
lines and spacing), `desktop_share` (keep a desktop strip when docking), and
`auto_dock` (stop new windows from floating: `new` docks windows opened after
the daemon started, `all` also adopts the session it found).

# Backlog

The development queue. Each item says **what**, **why it matters**, and **done when** — an item
without a testable "done when" is a wish, not a task. Order is deliberately shallow: pick the top
one that is not blocked.

The agenda (`pi-agenda show`) works this list — `pergola-dev` every 2d, `pergola-gate` every 14d
and the soak/e2e checks; closing an item here means committing code, tests and a CHANGELOG line.

## Done

### The queue can say it is empty, so a pass is not spent on nothing (0.5.0-dev)

`pergola-dev` opened a session every 2 days whether or not this file had an item in it; the
2026-11-17 pass closed all 19 and then re-verified a clean tree 20 times in 70 s (bug #198).
`scripts/backlog-open.sh` answers the question for a scheduler: exit 0 + `N open` when an item
sits outside `## Done` / `## Blocked` / `## Not planned`, exit 1 with the reason when none does
(`--list` prints them, `PERGOLA_BACKLOG` points it at a fixture). The agenda row now registers
that command as its queue check, so a due pass with an empty queue records `due-empty`, rolls the
row forward and opens no session. Measured: the script returns 1 on this file today and 0 with
`2 open` on a fixture; `pi-text dispatch --due --dry-run` prints `idle pergola-dev`.

### A drag reported inside the own-apply guard still docks (0.5.0-dev)

Openbox reports a whole drag as one ICCCM-synthetic `ConfigureNotify`, and the
daemon re-applies the layout just before the drop, so `note_configure`'s
own-apply guard (100 ms) discarded the only event that carried the drop
position: the e2e assertion "a window dragged to the left edge is docked" failed
on openbox (42/43) while xfwm4 passed. Removing the guard was measured and
rejected — intermediate echoes of a two-step apply read as drags and every
window docked itself (openbox 1 -> 7 failures). An event inside the guard is now
remembered (newest rect per window) and re-tested after it: the window's live
rect is read back, an echo that settled on the rect we applied is discarded, and
a rect that stands still away from it for 80 ms is judged as the drop. Unit
tests pin the echo, the settled drop and the still-moving cases.
`scripts/e2e.sh --wm openbox` 211-212/212 (2 runs, the left-edge assertion
passing in both) and `--wm xfwm4` 214/214.

### A failed hotkey or palette action reaches a desktop notification (0.5.0-dev)

A chord runs inside the daemon and a palette action runs as a detached child,
so their stderr goes to the journal — the one place a person does not look. The
new `notify` config key (default `true`) sends the failure as a desktop
notification through `notify-send` (or `PERGOLA_NOTIFY`), best effort: without a
notifier the failure still reaches the journal. The palette now captures the
action's reply instead of letting it hit `/dev/null`, so a rejected action is no
longer silent. The e2e binds a chord to a command that fails at run time
(`dock sideways`) and asserts the toast arrives.

### A window on another host desktop is left alone (0.5.0-dev)

The host WM's desktops are not pergola's tags. A window the user sends to
another desktop (`_NET_WM_DESKTOP` ≠ `_NET_CURRENT_DESKTOP`, and not the sticky
`0xFFFFFFFF`) belongs to the WM there; `apply`, `park_inactive` and the floating
pass now skip it, and a newly opened window on another desktop is not adopted
until the user brings it here. This is the daemon side of the measured
`pi-tile --park` failure (#114): a parked window came back on the next
`arrange` because the daemon re-applied the whole tree. The predicate is
unit-tested (current, foreign, sticky, missing properties) and the e2e moves a
window to a second host desktop, runs `arrange`, asserts it stays, then returns
it to its tile.

A second hole the same measurement found: `arrange` **docks** a floating window
back into the tree, and the raise that follows a fresh dock is what xfwm4 uses
to remap a window onto the current desktop. The geometry move is skipped for a
foreign window, but the raise was not, so a parked window came back the moment
`arrange` gave it a panel. The raise now sits behind the same guard, and the
e2e covers the floating case as well as the docked one.

### Tab groups — `views > 1` is real (0.4.0-dev)

A panel with several views keeps every view mapped at the panel rectangle and raises the
`selected` one; `tab --window <id>` folds a window in, `tab-select <seat|window>` picks,
`tab-next`/`tab-prev` cycle (default chords `Ctrl+Alt+Page_Down`/`Page_Up`, `Super+…` fallback),
and closing the selected view promotes the next. Switching a tab activates the shown window, so
the keyboard follows the view. `Panel::select`/`cycle`/`remove_view` and
`LayoutEngine::removing_view` are unit-tested; the e2e asserts both views at one rect, exactly one
raised, `tree` reporting `selected`, a real chord press, and that closing one tab leaves its
sibling tiled. 75/75 on xfwm4 and openbox.

### Split ratios without a pointer (0.4.0-dev)

`pergola ratio <0.0-1.0>` rebalances the split that contains the focused panel and reports the
resulting weights (`ratio 0.30 · split weights [0.30, 0.70]`). Values outside 0–1 are refused, and
`LayoutEngine::set_ratio` clamps to `minimum_share`, matching a gap drag of the same delta. The
unit test checks the clamp against `resizing_gap` and rejects NaN; the e2e asserts the reported
ratio and the new geometry. Palette rows for half/third and a nudge chord are still open (see
the Ratios row in `docs/UX-NOTES.md`).

### Auto-dock policy that does not surprise (0.4.0-dev)

`auto_dock` is a tri-state — `off`, `new`, `all` — and the old boolean still parses (`false` is
`off`, `true` is `all`). `new` docks a window that appears after the daemon's first reconcile and
leaves the session it found alone; `all` also adopts the windows that were already on screen;
`off` never adopts. The target is the focused panel *only if it is in the tree*; otherwise the
last-focused docked panel, otherwise the whole tree, so the WM having focused the new window
does not make it its own docking target. A `DestroyNotify` handler removes a dead window's seat
immediately instead of waiting for the 2 s reconcile: the X server reuses the XID, and a reused
id was mistaken for the dead window and never adopted (measured on openbox). The unit test is a
table over the modes; the e2e asserts a new window floats with `off` and docks with `new`. 85/85
on xfwm4 and openbox.

### AUR package: `pergola-git` (0.4.0-dev)

`packaging/aur/PKGBUILD` builds from `main` and installs to system paths: `/usr/bin/pergola`,
`/usr/bin/pergola-health`, `/usr/lib/systemd/user/pergola.service`, the XDG autostart entry, the
LICENSE/NOTICE and the docs. `makepkg -f` then `pacman -U` was run end to end: the file list
matches what `install.sh` writes (the config ships as an example in `/usr/share/doc/pergola-git/`,
since pacman must not write into `$HOME`), `/usr/bin/pergola doctor` was green on the installed
copy, and the package was removed afterwards to leave the user install as the live one.

The backlog asked for `pergola-bin`; that name is a contract in the AUR (`-bin` = prebuilt, no
compilation) and there is no release artifact to download, so shipping it would have meant a
package that lies about itself. The rename and the reason are in `packaging/aur/README.md`; a
`pergola-bin` package is item 2, blocked on a release pipeline.

Two deliberate deviations from the source install: the packaged unit does not hard-code
`DISPLAY=:0` (it inherits the graphical session's environment), and `install.sh` remains the path
for source installs.

### Overlay dim layer (0.4.0-dev)

With a compositor owning `_NET_WM_CM_S0`, `overlay on` draws a full-screen 40% black layer under
the split lines, so 2 px borders read at a glance. The layer carries an empty SHAPE input region,
so pointer events reach the tiles under it (checked at eight screen points, none of which landed
on it). Without a compositor, or without SHAPE, the overlay is borders only and `overlay on`
reports `dim unavailable` rather than claiming a layer it cannot draw. Measured under xfwm4's
compositor: mean screen brightness 0.162 off → 0.098 on → 0.159 off (a 0.60× factor, as 40%
black predicts; the absolute value follows the screen content). `overlay off` destroys the
layer, and 100 toggles leave the root window's child count unchanged with no window carrying
`_NET_WM_WINDOW_OPACITY`. The e2e proves the fallback, then a stand-in selection owner proves the
layer (`scripts/xhelper.py`).

### Config validation and `pergola config` (0.4.0-dev)

`pergola config` prints every key with the value in force and where it came from (`default`, or
`config.json:<line>`), so a misspelled key can no longer hide behind a default. `pergola config
--check` exits non-zero and names the line for an unknown key, an out-of-range
`gap`/`minimum_share`/`desktop_share`, a `terminal` that is not on `PATH`, or a `bindings` chord
that does not parse; it reads the file itself, so it works with no daemon running. The line finder
tracks JSON strings and nesting, so a binding named after a command does not shadow the key, and a
one-line config still names its source. Also fixes the CLI panicking on a closed pipe
(`pergola config | head`), by restoring the default SIGPIPE handler in `main`.

### Settings and chords in the palette (0.4.0-dev)

`pergola settings` opens the same menu programs the action palette uses with one row per config
key and one per command — `gap = 6`, `chord: dock left = Ctrl+Alt+Shift+Left, Super+Shift+Left` —
so the chord map is the `keys --check` view, not a guess. Picking a row asks for a value with the
current one prefilled (`rofi -filter`, `dmenu`, `zenity`/`yad --entry-text`), then applies it
through `pergola settings <key> <value>`, or `pergola bind <command> <chord>` for a chord row.
That primitive validates the whole candidate file with the checks `config --check` runs, writes
atomically and reloads; only problems the change *introduces* block it, so a pre-existing typo
elsewhere does not make the file uneditable (`--check` still reports it, on its line). The menu
path is proven in the e2e with a stand-in dmenu, so no GUI is needed. Still missing: per-key help
in the rows, and a rejected value reported as a notification (UX-NOTES).

### A second window manager in the measured gate (0.4.0-dev)

`scripts/e2e.sh` takes `--wm xfwm4|openbox|i3`, and `pergola-health --e2e` now takes `--wm` too,
forwards it, and records which one ran: the suite's summary line names it (`e2e: 93 passed, 0
failed (wm=openbox)`) and every `health.jsonl` record carries `"wm"`. Two openbox runs of the
whole suite passed with no WM-specific tolerance: the geometry comparison uses each client's own
resize increment (never under 2 px), which already covered the xterm size-hint snap both WMs do,
so no per-WM deviation needed its own number. Run the gate on an idle machine — under CPU
saturation three checks flake (`P list` empty reply, an `arrange` tile left at 0,0, a held chord
that does not step back), and the same three flake at the previous commit, so they are the
suite's fixed sleeps rather than a product regression (`docs/DESIGN.md`).

### Layout undo (0.4.0-dev)

`pergola undo` (bound to `Ctrl+Alt+z`, falling back to `Super+z`) restores the layout from before
the last change: a dock, a float, a drag out to the desktop, an edge drop, an arrange, a gap
change or a gap drag. The ring keeps ten states; each snapshot is the document (tree, floating
rects, tab views) plus the engine's gap and the camera, so undoing a zoomed change puts the camera
back where it was too. A command that changed nothing is not recorded twice, and `undo` itself is
never recorded — otherwise it would oscillate between two states. Windows that closed since the
snapshot are dropped by the reconcile, so undo cannot resurrect a dead seat. `History<T>` lives in
`pergola-core` with its own tests for the bound and the de-duplication; the e2e docks a window by
chord and undoes it.

### Pointer affordances (0.4.0-dev)

The mouse paths are visible now. Hovering a gap band draws a 3 px line down its long axis, so the
6 px strip is findable without hunting; the hint is a separate override-redirect layer with an
empty SHAPE input region, so it never takes the pointer from the band. Dragging a window within
70 px of a work-area edge draws a 4-bar frame around the rectangle the drop would produce: the
preview runs the same three steps as `dock_window` (choose the target, take the seat out of the
tree, insert against the edge), so the frame cannot point somewhere the window will not land. A
docked tile dragged out of its own rectangle and settled near a different edge re-docks there in
one gesture. Hints are skipped when the SHAPE extension is missing; the bands still work. The e2e
asserts the hover hint appears on a gap and leaves no window behind, the drop frame's four bars
under xfwm4 (openbox coalesces a drag into one `ConfigureNotify`, so there is nothing to draw
mid-drag), and the one-gesture re-dock. `HINT_COLOR`, `hint_line`, `frame_bars` and `pointer_edge`
are unit-tested.

### Event stream for status bars (0.4.0-dev)

`pergola subscribe` turns the control socket into a push stream of newline-delimited JSON: a state
snapshot on connect, then `dock`, `undock`, `window-opened`, `window-closed`, `layout` and `focus`
records. Events are diffs of committed state between event-loop passes, so one command's several
mutations collapse into one transition and no event is emitted from a half-applied tree. Writes are
non-blocking and per-client, with a 256-record queue, a remembered partial-write offset (a plain
`write_all` would resend a record after `WouldBlock` and corrupt the stream) and a ten-second
idle drop, so a bar that stops reading cannot stall the window manager — measured at load 5.6: a
subscriber whose stdout went to an undrained fifo left `pergola status` answering in 47 ms. When
the daemon exits, the client prints a note and exits 0 so a supervisor restarts it. Unit tests
cover the transition table (new window, dock, undock, close, layout, focus, no-change); the e2e
reads the whole stream from one session and asserts every required record names the window.
The harvest's **ipc / scripting** class was 227 issues.

### Pager activation requests (0.4.0-dev)

A taskbar's "click to focus" sends `_NET_ACTIVE_WINDOW` to the root window; the daemon only ever
*set* that property when it focused a window, so the click was a silent no-op. The daemon now
selects the root's shared `SubstructureNotify` mask — it cannot take `SubstructureRedirect`, which
the desktop owns — and handles the request: a window in the tree is focused and its seat recorded
as last-focused; a window in a tab group that is not the shown view switches the tab first, so the
desktop never focuses a window nobody can see; a window the daemon does not manage (a stale pager
entry, an override-redirect popup) is ignored rather than focused off the tree. The daemon's own
activation requests echo back through the same selection, so they are tracked for two seconds and
consumed instead of re-handled, which would otherwise bounce focus between the layer and the
desktop forever. Timestamps clients send as `0` are accepted. Unit tests cover the four targeting
cases (shown view, hidden view, floating window, unmanaged id); the e2e asserts an external
`xdotool windowactivate` changes the daemon's active window, that the daemon logged handling the
request itself (an assertion on `status active` alone would pass with the handler deleted, because
the desktop sets the property either way), and that a request for a hidden tab view shows it.
The harvest's **ewmh / icccm** class was 168 issues.

### Named workspaces (tags) on top of views (0.5.0-dev)

A tag is a named set of views over the one tree: `active_tree()` prunes the panels that hold none
of its views and collapses a one-child split, keeping every surviving node id. `pergola window-tag
<name>` moves the focused window onto a workspace (creating it), `tag-view <name>` switches to it,
`tag-view all` shows every view, `tag-view` lists the workspaces, and `tag-remove` deletes a name
without closing its windows. A view belongs to at most one tag (the sets stay disjoint), a tag
remembers the window that had the keyboard, and a window that opens on a shown tag joins it.

A view on a tag that is not shown is *parked* below the work area, keeping its seat, panel, place
in the tree and rectangle, so switching back is exact; the active tag lives in the document and
survives a restart. `status`/`list`/`rects` report the active tag, and the event stream publishes a
`tag` record (with `tag`/`tags` in the state snapshot).

The pruning rule is pure and unit-tested (disjoint sets, collapse of a one-child split, an empty
tag, a tag surviving its last window, a tagged document read by a tags-unaware reader); the e2e
asserts disjoint sets, only the active tag mapped, a parked window mostly off the work area (0%
visible under xfwm4, 7% under openbox — both clamp a window they must not lose),
the escape hatch, a restart and a `tag` record. The document gained `doc.tags`/`doc.active_tag`
behind `serde(default)`, so old documents load unchanged and an older binary reads a tagged
document without losing a view — the `MINOR` bump the item called for. The harvest's
**workspace / tag** class was 236 issues.

Known gaps, recorded rather than hidden: `arrange`/`float-all` rebuild the shared tree and so act
on every tag, the camera and gap are global, tags have no default chords (pressing one on a
desktop with no tag would blank the screen), and a desktop clamps a parked window so a strip stays
reachable at the bottom edge — xfwm4's title bar, openbox's first 66 px of an 862 px client
(`docs/UX-NOTES.md`).

### Settings that explain themselves (0.5.0-dev)

Every `pergola settings` row now carries its own help — what the key controls, the range the
validator enforces and the compiled default (`gap = 6 — pixels between tiles; 0 to 400 (default 6)`)
— so the menu is legible without the manual. A chord row accepts the comma-separated list it
prefills (`Super+9, Super+8` binds both) instead of failing to parse its own value. A *rejected*
value is also sent as a desktop notification, because the menu and the palette run detached and
their stderr goes to the journal; the notifier is `notify-send`, overridable with
`PERGOLA_NOTIFY` (its own arguments stay), and with no notifier the message stays on stderr with the
same non-zero exit. The tests: four unit tests over the invocation (default app name, override
words, empty override, a notify-send override) and an e2e block asserting the help text, the
comma-separated chord row writing both chords, and a stand-in notifier recording the rejection while
the value is still refused and the file untouched. `docs/UX-NOTES.md`'s Settings row is closed; the
Feedback row narrows to dock/float errors.

### Ratios from a chord, and a palette built from the focused window (0.5.0-dev)

`ratio` now takes a delta as well as an absolute share: `+0.05` / `-0.05` read the focused
panel's current weight in its split and move it, so one chord works whatever the split's size is.
The default plan binds `ratio -0.05` to `Ctrl+Alt+,` and `ratio +0.05` to `Ctrl+Alt+.` (Super
fallback when the desktop owns Ctrl+Alt), and the reply reports the *applied* share — the weight the
engine clamped to — not the number that was asked for.

The action palette is no longer a fixed list. `palette::state()` asks the daemon for the active
window and its panel, then the tree, so `items_for()` hides "Float this window" for a window that
already floats and offers "Fold this window into a tab" instead; the prompt names the window. Ratio
rows for half / one-third / two-thirds were added. `palette --list` prints the same rows on stdout,
which is how the e2e checks the state without a menu program: a floating window hides Float and
shows Fold, a docked one does the reverse, and the half row is present. The nudge chord is asserted
by measuring the focused window's own width after a real key press (the bound chord is read from
`pergola keys`, not assumed). Pure parsers (`parse_active`, `state_from_tree`) have unit tests, and
the palette's label/verb tables are tested for both states.

### A cue that reads without a compositor (0.5.0-dev)

`dim` in `config.json` (`0.4` default, `0`–`0.9`) sets the overlay's dim strength, so `pergola config`
shows it with its source and `config --check` rejects an out-of-range value. The split lines are
drawn 4 px wide on their thin axis whenever no dim layer is created — no compositor, no SHAPE, or
`dim 0` — so a boundary still reads on a bare X server; with a dim layer they stay 2 px. The e2e
measures the divider windows themselves (they carry `_PERGOLA_OVERLAY`, so `xhelper.py bars` reads
their real geometry from the server): 4 px with no compositor, 2 px under a stand-in compositor
owner, and `dim 0` leaves no layer and reports `dim off (dim 0)`. `dim 0.6` is 60% opaque. The
width is also unit-tested. The honest gap that remains: nobody has looked at a 4 px line on a real
bare-X session yet (UX-NOTES, the Overlay row).

### Workspaces one key away (0.5.0-dev)

`{"tags": true}` adds the nine workspace chords to the default plan: `Super+1`…`9` (tried as
`Ctrl+Alt+1`…`9` first, like every command) switch to `tag-view 1`…`9`, and `Super+Shift+n` runs
`window-tag n` on the focused window. It is opt-in because the first press on a desktop that never
named a workspace would blank the screen, and because the plan is also what `keys --check`, the
settings rows and the palette display — `binding_plan_with(overrides, tags)` takes the key as an
argument rather than reading a file inside the plan builder (DESIGN §4a). `pergola reload` applies
the change without a restart, and `pergola bind "tag-view 1" …` can still replace one chord.

The palette grows a row per workspace the document actually holds — "Switch to workspace work"
and "Move this window to workspace work" — and offers none when no workspace exists. Labels became
owned strings for that (a tag name only exists at run time); the state comes from `doc.tags` and
`doc.active_tag` in the tree, and both parsers are unit-tested.

A switch is announced by a desktop notification naming the workspace and its window count. The
item asked for an overlay indicator, and the overlay is the wrong place for it: the x11 crate has
no text drawing, so the only honest mark would be a strip of unnamed rectangles, which says less
than the notification and still needs a status bar to be legible. The rejection and the reason are
recorded in DESIGN §4a and the UX-NOTES Workspaces row, so the gap is named rather than hidden.
The e2e presses the chords for real: the switch lands and the status names tag 1, a new workspace
starts empty, the notification is in the `PERGOLA_NOTIFY` log, `Super+Shift+1` puts the focused
window on tag 1 and back on screen, `tag-view 2` parks it, the same chord returns, and `tags:
false` releases exactly the workspace chords while `dock left`/`Super+Left` survive. `notify::send`
also reaps its child now, so a daemon that lives for days does not accumulate one zombie per
switch.

## Blocked: Release pipeline, so `pergola-bin` can exist

**Built and verified; blocked on the first stable tag.** `.github/workflows/release.yml` builds
`x86_64` and `aarch64` on a `v<version>` tag (and on `workflow_dispatch` without publishing),
packages each with `scripts/make-release-artifacts.sh`, verifies the `.sha256` files and generates
the AUR files with `scripts/gen-bin-pkgbuild.sh`; the release carries the tarballs, checksums,
`PKGBUILD` and `pergola.install`. A `-dev` version or a tag that disagrees with `Cargo.toml` fails
the run before anything is published.

Verified locally on 2026-11-17: a `0.5.0` artifact was built and its checksum checked; a generated
`PKGBUILD` installed through `makepkg` with the real sums (only the URL was pointed at the local
tarball) and its file list is byte-identical to `pergola-git`'s modulo the package name; the
workflow YAML parses. The `aarch64` compile itself is CI-only — this host has no cross toolchain,
so it has not been run here. The `pergola-git` `pkgver()` was hard-coded to `0.4.0` and lagged the
MINOR bump; it now reads `Cargo.toml`.

**Remaining:** a `v<version>` tag. `docs/VERSIONING.md` says dev builds are commits, not tags and
tags only stable releases, so the first tag is the 1.0.0 gate — no earlier tag can honestly
produce a release.

### 1.0.0 gate: measured on HEAD 3f56532 (2026-11-20) — not met

The gate text is `docs/VERSIONING.md` § "The 1.0.0 gate". HEAD moved from the
recorded `a3d5bce` to `3f56532` (2026-11-20 14:23 -0500, "fix(health): drift is
not computed for a window on another desktop"); branch `main`, working tree
clean. Measurements below are from that commit.

1. **Scope complete — FAIL.** `scripts/e2e.sh` (xfwm4) is 219 passed, 0 failed.
   `scripts/e2e.sh --wm openbox` is 217 passed, 2 failed, exit 1. The two
   failures are commands in the README: "holding a chord goes back once, not
   eleven times: expected [p-80000c] got [p-40000c]" (camera `back`) and "undo
   restored a third tree" (`undo`). Repro: `scripts/e2e.sh --wm openbox`.
   So not every command works on the second WM.
2. **No known reproducible bugs — FAIL on the heading, PASS on the literal test.**
   GitHub has 0 open issues (only PRs #3 and #4 are open; #1 and #2 are closed
   PRs). But the openbox failure above is a known, reproducible bug: it is
   agenda bug #349 ("pergola e2e is not green on openbox", open, p2), and
   `docs/COMPATIBILITY.md` cites it as "bug #349" without saying it is an
   agenda id, not a repo issue. A reader who checks GitHub finds no #349.
3. **Frozen interfaces — PARTIAL.** CLI: `docs/COMMANDS.md` documents every
   command. IPC: `docs/DESIGN.md` documents the socket request/response shape
   (lines 121, 226). layout.json: `version: 2` is named in `docs/VERSIONING.md`
   and `crates/pergola-core/src/document.rs` points at `docs/RESEARCH.md`, but
   RESEARCH.md documents TrellisWM's grammar and `capture/layout-schema.json`
   does not exist in this repo; the v2 document grammar is not written down in
   Pergola's own docs. The "loads in every later 1.x" promise is additive
   loading (`docs/SETUP.md`), not yet tested across a 1.x line.
4. **Green gates — PASS for the default run.** `cargo test`: 138 tests, 0 failed
   (77 + 53 + 8 + 0 + 0), exit 0. `scripts/e2e.sh` (xfwm4): 219 passed, 0
   failed, exit 0. Working tree clean. The openbox variant is condition 1's
   failure, not a separate default-run failure.
5. **Documented limitations accurate — FAIL.** `README.md` "Known limitations"
   says "Verified inside xfwm4 (XFCE) and Openbox", but openbox has 2 failing
   assertions. `docs/COMPATIBILITY.md` does disclose "not green", but the README
   bullet reads as a clean verification and does not point at the failing
   assertions. That is the silent gap the condition forbids. The compat doc's
   count is also stale: it says 221 assertions and 3 failures; this run measured
   219 assertions and 2 failures.

**Verdict: the gate is not met; no tag.** Blocking condition: 1 (openbox
`back`/`undo`). Condition 2 is open as agenda bug #349; conditions 3 and 5 need
doc fixes (layout.json v2 grammar; README's Openbox claim). Condition 4's
default runs are green. Health: `pergola-health` exits 0.

## Not planned

- **Wayland-native.** An external process cannot move Wayland clients; anything here would be a
  compositor, which is a different project. XWayland clients only, documented.
- **Screenshot layer for zoom** (TrellisWM's approach). It needs ScreenCaptureKit-class capture
  plus compositing; the geometry transform is the honest X11 equivalent.
- **macOS.** Different window system, different constraints; out of scope.
- **Tray and panel ownership** (132 issues in the harvest). Pergola reads the work area a
  desktop's panel leaves; it does not draw a bar or own a tray. A tiler that owns the bar is a
  desktop environment, not a tiler.
- **Compositing** (57 issues). Opacity and effects belong to the running compositor; the overlay
  dim layer is the only compositor interaction worth having.

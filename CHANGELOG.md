# Changelog

All notable changes to Pergola. Format follows the versioning rules in
[docs/VERSIONING.md](docs/VERSIONING.md).

## [0.5.0-dev]

### Added

* **Workspaces one key away.** `{"tags": true}` adds the nine workspace chords
  to the default plan: `Super+1`…`9` (first choice `Ctrl+Alt+1`…`9`) switch to
  `tag-view 1`…`9`, and `Super+Shift+n` moves the focused window onto workspace
  `n`. It is opt-in because `Super+1` on a desktop that never named a workspace
  would blank the screen; with the key off the plan, `pergola keys --check` and
  the settings rows are unchanged. `pergola reload` applies the change without a
  restart, and `pergola bind` can replace any single chord by name.

  A workspace switch now also raises a desktop notification naming the workspace
  and its window count (through `notify-send`, or `PERGOLA_NOTIFY`). The screen
  changes wholesale and the overlay draws no text, so the words are the only
  honest indicator; there is deliberately no strip of unnamed rectangles.
  `pergola palette` offers a row per workspace the document actually holds
  (switch, and move the focused window), and offers none when no workspace has
  been started. The e2e presses the real chords: the switch lands, a new
  workspace starts empty, the move chord brings the window with it, the same
  chord returns, and `tags: false` releases exactly those chords while the rest
  of the plan survives. The notification is asserted through the `PERGOLA_NOTIFY`
  stub the suite already uses for a rejected setting.

* **A failed hotkey or palette action reaches a desktop notification.** A chord
  runs inside the daemon and a palette action runs as a detached child, so their
  stderr goes to the journal — the one place a person does not look. The new
  `notify` config key (default `true`) sends the failure as a desktop
  notification through `notify-send` (or `PERGOLA_NOTIFY`), best effort: without
  a notifier the failure still reaches the journal. The palette now captures the
  action's reply instead of letting it hit `/dev/null`, so a rejected action is
  no longer silent. The e2e binds a chord to a command that fails at run time
  (`dock sideways`) and asserts the toast arrives.

* **`scripts/backlog-open.sh` — is there work left?** Exits 0 with `N open` when
  BACKLOG.md holds an item outside `## Done` / `## Blocked` / `## Not planned`,
  and 1 with the reason when it does not (`--list` names them). The agenda's
  `pergola-dev` row checks it before opening a session, so a development pass is
  no longer scheduled against an empty queue — the 2026-11-17 pass ran 49 hours,
  closed every item and then re-verified a clean tree 20 times (bug #198).

### Fixed

* **`scripts/health.sh --e2e` builds the tree under test instead of failing on a
  stale binary.** The provenance check added on 2026-11-17 refuses to judge a
  tree the binary on disk was not built from, which is right — but the refusal
  itself exits 1, so the weekly `pergola-e2e-openbox` row filed a priority-1
  task for a tree nobody had built after the last commit (task #497, 2026-11-25:
  binary 61c6a54, tree bbc2f7d). A stale binary is now rebuilt first (`cargo
  build --release`, logged to `/tmp/pergola-health-build.log`), so the row
  measures HEAD instead of the gap between builds. An explicit `PERGOLA_BIN` or
  `PERGOLA_NO_BUILD=1` keeps the refusal, so a release artifact is still
  measured as published. `scripts/health-selftest.sh` pins all three branches on
  a throw-away checkout in ~8 s, no display needed. Found on the way: the build
  script watched `.git/HEAD`, which a commit on a branch never touches, so
  `cargo build --release` could print "Finished" and leave the binary stamped
  with the previous commit. It now watches `.git/refs` as well.

* **The e2e suite is green under Openbox, and stays green when repeated.** The
  failures that moved between runs (`back` going back twice, `undo` restoring a
  third tree, `tag-windows`, `ratio-nudge`, an activation request) were one bug:
  every `ConfigureNotify` the daemon could not recognise as its own apply started
  a movement watch, and a watch that saw 44 px of travel re-docked the tile. It
  could not recognise two Openbox habits. Openbox reports the client at exactly
  the position asked for, so the frame rect computed from the report is 1 px left
  and 1 px up of the rect we recorded — at a 0.5 px comparison every apply grew a
  phantom watch (`DECORATION_SLOP_PX`, 2 px). And Openbox pulls a parked window
  back so 44 px stay reachable, which is 44 px of travel away from the rect we
  parked it at — a configure for a window our own last apply placed outside the
  work area is now the WM's placement, not a gesture (`parked_off_screen`, tested
  against the work area the rects were computed in, so a zero-sized screen parks
  nothing). Unit tests pin both, and `scripts/e2e-soak.sh` runs the suite N times
  so "green" means repeated, not lucky: measured 2026-11-24, 7 consecutive
  openbox runs (one direct run, three in parallel on their own displays, then
  `scripts/e2e-soak.sh --wm openbox --runs 3`), 0 failed (215–217 assertions)
  and xfwm4 219/219.

* **First-run docking and terminal splitting keep their target.** Focusing a new
  floating window no longer replaces the last tiled target. `split-terminal`
  remembers the source before the new terminal takes focus and works from a
  floating or tiled window. When `$TERMINAL` is unset, Pergola selects an
  installed terminal instead of assuming `xfce4-terminal`.
* **Undo keeps live windows visible.** Returning to an older layout restores
  windows opened after that snapshot as floating windows on their current
  workspace. Layout saves stage
  and rename the complete file so an interrupted write does not truncate the
  previous layout. The X11 suite checks undo across a workspace switch after
  a new window opens.
* **Detached menus retain their selected daemon.** Palette and settings actions
  carry `--socket` into their child commands. Menu and reload failures return
  an error, while ordinary menu cancellation stays quiet. Undo is visible near
  the top of the action palette, and first-run instructions show a nested split.

* **A window dragged under openbox still docks.** Openbox reports a whole drag
  as one `ConfigureNotify`, and the daemon re-applies the layout just before the
  drop, so `note_configure`'s own-apply guard swallowed the only event carrying
  the drop position (the e2e left-edge drag assertion failed on openbox while
  xfwm4 passed). A configure inside the guard is now remembered and re-tested
  once the guard expires, against the window's *live* geometry: an echo that
  settled on the rect we applied is discarded, and a rect that stands still away
  from it for 80 ms is judged as the drop. The guard keeps its job — removing it
  was measured to dock every window — without losing the gesture. Unit tests pin
  the echo, the settled drop and the still-moving cases. Measured: the openbox
  edge-drag assertion passed in two full runs (211-212/212) and xfwm4 214/214.

* **A window docked beside the desktop leaf is visible, and the desktop keeps its
  share.** The desktop leaf is a floating layer, not a tile, so `inserting()`
  now retargets a dock against it to the body beside it. Before this, docking
  beside a zero-weight desktop split the leaf's weight (zero) and gave the new
  window zero pixels — a window that opens and never appears; on the other axis
  the wrap path handed the desktop half the screen. Two engine tests assert the
  docked window has width and the desktop's configured share survives.

* **A window on another host desktop is left alone.** The host WM's desktops
  are not pergola's tags: a window the user sends to another desktop belongs to
  the WM there. `apply` used to move every seat in the tree, so the next
  `arrange` dragged such a window back to the current desktop — the measured
  `pi-tile --park` failure. A seat whose `_NET_WM_DESKTOP` is neither the
  current desktop nor the sticky `0xFFFFFFFF` value is now skipped by `apply`,
  `park_inactive` and the floating pass; a newly opened window on another
  desktop is not adopted until the user brings it here. Raising is skipped too:
  its geometry may be untouched, but xfwm4 answers a raise request by mapping
  the window on the *current* desktop, which is how a floated, parked window
  still came back. The predicate is unit-tested (current, foreign, sticky, and
  missing properties) and the e2e moves both a tiled and a floating window to a
  second host desktop, runs `arrange`, and asserts each stays there before
  returning it to its tile.

* **Named workspaces (tags).** A tag is a named set of views; the active tag
decides which views are laid out. `pergola window-tag <name>` moves the focused
window onto a workspace (creating it), `tag-view <name>` switches to it,
`tag-view all` shows every view again, `tag-view` with no argument lists the
workspaces (a `*` marks the active one) and `tag-remove <name>` deletes the name
without closing its windows. A view belongs to at most one tag, so the sets stay
disjoint; a tag remembers the window that had the keyboard, so returning to it
lands where you left off; and a window that opens while a tag is shown joins
that tag instead of appearing on the wrong workspace.

  A view on a tag that is not shown is *parked* below the work area, not
  unmapped and not dropped: it keeps its seat, its panel, its place in the tree
  and its rectangle, so switching back is exact and needs no saved layout. The
  active tag is part of the layout document and survives a restart. `status`
  names the workspace, `list` marks a parked window and its tag, `rects` reports
  only the active tag's panels, and the event stream publishes a
  `{"type":"tag","name":…}` record plus `tag`/`tags` in the state snapshot.

  The pruning rule lives in `pergola-core` as a pure function and is unit-tested
  (disjoint sets, a one-child split collapsing, an empty tag showing only the
  desktop, a tag surviving its last window, and a tagged document parsed by a
  reader that only knows `root`); the e2e asserts two tags hold disjoint view
  sets, only the active tag is mapped, a parked window is mostly off the work
  area (0% of its client visible under xfwm4, 7% under openbox — both clamp a
  window they must not lose), the escape hatch, a daemon restart and a `tag`
  event. The harvest's **workspace / tag** class was 236 issues.

* **Resize a split from a chord, and a palette built from the focused window.**
`pergola ratio` now takes a delta as well as an absolute share: `+0.05` / `-0.05`
read the focused panel's current weight in its split and move it, so one chord
works at any split size. The default plan binds `ratio -0.05` to `Ctrl+Alt+,`
and `ratio +0.05` to `Ctrl+Alt+.` (Super fallback when the desktop owns
Ctrl+Alt), and the reply reports the share that was actually applied — the
weight the engine clamped to — rather than the number asked for.

  The action palette is built from the focused window instead of a fixed list:
it asks the daemon for the active window and its panel, then the tree, hides
"Float this window" for a window that already floats and offers "Fold this
window into a tab" instead, and names the window in the prompt. Ratio rows for
half / one-third / two-thirds are new. `pergola palette --list` prints the same
rows on stdout, so scripts and the e2e can see the state-aware list without a
menu program; `pergola palette` still opens the menu.

* **Settings rows explain themselves, and a rejection is a notification.**
Every `pergola settings` row now carries one line of help — what the key
controls, the range the validator enforces and the compiled default
(`gap = 6 — pixels between tiles; 0 to 400 (default 6)`) — and a chord row
accepts the comma-separated list it prefills (`Super+9, Super+8` binds both; it
used to hand the whole string to `bind` as one unparseable chord). A rejected
value is also sent as a desktop notification, because the menu and the palette
run detached and their stderr goes to the journal: `notify-send` by default,
`PERGOLA_NOTIFY` to use another program (its own arguments are kept), and with
no notifier the message stays on stderr with the same non-zero exit. The warning
text and the "never write an invalid value" rule are unchanged.

* **A release pipeline for prebuilt binaries.** `.github/workflows/release.yml`
  builds `x86_64` and `aarch64` on a `v<version>` tag and attaches
  `pergola-<version>-<arch>-linux-gnu.tar.gz`, its `.sha256`, and a generated
  AUR `PKGBUILD` + `pergola.install` to the GitHub release; `workflow_dispatch`
  builds the same artifacts without publishing. `scripts/make-release-artifacts.sh`
  packages the binary, `pergola-health`, the unit, the autostart entry, the
  example config and the docs into a deterministic tarball;
  `scripts/gen-bin-pkgbuild.sh` writes the `pergola-bin` files with the real
  checksums. The run fails on a `-dev` version or a tag that disagrees with
  `Cargo.toml`, so a mislabelled artifact cannot be published. No release exists
  yet: `docs/VERSIONING.md` tags only stable releases, so the pipeline waits for
  the 1.0.0 gate (BACKLOG, *Blocked*).

* **The overlay's dim strength is a config key, and the split lines get wider
  when there is no dim layer.** `dim` (`0.4` by default, `0`–`0.9`) sets the
  opacity of the overlay's dim layer, so a bright or a dark desktop can have the
  strength it needs; `0` turns the layer off without turning the overlay off.
  Because the layer was the cue that made a 2 px divider visible, the line is
  drawn 4 px wide whenever no dim layer is created — no compositor, no SHAPE,
  or `dim 0` — so a boundary still reads on a bare X server. `overlay on` now
  reports all three outcomes: `dim 40% 0x…`, `dim off (dim 0)` and
  `dim unavailable`. The width is asserted by unit test and the opacity by the
  e2e under a stand-in compositor owner (0.6 → 60% opaque, `dim 0` → no layer).

### Changed

* **A panel keeps a unique id when X11 reuses a closed window's id (PR #3,
  Etothetaui).** The panel id is `p-<hex window id>`, and a tab group in the
  document can outlive the window whose id it was named after: a new client
  handed the recycled id used to adopt the old group's panel and its geometry.
  `free_panel_id` now picks `p-<id>-N` for the new seat and leaves the group
  alone.

* **The layout document gains `doc.tags` and `doc.active_tag`.** Both are
  `#[serde(default)]`, so a document written before tags loads unchanged (no
  tag = every view shown) and an older binary reads a tagged document by
  ignoring the keys — every view is still in `root`, so nothing is lost. This is
  a document-format change and therefore a deliberate `MINOR` bump
  ([docs/VERSIONING.md](docs/VERSIONING.md)).

* **The daemon waits on X11 and the IPC listener together (PR #3, Etothetaui).**
  The main loop used to block in `next_event(50)` and wake on the socket only
  through the 2 s reconcile pass. It now `poll`s the X connection and a wake
  socket, so an idle daemon sleeps until either side has work, a request is
  served without waiting for the next pass, and the X server is woken once per
  pass instead of on a fixed timer. Adoption, title refresh and the client-list
  watch are driven by the WM's `_NET_CLIENT_LIST` change instead of a 2 s poll.
  Measured in the contributor's pairs: settled RSS 2,620 -> 2,164 KiB, and this
  PR's release binary 1,709,000 -> 1,219,736 bytes on the same toolchain
  (`opt-level = "s"`, fat LTO, `strip`), with the e2e suite green on xfwm4 and
  openbox (219/219 and 215-217/217, three runs each on the merged tree).

* **`pergola-git`'s `pkgver()` was hard-coded to `0.4.0`** and did not follow
  the crate version, so the AUR package reported an older minor after a bump. It
  now reads `Cargo.toml` and strips `-dev` (pacman's `pkgver` cannot contain a
  hyphen); `makepkg --nobuild` reports `0.5.0.r41.g96f7d06`.

## [0.4.0-dev]

### Changed

* **`pergola-health --e2e` takes `--wm` and records it.** The regression suite has
  taken `--wm xfwm4|openbox|i3` since 0.4.0; the health check now forwards a WM
  too (`--wm openbox`), names it in the suite's summary line and adds `"wm"` to
  every `health.jsonl` record, so a green line cannot be mistaken for "measured
  on every desktop". The openbox run of the whole suite passes with no per-WM
  tolerance: the geometry check already compares each client's own resize
  increment (never under 2 px).

### Added

* **Pager activation requests are honoured.** A taskbar's "click to focus" sends
  `_NET_ACTIVE_WINDOW` to the root window; Pergola only ever *set* that property
  when it focused a window, so the click was a silent no-op. The daemon now
  selects the root's shared `SubstructureNotify` mask (it cannot take
  `SubstructureRedirect`, which the desktop owns) and handles the request: a
  window in the tree is focused and its seat recorded as last-focused; a window
  in a tab group that is not the shown view switches the tab first, so the
  desktop never focuses a window nobody can see; a window the daemon does not
  manage — a stale pager entry, an override-redirect popup — is ignored rather
  than focused off the tree. The daemon's own activation requests echo back
  through the same selection, so they are tracked for two seconds and consumed
  instead of re-handled, which would otherwise bounce focus between the layer
  and the desktop forever. Timestamps clients send as `0` are accepted. Unit
  tests cover the four targeting cases (shown view, hidden view, floating
  window, unmanaged id); the e2e asserts an external `xdotool windowactivate`
  changes the daemon's active window, that the daemon logged handling the
  request itself (an assertion on `status active` alone would pass with the
  handler deleted, because the desktop sets the property either way), and that
  a request for a hidden tab view shows it.

* **`pergola subscribe` — a push stream for status bars.** The control socket
  stops being request/response only: a client sends `subscribe` and the daemon
  pushes newline-delimited JSON — a state snapshot on connect (so a bar that
  starts late is not blind), then `dock`, `undock`, `window-opened`,
  `window-closed`, `layout` (carrying the new tree) and `focus` records. Events
  are diffs of *committed* state between event-loop passes, so the several
  mutations of one command collapse into one transition instead of a burst of
  phantom focus events, and nothing is emitted from a half-applied tree. Writes
  are non-blocking and per-client: a 256-record queue, a remembered
  partial-write offset (a plain `write_all` on a non-blocking socket accepts a
  prefix and then fails, which would resend the record and hand the client a
  duplicated line) and a ten-second idle drop, so a bar that stops reading
  cannot stall the window manager — measured at load 5.6, a subscriber whose
  stdout went to an undrained fifo left `pergola status` answering in 47 ms.
  When the daemon exits the client prints a note and exits 0, so a supervisor
  restarts it. The diff has its own unit tests (new window, dock, undock,
  close, layout, focus, no-change) and the e2e reads a whole session's stream,
  asserting every required record names the window.

* **`pergola undo` — one chord back from a wrong dock.** `Ctrl+Alt+Z` (`Super+Z`
  where the desktop owns the primary chord) or the palette row "Undo the last
  layout change" restores the layout from before the last change: a dock, a
  float, a drag out to the desktop, an edge drop, an `arrange`, a `ratio` or a
  gap drag. The ring keeps ten states, and a snapshot is the whole document (the
  tree, the floating rectangles, each panel's tab views) plus the gap and the
  camera — so undoing a zoomed change puts the camera back too, which is exactly
  what `back`/`forward` never did. A command that changed nothing is not
  recorded a second time, `undo` itself is never recorded (it would oscillate
  between two states), and a window that closed after the snapshot is dropped by
  the reconcile, so undo cannot resurrect a dead seat. `History<T>` lives in
  `pergola-core` with tests for the bound and the de-duplication. 97 unit tests,
  114 e2e assertions on xfwm4 and openbox; the e2e docks a window by chord,
  undoes it and checks the X server's geometry came back.

* **`pergola config` and `config --check`.** `pergola config` prints every key
  with the value actually in force and where it came from — `default`, or
  `config.json:<line>` — so a misspelled key can no longer hide behind a
  default. `pergola config --check` exits non-zero and names the line for an
  unknown key, an out-of-range `gap`/`minimum_share`/`desktop_share`, a
  `terminal` that is not on `PATH`, or a `bindings` chord that does not parse.
  It reads the file itself, so it works with no daemon running, which is exactly
  when a typo is most confusing. The line finder tracks JSON strings and nesting,
  so a binding named after a command does not shadow the key, and a one-line
  config still names its source. 86 unit tests (nine new), 101 e2e assertions on
  xfwm4 and openbox.

* **`pergola settings` — the config and chord editor.** `pergola settings` opens
  the same menu programs the action palette uses with one row per scalar key
  (`gap = 6`) and one per command's chords (`chord: dock left = Ctrl+Alt+Shift+Left,
  Super+Shift+Left`), so the chord map is what `keys --check` prints rather than a
  guess. Picking a row asks for a value with the current one prefilled and
  applies it through `pergola settings <key> <value>` — or `pergola bind
  <command> <chord>` for a chord row — which validates the whole candidate file
  with the same checks as `config --check`, writes atomically and reloads. Only
  problems the change *introduces* block it, so a pre-existing typo elsewhere
  does not make the file uneditable; `config --check` still reports that typo on
  its line. `settings --list` prints the rows for a script, and the menu path is
  proven in the e2e with a stand-in dmenu, so no GUI is needed. 93 unit tests,
  109 e2e assertions on xfwm4 and openbox.

* **The CLI no longer panics on a closed pipe.** `pergola config | head` died
  with `failed printing to stdout: Broken pipe` plus a Rust panic message,
  because Rust ignores `SIGPIPE`; `main` now restores the default handler, so a
  piped command ends silently like every other Unix tool.

* **The overlay dims the tiles when a compositor can render it.** `overlay on`
  now draws a full-screen 40% black layer under the split lines whenever a
  compositor owns `_NET_WM_CM_S0` — the selection xfwm4, picom, kwin and mutter
  all advertise themselves with — so the 2 px borders read at a glance instead
  of disappearing into the window contents. Measured under xfwm4's compositor:
  mean screen brightness 0.162 with the overlay off, 0.098 with it on, 0.159
  after `overlay off` — a 0.60x factor, which is what 40% black over the
  content predicts (the absolute value depends on what is on screen). The layer
  gets an empty SHAPE input region, so pointer events pass through to the tiles
  (checked at eight screen points: none landed on the dim window); on a server
  without SHAPE, or without a compositor, the overlay stays borders-only and
  `overlay on` says `dim unavailable` instead of
  claiming a layer it cannot draw. `overlay off` destroys the layer; 100 toggles
  leave the root window's child count unchanged and no window carrying
  `_NET_WM_WINDOW_OPACITY`. The e2e proves the fallback with no compositor, then
  starts a stand-in owner and asserts the layer's opacity, click-through and
  destruction (`scripts/xhelper.py`). About 90 assertions, run under both WMs.

* **An AUR package that builds from source.** `packaging/aur/PKGBUILD`
  (`pergola-git`) compiles from `main` and installs to system paths instead of
  your home: `/usr/bin/pergola`, `/usr/bin/pergola-health`,
  `/usr/lib/systemd/user/pergola.service`, the XDG autostart entry, LICENSE,
  NOTICE and the docs. `install.sh` stays the path for source installs. The
  file list mirrors what `install.sh` writes; the one difference is the config,
  which ships as an example in `/usr/share/doc/pergola-git/` because pacman must
  not write into `$HOME`. The packaged unit also drops the hard-coded
  `DISPLAY=:0` and inherits the graphical session's environment instead.
  Verified by building, installing with `pacman -U`, running `pergola doctor`
  green from `/usr/bin`, and removing the package again. The backlog asked for
  `pergola-bin`; that name means *prebuilt binary* in the AUR and no release
  artifact exists, so the honest name is `pergola-git` — the rename and the
  reason are in `packaging/aur/README.md`, and a `-bin` package is now its own
  backlog item, blocked on a release pipeline.

* **`auto_dock` is a tri-state (`off` / `new` / `all`).** `new` docks a window
  opened after the daemon started and leaves the session it found alone; `all`
  also adopts the windows that were already on screen at startup; `off` never
  adopts (the historical default). The old boolean still parses: `false` is
  `off`, `true` is `all`. The dock target is the focused panel only if it is in
  the tree, then the last-focused docked panel, else the whole tree, so the WM
  focusing the new window does not make it its own target. A `DestroyNotify`
  handler drops a dead window's seat immediately: the X server can reuse an XID
  within milliseconds, and on openbox the reused id made a new window look like
  the dead one, so it was never adopted (measured: `auto_dock: new` passed on
  xfwm4 and failed on openbox until the seat was removed on destroy). The unit
  test is the mode table; the e2e asserts a new window floats with `off` and
  docks with `new`. 85/85 on xfwm4 and openbox.

* **Tab groups work.** A panel can hold several windows as tabs: every view is
  mapped at the panel rectangle and only the selected one is raised. `pergola
  tab --window <id> [--target <panel>]` folds a window into a panel as a tab
  (a floating window is folded in from the desktop layer and docked);
  `tab-select <seat|window>` shows one view, `tab-next`/`tab-prev` cycle it
  (bound to `Ctrl+Alt+Page_Down`/`Page_Up`, falling back to `Super+Page_Down`/
  `Page_Up` when the desktop owns the primary). Switching a tab gives the shown
  window the keyboard, so typing goes to the view you can see. Closing the shown
  tab drops that view only — its siblings keep the panel — and promotes the
  neighbour; the group collapses only when the last view goes. The document
  model already had `views`/`selected`/`tabbed`; the daemon just never drove
  them. Unit tests cover selection repair and cycling (wrapping both ways); the
  e2e asserts against X geometry that both tabs are mapped at one rectangle,
  exactly one is raised, the default chord cycles the view, and the survivor
  keeps its tile after its sibling is killed.
* **Keybindings are configurable.** A `bindings` map in `config.json` overrides
  the default chords per command: `{"zoom-out": ["Super+Page_Up"]}` replaces
  that command's candidates, an empty list unbinds it, and commands you do not
  mention keep their defaults. Any command the daemon understands can be bound,
  not only the ones in the default plan (`view 3`, `gap 12`, `frame-desktop`).
  `pergola reload` now re-grabs the hotkeys, so a remap takes effect without a
  restart; `pergola keys --check` prints the chords actually grabbed, `off` for
  a deliberate unbind. Two unit tests cover the merge and the add/unbind paths.
  This is a MINOR bump: a new config key, per [docs/VERSIONING.md](docs/VERSIONING.md).
* **`pergola bind` and `pergola unbind` change a chord without editing JSON.**
  `pergola bind "gap 12" Super+9` writes `config.json` (preserving every other
  setting), reloads the daemon and prints its report, so a conflict appears where
  the command ran instead of only in the journal. `pergola bind <command>` with no
  chord shows the effective candidates and where they come from; `--default`
  removes the override. An unknown command or an unparseable chord is rejected
  before anything is written. A malformed `config.json` is never rewritten — the
  user's typo stays and the error is reported. Four unit tests cover preserve,
  unbind/restore, malformed-file refusal and directory creation.
* **`pergola ratio <0.0-1.0>` rebalances a split without a pointer.** It changes
  the split containing the focused panel and reports the resulting weights
  (`ratio 0.30 · split weights [0.30, 0.70]`); values outside 0–1 are refused.
  `LayoutEngine::set_ratio` clamps to `minimum_share`, so the result matches a
  gap drag of the same delta; the unit test checks the clamp against
  `resizing_gap`, covers a three-child split and rejects NaN. The e2e asserts the
  reported ratio and the new geometry.
* **Pointer affordances — the mouse paths say what they will do.** Hovering a gap
  band draws a 3 px line down its long axis, so a 6 px strip is findable without
  hunting with the cursor; the hint lives in its own override-redirect layer
  (tagged `_PERGOLA_HINT`) with an empty SHAPE input region, so it never takes
  the pointer away from the band and `overlay on/off` semantics are unchanged.
  Dragging a window within 70 px of a work-area edge draws a 4-bar frame around
  the rectangle the drop would produce; `preview_dock()` runs the same three
  steps as `dock_window` (choose the target, take the seat out of the tree,
  insert against the edge), so the frame cannot point where the window will not
  land. A docked tile dragged out of its own rectangle and settled near a
  different edge re-docks against that edge in one gesture, with no float step
  in between. Hints are skipped when the SHAPE extension is missing. Two daemon
  bugs surfaced while building it: the `same_rect` early-return in
  `note_configure` swallowed a real drag that landed back on the applied
  rectangle (a drag in progress now never skips), and `settle_moves()` cleared
  the hover hint on every loop pass — it now clears only the drop hint. 100 unit
  tests, 124 e2e assertions on xfwm4; the mid-drag drop frame is asserted on
  xfwm4 only, because openbox coalesces a drag into a single `ConfigureNotify`.

### Changed

* **`pergola reload` re-grabs the hotkeys.** It used to re-read the config and
  re-apply geometry without touching the grabs, so changing `hotkeys` or `bindings`
  needed a daemon restart. It now reinstalls the bindings, and `hotkeys: false` at
  reload releases the grabs so the desktop gets its chords back. `reload` now
  returns the grab report (fallbacks and failures) to the caller, so `pergola
  reload` prints it too.

## [0.3.4-dev]

### Added

* **`docs/FIRST-RUN.md` — a five-minute walkthrough for a new user.** Open two
  terminals, dock left and right, then see the rest of the model; a glossary for
  every term the `status` line prints; and a first-run troubleshooting table. The
  README now points to it from Install and from the tree model, and the mouse
  section no longer claims the palette needs no command line.

### Fixed

* `pergola keys --check` printed the *planned* chord for every command, so a
  desktop that owns the primary chord (xfwm4 owns `Ctrl+Alt+arrows`) made it
  claim `bound Ctrl+Alt+Up` while the daemon had actually bound `Super+Up`.
  It now prints the label the daemon really grabbed.
* The daemon announced a chord only when *both* the primary and fallback were
  taken; the README promises a report when a chord falls back to its secondary
  binding. It now prints `pergola: '<command>' bound to <chord> (<primary> is
  owned by the desktop)` at startup. — 2026-11-15
* **`pergola tree` kept a window's old title after a rename.** The `PropertyNotify`
  handler updated the daemon's own seat records but not the document metadata
  that `tree` serialises, so a title change only reached the tree view after the
  next 2 s reconcile or an `arrange`. `sync_meta()` now copies the live seats into
  the document before both `tree` and `save` (`herbstluftwm#35`, `bspwm#50`).
  Verified live: `xdotool set_window --name` then `pergola tree` shows the new
  title immediately.
* **A single-report WM drag is a drag.** Edge docking required three
  `ConfigureNotify` events, which xfwm4 emits and openbox does not (openbox sends
  one ICCCM-synthetic event at the drop position). `MoveWatch` now tracks the last
  applied rect as the drag baseline and treats one event that travelled >= 40 px
  from it as a drag, so a WM that reports only the drop position still docks.
  Two unit tests: `a_single_report_that_travelled_is_a_drag`,
  `a_placement_at_the_edge_is_not_a_drag`.
* **The own-apply guard shrank from 250 ms to 100 ms.** The guard swallows the
  stale `ConfigureNotify` a WM delivers after our move (openbox sends the
  pre-move position after we have already applied the new one; measured at
  ~0.5 ms after the apply). A geometric replacement was tried and failed — the
  stale echo is 803 px from the applied rect, so geometry cannot tell it from a
  real drag. 100 ms still catches the burst while shrinking the window in which
  a real drag can be swallowed 2.5×. Both WMs passed 43/43 three runs each.

### Changed

* **`scripts/e2e.sh` takes `--wm xfwm4|openbox|i3`** (or `PERGOLA_TEST_WM`). The
  "any EWMH desktop" claim is now measurable: the 1.0.0 gate wants every command
  under two window managers. The sandbox refuses to run on a display that already
  has a server, so a kept sandbox cannot masquerade as a product bug. The edge-dock
  test now performs a real pointer drag instead of `wmctrl -e` geometry pokes,
  because each WM coalesces those differently.
* **`scripts/health.sh --e2e` records live-only failures as warnings.** When two
  sessions tile the same live display, the soak's cpu/drift/orphan observations
  fail for reasons that are not the code's. With `--e2e`, those live-only
  observations are written to `health.jsonl` as `warnings` and no longer fail the
  exit code; the 12-hour soak runs without `--e2e` and keeps the strict verdict.

## [0.3.3-dev] — 2026-11-15

### Fixed

* **`pergola keys --check` said every chord was taken while the daemon ran.** The
  probe connected as a fresh client and tried to grab each chord itself, so it
  collided with the daemon's own grabs and reported `TAKEN` for a setup that was
  working. It now asks the running daemon what it actually bound (`keys` over
  IPC) and falls back to the live probe only when no daemon is up. Verified
  against the live daemon: `keys --check` prints `bound` for every chord and says
  it is reporting the daemon's grabs.

## [0.3.2-dev] — 2026-11-14

### Fixed

* **A held chord fires once, not eleven times.** The X server auto-repeats a
  held key (~11 presses a second); `back` held for a second would walk eleven
  history entries. Repeat detection is exact, not a time window: the server
  sends a `KeyRelease` and a `KeyPress` with the *same* timestamp for every
  repeat, so a press matching the preceding release's keycode and timestamp is
  dropped (`awesome#230`). Verified end-to-end by holding `back` for 1.2 s and
  asserting one step (`scripts/e2e.sh`, `PERGOLA_E2E_KEEP=1` keeps the sandbox
  for debugging).
* **A `Super+…` chord no longer dies when Num Lock is on.** The key-event path
  compared the raw modifier state against a mask of `Shift|Control|Mod1`, so a
  grab registered for `Mod4` never matched an event carrying `Mod4|Mod2`. It now
  masks the event state down to the four binding modifiers
  (`pergola_x11::event_mods`), so lock keys cannot break a chord
  (`bspwm#16`, `i3#943`, `spectrwm#103`).
* **Bindings survive a keyboard-layout switch.** Changing to a Cyrillic/Neo/Bone
  layout renumbers keycodes and left the old grabs pointing at the wrong keys.
  `MappingNotify` now triggers `reinstall_bindings()`, which rebuilds the keymap,
  ungrabs everything and re-grabs (`i3#79`, `awesome#157`, `awesome#219`).
* **Grabs are registered for every lock combination.** The Num Lock mask is read
  from the modifier map instead of assumed to be Mod2, and `grab_with_locks()`
  grabs each binding with and without every lock key held.
* **A terminal tile no longer ends up 11 px short of its slot.** Client sizes are
  now fitted to the `WM_NORMAL_HINTS` resize grid by `snap_to_hints()` before the
  frame is placed, so the tile the engine computed and the tile on screen agree
  (`awesome#200`, `awesome#197`).
* **A 0×0 work area no longer collapses the tree.** A RandR mid-change or a
  monitor asleep reports a zero-sized work area; `apply()` now returns early and
  retries on the next pass instead of dividing by zero (`i3#926`).
* **Fullscreen and maximised windows are left alone.** `WindowInfo` now carries
  `_NET_WM_STATE_FULLSCREEN` and the maximised flags, and `apply()`/`note_configure()`
  skip them, so entering fullscreen cannot be mistaken for an edge-dock drag
  (`awesome#245`, `awesome#203`).
* **Transient windows are never tiled.** `WM_TRANSIENT_FOR` is read into
  `WindowInfo.transient`; `reconcile()` will not adopt a dialog into the tree and
  `apply()` skips it, so a completion popup or a file chooser stays where its
  toolkit put it (`awesome#22`, `i3#787`).
* **A minimised window keeps its tile.** Liveness is now decided from
  `_NET_CLIENT_LIST` rather than from the mapped state, so minimising a window no
  longer collapses its tile; `pergola list` reports the seat as `hidden`
  (`i3#718`, `awesome#92`).
* **A window title or class change redraws immediately.** `reconcile()` now
  subscribes to `PROPERTY_CHANGE` and refreshes the title/class on
  `PropertyNotify`, so a browser tab switch or a terminal `cd` does not leave a
  stale label (`herbstluftwm#35`).

### Changed

* `scripts/e2e.sh` compares tile edges against the client's own resize increment
  rather than a fixed ±2 px, matching `scripts/health.sh`, so a snapped terminal
  is not reported as drift. `PERGOLA_E2E_KEEP=1` keeps the sandbox for debugging.

### Added

* `docs/HISTORY-LESSONS.md` — 700 issues from i3, awesomewm, bspwm, xmonad,
  herbstluftwm, qtile and spectrwm, classified into failure classes and mapped to
  the fix or the honest gap. `scripts/research-issues.py` regenerates the harvest
  from the GitHub REST API; `docs/research/` holds the digest and class listing.
* 22 unit tests (was 21) covering the repeat guard, plus one e2e assertion that a
  held chord acts once. Test counts are the measured values (64 unit, 43 e2e).

## [0.3.1-dev] — 2026-11-14

### Fixed

* Docking a **maximised** window silently did nothing: the WM ignores
  `_NET_MOVERESIZE_WINDOW` for a maximised window, so the tile kept the
  full-screen geometry while the tree recorded the tile. It now clears
  `_NET_WM_STATE_MAXIMIZED_{HORZ,VERT}` before the move; `scripts/e2e.sh`
  maximises a window, docks it by chord and asserts the state is gone.
* `pergola-health` geometry drift was wrong in both directions: full-height tiles
  were skipped (`y+h` compared against the work-area height instead of the work
  area's real bottom edge) so a maximised tile could drift unnoticed, and a client
  with resize increments (a terminal) was compared by its frame origin, which the
  WM is free to shift by one increment. It now skips against the work-area
  rectangle and compares the four tile edges with the client's own increment.
  Verified against the live case that prompted the fix.
* Test counts are the measured values (59 unit, 84 e2e).

## [0.3.0-dev] — 2026-11-14

### Added

* **Tiling without a terminal.** `Ctrl+Alt+Shift+←/→/↑/↓` docks the focused window
  left/right/above/below, `Ctrl+Alt+Shift+W` floats it, and `Ctrl+Alt+M` opens an
  action palette (`pergola palette`) that lists the same actions by human name
  through `rofi`, `zenity`, `yad` or `dmenu`. Two focused windows and two chords
  are a side-by-side tiling, with no shell involved.
* **Mouse tiling.** Every split gap is an invisible input-only grab strip with the
  matching resize cursor: drag it to move the boundary (clamped by
  `minimum_share`, exact under the camera transform). Dragging a floating window
  to a work-area edge docks it there (14 px slack), and dragging a docked tile
  away from the edge floats it where it was dropped. The edge behaviour is the
  `edge_dock` config key, default `true`.
* `LayoutEngine::gap_handles()` — the pointer-facing geometry of the tree: one
  band and one weight denominator per draggable boundary. Pure and unit-tested,
  so the daemon never re-derives layout math for the mouse.
* `scripts/e2e.sh` now covers keyboard docking, a real pointer drag on the gap
  and an edge drop followed by a drag-out (82 assertions).

### Changed

* `neighbour up` / `neighbour down` are no longer default chords: the
  `Ctrl+Alt+Shift+arrows` they used are the dock chords now. The command and its
  URL still work (`pergola neighbour up`).

### Fixed

* A docked window that the user drags is no longer left with the tree and the
  screen disagreeing: the pointer path floats it at the drop position, which is
  the inverse of drag-to-tile.

### Documentation

* `docs/UX-NOTES.md` records the human-interface pain points found while building
  this release, each with a severity and a proposed fix, so the next round starts
  from evidence instead of guesses.
* README, `docs/SETUP.md` and `docs/DESIGN.md` describe the keyboard/mouse/palette
  paths, the new config key and the pointer architecture; test counts are the
  measured values (59 unit, 82 e2e).

## [0.2.1-dev] — 2026-11-14

### Fixed

* Rectangles round their **edges**, not their sizes, so a row of tiles can no longer overflow its
  frame by one pixel (873 px split in two gave 434+6+434 = 874).
* `pergola-health` now asserts every tile is inside `_NET_WORKAREA` — the executable guard for the
  "tiled over the panel" class.
* A window that is entirely outside the framed view is parked below the work area instead of to the
  side, where most WMs clamp it back to keep a title-bar sliver reachable.
* The overlay no longer draws a boundary line for a zero-weight child (a collapsed desktop leaf).
* A second daemon no longer unlinks the live socket: it used to steal the socket from a running
  daemon, leaving that daemon alive but unreachable. It now refuses when the socket accepts a
  connection and only reclaims a stale file left by a killed daemon. `scripts/e2e.sh` asserts both
  halves (the refusal and that the first daemon keeps answering).
* `install.sh` writes the installing session's `$DISPLAY` into the systemd user unit instead of
  hard-coding `:0`, so the daemon also starts from a session that is not the first display.

### Documentation

* New [docs/SETUP.md](docs/SETUP.md): requirements, what `install.sh` writes and where, the config
  reference, start-at-login, upgrade, uninstall and troubleshooting. The README now links to it and
  carries the configuration keys inline.
* Test counts in the README and `docs/DESIGN.md` corrected to the measured values (46 unit tests,
  76 e2e assertions).

## [0.2.0-dev] — 2026-11-14

### Fixed

* Tiles use the work area (`_NET_WORKAREA` for the current desktop) instead of the whole display,
  so a top panel no longer pushes the bottom of every tile off-screen. The camera transform, and
  the desktop-to-world mapping for floating windows, now respect a work area that does not start
  at `0,0`; `pergola rects` reports frame coordinates that `pergola-health` can compare directly.
* A zero-weight child (a collapsed desktop leaf) no longer reserves a gap, so docking beside the
  desktop leaves no black strip at the screen edge.
* `pergola-health` tolerates a client's own resize increments (terminals snap to a character grid)
  instead of reporting them as drift.

### Changed

* **The overlay is a divider, not a border.** It now draws one 2 px line per split boundary,
  centred in the gap, plus a 3 px amber frame around the viewport when a node is framed. The old
  design drew a border around every tile, which sat on top of title bars and read as a debug
  artefact. Default is **off**; `overlay on` or `everything` shows it.
* Default gap 8 → **6 px**.
* Docking beside the desktop gives the docked window the whole space by default
  (`desktop_share = 0`); set `desktop_share` in the config to keep a visible desktop strip.
* The persisted `overlay` flag no longer overrides the config on startup.

## [0.1.0-dev] — 2026-11-14

### Added

* `scripts/health.sh` (installed as `pergola-health`): daemon liveness, CPU, layout invariants,
  geometry drift against the X server, orphaned docked windows, and an optional `--e2e` pass.
  Exits non-zero and appends one JSON line per run, so a scheduler can act on it.
* `BACKLOG.md`: the development queue, every item with a testable "done when".
* `scripts/e2e.sh` asserts that a floating window's geometry survives a zoom.

## [0.0.0-dev] — 2026-11-14

First working build, installed and tested on Arch Linux / XFCE (xfwm4) and in an
Xvfb + xfwm4 sandbox.

### Added

* `pergola-core`: recursive split tree (`Desktop`/`Panel`/`Split`), geometry with
  gaps, docking on four edges (same-axis sibling insert, cross-axis nesting,
  root wrapping), removal with split collapse, gap resizing with a minimum
  share, neighbour search, camera with back/forward history and nine saved
  views, command vocabulary and `pergola://` URLs, `version: 2` document with
  tolerant loading and weight repair.
* `pergola-x11`: EWMH client-list enumeration, frame-aware moves through
  `_NET_MOVERESIZE_WINDOW`, `_NET_ACTIVE_WINDOW`, override-redirect tree overlay,
  hotkey grabs with a `Super` fallback when the desktop owns a chord,
  `pergola doctor` diagnostics.
* `pergola` daemon: tree/window reconciliation (on demand and every 2 s),
  camera transform, desktop-leaf mapping for floating windows, layout
  persistence, Unix-socket IPC, CLI with 25 subcommands.
* `install.sh`, XDG autostart entry, systemd user unit.
* 37 unit tests; `scripts/e2e.sh` with 78 assertions on a real X server.

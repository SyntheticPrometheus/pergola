#!/usr/bin/env bash
# End-to-end test: a real X server (Xvfb), a real window manager, three real
# xterm windows, and the pergola daemon on top. Every assertion is made against
# geometry that the X server reports, not against the daemon's own bookkeeping
# alone.
#
#   scripts/e2e.sh                 uses target/release/pergola if present
#   scripts/e2e.sh --wm openbox    any of xfwm4 (default), openbox, i3
#   PERGOLA_BIN=... scripts/e2e.sh
#
# The WM is a parameter because the "any EWMH desktop" claim must be measured,
# not asserted: the 1.0.0 gate wants every command run under two window managers
# (docs/VERSIONING.md), and the summary line names the one that actually ran.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WM="${PERGOLA_TEST_WM:-xfwm4}"
while [ $# -gt 0 ]; do
  case "$1" in
    --wm) WM="${2:-}"; shift 2 ;;
    --wm=*) WM="${1#--wm=}"; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "e2e: unknown argument $1" >&2; exit 2 ;;
  esac
done
case "$WM" in
  xfwm4)   WM_CMD=(xfwm4 --compositor=off) ;;
  openbox) WM_CMD=(openbox) ;;
  i3)      WM_CMD=(i3 --no-config --shmlog-size 0) ;;
  *) echo "e2e: unknown window manager '$WM' (xfwm4|openbox|i3)" >&2; exit 2 ;;
esac
command -v "$WM" >/dev/null || { echo "e2e: $WM is not installed" >&2; exit 2; }
BIN="${PERGOLA_BIN:-$ROOT/target/release/pergola}"
[ -x "$BIN" ] || BIN="$ROOT/target/debug/pergola"
[ -x "$BIN" ] || { echo "build pergola first (cargo build)"; exit 1; }

DISP="${PERGOLA_TEST_DISPLAY:-:99}"
# A KEPT sandbox (PERGOLA_E2E_KEEP=1) or a stray daemon on the same display makes
# the next run fail in ways that look exactly like product bugs (measured
# 2026-11-15: half the geometry checks failed and blamed the engine, because a
# previous sandbox still owned :99). Refuse loudly instead.
x_alive() {
  # A dead display can leave xset waiting for a connection indefinitely.
  # Xvfb in this test always creates a local socket when it is ready.
  [ -S "/tmp/.X11-unix/X${DISP##*:}" ] || return 1
  if command -v xset >/dev/null; then timeout 3 xset -display "$DISP" q >/dev/null 2>&1; return $?; fi
  return 0
}
if [ -z "${PERGOLA_E2E_KEEP:-}" ] && x_alive; then
  echo "e2e: display $DISP is already in use (a KEPT sandbox?) — clean /tmp/pergola-e2e.*" >&2
  echo "     or run with PERGOLA_TEST_DISPLAY=:9x" >&2
  exit 2
fi
W=1600
H=900
TMP="$(mktemp -d /tmp/pergola-e2e.XXXXXX)"
SOCK="$TMP/sock"
export DISPLAY="$DISP"
export XDG_STATE_HOME="$TMP/state"
export XDG_CONFIG_HOME="$TMP/config"

PIDS=()
PASS=0
FAIL=0

say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  ok   %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$*"; }
fail() { bad "$*"; exit 1; }

check_eq() { # expected actual label
  if [ "$1" = "$2" ]; then ok "$3 ($2)"; else bad "$3: expected [$1] got [$2]"; fi
}
check_near() { # expected actual tolerance label
  # An empty operand means the geometry lookup returned nothing; a bare `$(( ))`
  # on that prints "arithmetic syntax error" and hides which assertion failed.
  # Report the missing value as a FAIL instead.
  case "$1" in ''|*[!0-9-]*) bad "$4: expected $1±$3, got [${2:-<empty>}]"; return;; esac
  case "$2" in ''|*[!0-9-]*) bad "$4: expected $1±$3, got [<empty>]"; return;; esac
  local d=$(( $1 > $2 ? $1 - $2 : $2 - $1 ))
  if [ "$d" -le "$3" ]; then ok "$4 (expected $1, got $2)"; else bad "$4: expected $1±$3, got $2"; fi
}
check_true() { # condition-cmd label
  if eval "$1" >/dev/null 2>&1; then ok "$2"; else bad "$2"; fi
}

cleanup() {
  "$BIN" --socket "$SOCK" stop >/dev/null 2>&1 || true
  for pid in "${PIDS[@]:-}"; do kill "$pid" >/dev/null 2>&1 || true; done
  sleep 0.3
  for pid in "${PIDS[@]:-}"; do kill -9 "$pid" >/dev/null 2>&1 || true; done
  # PERGOLA_E2E_KEEP=1 keeps the sandbox (daemon.log, xvfb.log) for debugging.
  if [ -n "${PERGOLA_E2E_KEEP:-}" ]; then
    printf 'e2e sandbox kept: %s\n' "$TMP"
  else
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------- sandbox ----
# Desktop notifications: `pergola settings` sends one when it rejects a value.
# Left alone, `notify-send` would talk to the *parent session's* bus and pop a
# toast on the real desktop during the test, and whether a window appears would
# depend on the machine. The stub records every message for the assertions and
# keeps the display untouched; it is also the user-facing override, not a hook.
cat > "$TMP/notify.sh" <<'EOS'
#!/bin/sh
printf '%s\n' "$*" >> "$PERGOLA_NOTIFY_LOG"
EOS
chmod +x "$TMP/notify.sh"
export PERGOLA_NOTIFY="$TMP/notify.sh"
export PERGOLA_NOTIFY_LOG="$TMP/notify.log"
say "pergola e2e: $BIN"
say "sandbox: Xvfb $DISP ${W}x${H} + $WM + 3 xterms"
Xvfb "$DISP" -screen 0 "${W}x${H}x24" >"$TMP/xvfb.log" 2>&1 & PIDS+=($!)
sleep 1
"${WM_CMD[@]}" >"$TMP/wm.log" 2>&1 & PIDS+=($!)
sleep 2
XPIDS=()
for i in 1 2 3; do
  xterm -T "pergola-e2e-$i" -geometry 60x18+$((60*i))+$((40*i)) \
    -e sh -c 'exec sleep 3600' >/dev/null 2>&1 & XPIDS+=($!)
done
sleep 2

# ---------------------------------------------------------------- daemon -----
"$BIN" --socket "$SOCK" daemon >"$TMP/daemon.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 50); do "$BIN" --socket "$SOCK" status >/dev/null 2>&1 && break; sleep 0.1; done
"$BIN" --socket "$SOCK" status >/dev/null 2>&1 || { cat "$TMP/daemon.log"; fail "daemon did not start"; }
P() { "$BIN" --socket "$SOCK" "$@"; }

# One daemon per display: a second one must refuse, not unlink the live socket.
if "$BIN" --socket "$SOCK" daemon >"$TMP/daemon2.log" 2>&1; then
  bad "a second daemon is refused"
else
  ok "a second daemon is refused"
fi
P status >/dev/null 2>&1 || fail "the second daemon stole the live socket"

# An XTerm XID exists before the WM maps it or Pergola adopts it. Wait for
# each exact fixture title and XID in both live lists before testing Dock.
deadline=$((SECONDS + 10))
while :; do
  wm_clients=$(timeout 1 wmctrl -l 2>&1)
  pergola_clients=$(timeout 1 "$BIN" --socket "$SOCK" list 2>&1)
  mapfile -t XWINS < <(awk '
    $1 ~ /^0x/ { wm[$NF] = tolower($1) }
    $1 == "floating" || $1 == "docked" || $1 == "tab" { seat[$NF] = tolower($2) }
    END {
      for (i = 1; i <= 3; i++) {
        title = "pergola-e2e-" i
        if (title in wm && wm[title] == seat[title]) print wm[title]
      }
    }' <<< "$wm_clients
$pergola_clients")
  [ "${#XWINS[@]}" -eq 3 ] && break
  if [ "$SECONDS" -ge "$deadline" ]; then
    say "expected pergola-e2e-1, pergola-e2e-2 and pergola-e2e-3"
    say "window manager clients: $wm_clients"
    say "Pergola clients: $pergola_clients"
    fail "the three named clients were not adopted"
  fi
  sleep 0.1
done
W1="${XWINS[0]}"
W2="${XWINS[1]}"
W3="${XWINS[2]}"
say "windows: $W1 $W2 $W3"

# frame geometry as the X server sees it: client position/size + frame extents
frame_geom() {
  local win="$1" pos size left right top bottom
  pos=$(xdotool getwindowgeometry "$win" | sed -n 's/.*Position: \([-0-9]*\),\([-0-9]*\).*/\1 \2/p')
  size=$(xdotool getwindowgeometry "$win" | sed -n 's/.*Geometry: \([0-9]*\)x\([0-9]*\).*/\1 \2/p')
  local e
  e=$(xprop -id "$win" _NET_FRAME_EXTENTS | sed -n 's/.*= \([0-9]*\), \([0-9]*\), \([0-9]*\), \([0-9]*\)/\1 \2 \3 \4/p')
  read -r left right top bottom <<<"${e:-0 0 0 0}"
  read -r px py <<<"$pos"
  read -r sw sh <<<"$size"
  # The window may be gone (a panel id inherited from a closed window). Say
  # nothing rather than feeding an empty operand to arithmetic.
  [ -n "${px:-}" ] && [ -n "${sw:-}" ] || return 0
  echo "$((px-left)) $((py-top)) $((sw+left+right)) $((sh+top+bottom))"
}

# The window a panel id actually maps to. A panel id is not always a window id:
# a tab group keeps the id of the panel it grew from and a docked window can
# take an id a closed one left free, so `p-40000c` may hold `s-80000c`. The
# panel's shown view is the window at its rectangle.
panel_window() { # panel-id -> 0x… window id (empty when unknown)
  P tree | python3 -c '
import json, sys
want = sys.argv[1]
def walk(n):
    if n.get("kind") == "panel" and n.get("id") == want:
        views = n.get("views") or []
        seat = n.get("selected") or (views[0] if views else "")
        print("0x" + seat[2:] if seat else "")
        return True
    for c in n.get("children", []):
        if walk(c):
            return True
    return False
walk(json.load(sys.stdin)["doc"]["root"])
' "$1"
}

# Compare every docked panel against `pergola rects`, which is the same engine
# output the apply path uses. Tolerance covers WM decorations rounding.
check_geometry() {
  local label="$1" checked=0 skipped=0 id x y w h
  while read -r id x y w h; do
    [ -z "${id:-}" ] && continue
    # Windows outside the framed view are parked off-screen; some WMs clamp
    # them to keep a sliver reachable, so only on-screen tiles are asserted.
    if [ "$x" -lt 0 ] || [ "$y" -lt 0 ] || [ $((x+w)) -gt "$W" ] || [ $((y+h)) -gt "$H" ]; then
      skipped=$((skipped+1)); continue
    fi
    local win fg
    win=$(panel_window "$id")
    [ -n "$win" ] || win="0x${id#p-}"
    fg=$(frame_geom "$win")
    if [ -z "$fg" ]; then
      bad "$label $id maps to window $win, which the X server does not have"
      checked=$((checked+1)); continue
    fi
    local ax ay aw ah; read -r ax ay aw ah <<<"$fg"
    # A client with a character grid (every terminal) snaps its size to
    # WM_NORMAL_HINTS resize increments, so the honest tolerance for a tile's
    # edges is its own increment, never less than 2 px. Compare edges rather
    # than origin+size: a snapped client can be one increment smaller while the
    # WM anchors it at the requested position, so the frame origin is exact but
    # the extent is not — and a real drift (a maximised window) is off by a
    # whole tile, far beyond one increment.
    local inc iw ih
    inc=$(xprop -id "$win" WM_NORMAL_HINTS 2>/dev/null \
      | sed -n 's/.*resize increment: \([0-9]*\) by \([0-9]*\).*/\1 \2/p')
    read -r iw ih <<<"${inc:-0 0}"
    [ "${iw:-0}" -gt 2 ] || iw=2
    [ "${ih:-0}" -gt 2 ] || ih=2
    local worst=0 d
    for d in $((ax-x)) $((ay-y)) $(((ax+aw)-(x+w))) $(((ay+ah)-(y+h))); do
      [ "${d#-}" -gt "$worst" ] && worst="${d#-}"
    done
    local tol=$iw; [ "$ih" -gt "$tol" ] && tol=$ih
    if [ "$worst" -le "$tol" ]; then
      ok "$label $id geometry ($x,$y ${w}x$h)"
    else
      bad "$label $id geometry: expected $x,$y ${w}x$h, got $ax,$ay ${aw}x$ah (edge drift $worst > $tol)"
    fi
    checked=$((checked+1))
  done < <(P rects)
  if [ "$checked" -eq 0 ]; then
    # An empty `rects` means the active tree has no panel: say what the daemon
    # and the document actually held, so the failure names its own cause.
    printf '  DIAG %s: rects=[%s] status=[%s]\n' "$label" "$(P rects)" "$(P status)" >&2
    P list >&2 2>/dev/null || true
    fail "$label: no panels to check"
  fi
  [ "$skipped" -eq 0 ] || say "       ($skipped panel(s) parked outside the framed view)"
}

json_field() { P tree | python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

# Poll for a condition instead of sleeping a fixed time. A blind sleep is an
# oracle that fails when the machine is loaded, and the failure then reads as a
# product bug: measured 2026-11-17, the same binary produced 4 failures on one
# run and 2 different ones on the next, all in fixed-sleep sections.
wait_until() { # wait_until SECONDS COMMAND...
  local deadline=$(( SECONDS + $1 ))
  shift
  while [ "$SECONDS" -lt "$deadline" ]; do
    "$@" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

# The daemon's own `status` line must name this window as active. Polling the
# daemon is the oracle; `xdotool windowactivate` is only the request.
active_is() { # active_is WINDOW_ID
  local want
  want=$(printf '0x%x' "$(( $1 ))" 2>/dev/null) || want="$1"
  [ "$(P status | sed -n 's/.*active \(0x[0-9a-f]*\).*/\1/p')" = "$want" ]
}

# How much of a window's client area is on the test screen, in percent. A parked
# window is moved below the work area, but the desktop clamps it so it cannot be
# lost: xfwm4 keeps the title bar, openbox keeps a strip of the client. "Mostly
# off-screen" is the honest assertion that is true on both, and a window left in
# place (100%) still fails it.
onscreen_pct() {
  # An iconified client keeps its former rectangle while its frame is invisible.
  if xwininfo -id "$1" 2>/dev/null | grep -Eiq 'Map State: Is(Unmapped|Unviewable)'; then
    echo 0
    return
  fi
  local X= Y= WIDTH= HEIGHT= WINDOW= SCREEN= top bottom vis
  eval "$(xdotool getwindowgeometry --shell "$1")"
  [ -n "${Y:-}" ] && [ -n "${HEIGHT:-}" ] || { echo 100; return; }
  top=$(( Y < 0 ? 0 : Y ))
  bottom=$(( Y + HEIGHT > H ? H : Y + HEIGHT ))
  vis=$(( bottom - top ))
  [ "$vis" -lt 0 ] && vis=0
  [ "$HEIGHT" -gt 0 ] || HEIGHT=1
  echo $(( vis * 100 / HEIGHT ))
}

# Park the pointer inside the first tile. A gap band under the pointer shows a
# hover hint, and a hint is a window of its own, so any check that counts root
# children must first take the pointer off every gap — otherwise the count
# depends on where the last mouse gesture happened to leave it.
park_pointer() {
  local _ x y w h
  read -r _ x y w h <<<"$(P rects | awk 'NR==1 {print $1, $2, $3, $4, $5}')"
  [ -n "${w:-}" ] || return 0
  xdotool mousemove $((x + w / 2)) $((y + h / 2))
  sleep 0.2
}

# ------------------------------------------------------------------ tests ----
say
say "1. dock three windows"
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.4
P dock left >/dev/null || fail "dock left"
xdotool windowactivate "$W2" >/dev/null 2>&1; sleep 0.4
P dock right >/dev/null || fail "dock right"
xdotool windowactivate "$W3" >/dev/null 2>&1; sleep 0.4
P dock down >/dev/null || fail "dock down"
sleep 0.4
PANELS=$(P list | grep -c '^docked')
check_eq "3" "$PANELS" "panels docked"
check_geometry "after dock"

say
say "2. geometry covers the screen with gaps, no overlaps"
P rects > "$TMP/rects.txt"
python3 - "$W" "$H" "$TMP/rects.txt" <<'PY'
import sys
w,h=int(sys.argv[1]),int(sys.argv[2])
rows=[l.split() for l in open(sys.argv[3]) if l.strip()]
rects=[(r[0],int(r[1]),int(r[2]),int(r[3]),int(r[4])) for r in rows]
bad=0
for i,(a,ax,ay,aw,ah) in enumerate(rects):
    if ax<0 or ay<0 or ax+aw>w or ay+ah>h: print(f"  FAIL {a} outside screen"); bad+=1
    for b,bx,by,bw,bh in rects[i+1:]:
        if ax<bx+bw and bx<ax+aw and ay<by+bh and by<ay+ah:
            print(f"  FAIL {a} overlaps {b}"); bad+=1
if bad: sys.exit(1)
print(f"  ok   {len(rects)} tiles inside {w}x{h} with no overlaps")
PY
[ $? -eq 0 ] && PASS=$((PASS+1)) || FAIL=$((FAIL+1))

say
say "3. zoom"
P float --window "$W3" >/dev/null || fail "float W3"
sleep 0.3
FLOAT_BEFORE=$(P list | awk '$1=="floating"{print $2, $3, $4}' | head -1)
[ -n "$FLOAT_BEFORE" ] || fail "no floating window to check"
P everything >/dev/null
xdotool windowactivate "$W2" >/dev/null 2>&1; sleep 0.4
P frame-window >/dev/null || fail "frame-window"
sleep 0.4
read -r fx fy fw fh <<<"$(P rects | awk -v p="p-$(printf '%x' "$W2")" '$1==p {print $2,$3,$4,$5}')"
check_near "0" "$fx" 2 "framed window x"
check_near "0" "$fy" 2 "framed window y"
check_near "$W" "$fw" 2 "framed window width"
check_near "$H" "$fh" 2 "framed window height"
# The floating window must come back to the size it had before the zoom: live
# rects seen while zoomed are screen-space and must not be adopted as world
# positions.
P zoom-out >/dev/null || fail "zoom-out"
# The restore is applied by the daemon, so poll the daemon's own answer rather
# than assuming 0.4 s is enough on a loaded machine.
FLOAT_AFTER=""
for _ in $(seq 1 30); do
  FLOAT_AFTER=$(P list | awk '$1=="floating"{print $2, $3, $4}' | head -1)
  [ -n "$FLOAT_AFTER" ] && break
  sleep 0.1
done
if [ -z "$FLOAT_AFTER" ]; then
  bad "the floating window is gone after a zoom (placements: $(P list | awk '{print $1}' | sort | uniq -c | tr '\n' ' '))"
else
  check_eq "$FLOAT_BEFORE" "$FLOAT_AFTER" "floating window geometry survives a zoom"
fi
check_geometry "after zoom-out"

say
say "4. saved views and history"
P view 2 >/dev/null 2>&1 && true
P save-view 5 >/dev/null || fail "save-view"
P everything >/dev/null
P back >/dev/null || fail "back"
P forward >/dev/null || fail "forward"
P view 5 >/dev/null || fail "view"
FRAMED=$(json_field 'd["camera"]["framed"]')
[ -n "$FRAMED" ] && ok "camera restored to $FRAMED" || bad "camera not restored"

say
say "5. arrange / gap / float-all"
P arrange >/dev/null || fail "arrange"
sleep 0.3
check_geometry "after arrange"
DEPTH=$(json_field 'd["doc"]["root"].get("kind") == "split" and len(d["doc"]["root"]["children"]) or 0')
[ "$DEPTH" -ge 2 ] && ok "arranged tree has $DEPTH top-level children" || bad "arrange produced depth $DEPTH"
P gap 24 >/dev/null || fail "gap"
sleep 0.3
check_geometry "after gap 24"
# A ratio rebalances the split that holds the focused window; the reply shows
# the resulting weights so the change is observable without a UI.
P float-all >/dev/null; sleep 0.4
P arrange >/dev/null; sleep 0.4
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.3
RATIO_OUT=$(P ratio 0.3 2>&1)
case "$RATIO_OUT" in
  *"ratio 0.30"*) ok "ratio rebalances the focused split" ;;
  *) bad "ratio did not report a change: $RATIO_OUT" ;;
esac
# A ratio outside 0..1 is refused before it touches the tree.
if P ratio 1.5 >/dev/null 2>&1; then
  bad "ratio accepted 1.5"
else
  ok "ratio refuses a value outside 0..1"
fi
check_geometry "after ratio 0.3"
# The same rebalance is one key away, without a pointer and without knowing the
# split's current size: the default bindings carry `ratio +0.05` / `ratio -0.05`.
# A chord's reply goes to the journal, so the window the chord will act on is
# the assertion — and it is read from `status`, not assumed to be W1: an apply
# raises each panel in tree order, so the desktop can move focus after a layout
# change (observed on xfwm4). Pressing the chord does not move focus before it
# acts, so the active window at press time is the one whose share changes.
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.4
NUDGE_WIN=$(P status | sed -n 's/.*active \(0x[0-9a-f]*\).*/\1/p')
[ -n "$NUDGE_WIN" ] || fail "no active window to nudge"
eval "$(xdotool getwindowgeometry --shell "$NUDGE_WIN")"; NUDGE_W=$WIDTH
NUDGE=$(P keys | awk '$NF == "+0.05" {print $1; exit}')
[ -n "$NUDGE" ] || fail "no chord is bound to ratio +0.05"
NUDGE_KEY=$(printf '%s' "$NUDGE" | sed -e 's/Ctrl/ctrl/' -e 's/Alt/alt/' -e 's/Shift/shift/' -e 's/Super/super/')
# No `--clearmodifiers`: it wraps the chord in synthetic modifier traffic, and
# the other chord assertions press the keys plainly for the same reason.
xdotool key "$NUDGE_KEY"; sleep 0.6
eval "$(xdotool getwindowgeometry --shell "$NUDGE_WIN")"
if [ -n "${NUDGE_W:-}" ] && [ "$WIDTH" -gt "$NUDGE_W" ]; then
  ok "the ratio nudge chord ($NUDGE) widens the focused split ($NUDGE_W -> $WIDTH)"
else
  bad "the ratio nudge chord ($NUDGE) did not widen the split ($NUDGE_W -> ${WIDTH:-?})"
fi
# The palette is built from the focused window's state, not a fixed list: a
# floating window gets "Fold this window into a tab" instead of the no-op
# "Float this window", and the ratio rows are there. `--list` prints exactly the
# rows the menu would show, so this needs no menu program.
P float --window "$W1" >/dev/null; sleep 0.3
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.4
PAL=$(P palette --list)
if printf '%s' "$PAL" | grep -c '^Float this window' >/dev/null; then
  bad "the palette offers Float for a window that is already floating"
else
  ok "the palette hides Float for a floating window"
fi
if printf '%s' "$PAL" | grep -c '^Fold this window into a tab' >/dev/null; then
  ok "the palette offers folding a floating window into a tab"
else
  bad "no Fold row for a floating window: $(printf '%s' "$PAL" | head -3)"
fi
if printf '%s' "$PAL" | grep -c '^Split this split half and half' >/dev/null; then
  ok "the palette has a half-and-half ratio row"
else
  bad "no half ratio row in the palette"
fi
P dock left --window "$W1" >/dev/null; sleep 0.4
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.3
PAL=$(P palette --list)
if printf '%s' "$PAL" | grep -c '^Float this window' >/dev/null; then
  ok "the palette offers Float for a docked window"
else
  bad "the palette lost the Float row for a docked window"
fi
if printf '%s' "$PAL" | grep -c '^Fold this window into a tab' >/dev/null; then
  bad "the palette offers Fold for a window that is already tiled"
else
  ok "the palette hides Fold for a docked window"
fi
P float-all >/dev/null || fail "float-all"
sleep 0.3
check_eq "desktop" "$(json_field 'd["doc"]["root"]["id"]')" "float-all returns to the desktop leaf"
check_eq "3" "$(P list | grep -c '^floating')" "three floating windows"

say
say "6. persistence across a daemon restart"
P arrange >/dev/null; sleep 0.2
BEFORE=$(json_field 'json.dumps(d["doc"]["root"], sort_keys=True)')
P stop >/dev/null 2>&1
sleep 0.5
"$BIN" --socket "$SOCK" daemon >>"$TMP/daemon.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 50); do P status >/dev/null 2>&1 && break; sleep 0.1; done
AFTER=$(json_field 'json.dumps(d["doc"]["root"], sort_keys=True)')
check_eq "$BEFORE" "$AFTER" "tree survived restart"
sleep 0.3
check_geometry "after restart"

say
say "7. overlay and hotkeys"
P overlay on >/dev/null || fail "overlay on"
sleep 0.3
check_eq "on" "$(P status | sed -n 's/.*overlay \(on\|off\).*/\1/p')" "overlay is on"
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.3
P everything >/dev/null
P frame-window >/dev/null; sleep 0.3
xdotool key ctrl+alt+space; sleep 0.5
FRAMED=$(json_field 'd["camera"]["framed"]')
ROOT=$(json_field 'd["doc"]["root"]["id"]')
check_eq "$ROOT" "$FRAMED" "Ctrl+Alt+Space frames everything"
P overlay off >/dev/null

# The dim layer is only honest with a compositor: the daemon reads the
# _NET_WM_CM_S<screen> selection owner, which is how xfwm4, picom, kwin and
# mutter advertise themselves. The sandbox runs `xfwm4 --compositor=off`, so the
# fallback is what runs by default; a stand-in owner then proves the decision,
# the opacity and the layer's lifetime. Whether the layer is really *rendered*
# is the compositor's job and was measured against xfwm4's own compositor (mean
# screen brightness 0.168 -> 0.104 — docs/UX-NOTES.md).
XH="$(dirname "$0")/xhelper.py"
check_eq "0" "$(python3 "$XH" cm-owner)" "no compositor owns _NET_WM_CM_S0 in the sandbox"
OUT=$(P overlay on)
if printf '%s' "$OUT" | grep -c 'dim unavailable' >/dev/null; then
  ok "overlay without a compositor draws borders only"
  # The divider is a real window, so its width is a server fact. With no dim
  # layer the line has to carry the cue alone, so it is drawn 4 px wide on its
  # thin axis instead of 2 (a 2 px line in a 6 px gap all but disappears).
  BARS=$(python3 "$XH" bars)
  if [ -n "$BARS" ]; then
    THIN=$(printf '%s\n' "$BARS" | awk '{split($2, d, "x"); print (d[1] < d[2] ? d[1] : d[2])}' | sort -u | paste -sd, -)
    check_eq "4" "$THIN" "no-dim overlay lines are 4 px wide"
    if [ "$(printf '%s\n' "$BARS" | wc -l | tr -d ' ')" -ge 1 ]; then
      ok "overlay drew a divider per split gap"
    else
      bad "overlay drew no divider"
    fi
  else
    bad "overlay on drew no divider windows"
  fi
else
  bad "overlay claimed a dim layer without a compositor: $OUT"
fi
P overlay off >/dev/null
park_pointer
check_eq "0" "$(python3 "$XH" bars | wc -l | tr -d ' ')" "overlay off leaves no divider windows"
park_pointer
CHILDREN_BASE=$(python3 "$XH" children)
DIMS_BASE=$(python3 "$XH" dims)
python3 "$XH" own-cm >/dev/null 2>&1 & COMPOSITOR=$!
PIDS+=("$COMPOSITOR")
for _ in $(seq 1 50); do [ "$(python3 "$XH" cm-owner)" != "0" ] && break; sleep 0.1; done
OUT=$(P overlay on)
DIM=$(printf '%s' "$OUT" | sed -n 's/.*dim 40% \(0x[0-9a-f]*\).*/\1/p')
if [ -n "$DIM" ]; then
  ok "overlay on draws a dim layer under a compositor"
  check_eq "1717986918" "$(xprop -id "$DIM" -notype _NET_WM_WINDOW_OPACITY 2>/dev/null | sed 's/.*= //')" "the dim layer is 40% opaque"
  THIN=$(python3 "$XH" bars | awk '{split($2, d, "x"); print (d[1] < d[2] ? d[1] : d[2])}' | sort -u | paste -sd, -)
  check_eq "2" "$THIN" "with a dim layer the lines are 2 px wide"
  xdotool mousemove 400 300; sleep 0.2
  if [ "$(xdotool getmouselocation --shell | sed -n 's/^WINDOW=//p')" = "$((DIM))" ]; then
    bad "the dim layer swallowed the pointer"
  else
    ok "the dim layer passes pointer events through to the tiles"
  fi
  P overlay off >/dev/null
  if xprop -id "$DIM" -notype _NET_WM_WINDOW_OPACITY >/dev/null 2>&1; then
    bad "the dim layer survived overlay off"
  else
    ok "overlay off destroys the dim layer"
  fi
else
  bad "overlay on drew no dim layer with a compositor owner: $OUT"
fi
# The strength is a config key, not a constant: 0.6 is 60% opaque and 0 turns
# the layer off entirely (the split lines then carry the cue on their own).
P settings dim 0.6 >/dev/null || fail "settings dim 0.6"
OUT=$(P overlay on)
DIM=$(printf '%s' "$OUT" | sed -n 's/.*dim 60% \(0x[0-9a-f]*\).*/\1/p')
if [ -n "$DIM" ]; then
  check_eq "2576980377" "$(xprop -id "$DIM" -notype _NET_WM_WINDOW_OPACITY 2>/dev/null | sed 's/.*= //')" "dim 0.6 is 60% opaque"
  P overlay off >/dev/null
else
  bad "dim 0.6 did not report 60%: $OUT"
fi
P settings dim 0 >/dev/null || fail "settings dim 0"
OUT=$(P overlay on)
if printf '%s' "$OUT" | grep -c 'dim off (dim 0)' >/dev/null; then
  ok "dim 0 reports the layer as off"
else
  bad "dim 0 did not report the layer as off: $OUT"
fi
park_pointer
check_eq "$DIMS_BASE" "$(python3 "$XH" dims)" "dim 0 draws no dim layer"
P overlay off >/dev/null
P settings dim 0.4 >/dev/null || fail "settings dim 0.4"
for _ in $(seq 1 100); do P overlay on >/dev/null; P overlay off >/dev/null; done
park_pointer
check_eq "$DIMS_BASE" "$(python3 "$XH" dims)" "100 overlay toggles leave no dim window"
check_eq "$CHILDREN_BASE" "$(python3 "$XH" children)" "100 overlay toggles leak no windows"
kill "$COMPOSITOR" 2>/dev/null
wait "$COMPOSITOR" 2>/dev/null

say
say "8. keyboard and mouse tiling"
P float-all >/dev/null
# float-all is a transition, not a request the daemon can answer in one pass:
# under load it can still be undocking when the next assertion counts panels.
# Wait for the count to reach zero instead of guessing a sleep.
for _ in $(seq 1 40); do
  [ "$(P list | grep -c '^docked')" = "0" ] && break
  sleep 0.1
done
# A maximised window ignores _NET_MOVERESIZE_WINDOW, so docking has to clear
# that state first or the tile silently keeps the full-screen geometry.
wmctrl -i -r "$W1" -b add,maximized_vert,maximized_horz >/dev/null 2>&1; sleep 0.6
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.4
xdotool key ctrl+alt+shift+Left; sleep 0.8
if xprop -id "$W1" _NET_WM_STATE | grep -q MAXIMIZED; then
  bad "docking a maximised window left it maximised"
else
  ok "docking a maximised window clears the state"
fi
for _ in $(seq 1 40); do
  [ "$(P list | grep -c '^docked')" = "1" ] && break
  sleep 0.1
done
check_eq "1" "$(P list | grep -c '^docked')" "the maximised window is docked"
P float-all >/dev/null; sleep 0.4
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.4
xdotool key ctrl+alt+shift+Left; sleep 0.5
check_eq "1" "$(P list | grep -c '^docked')" "Ctrl+Alt+Shift+Left docks the focused window"
xdotool windowactivate "$W2" >/dev/null 2>&1; sleep 0.4
xdotool key ctrl+alt+shift+Right; sleep 0.5
check_eq "2" "$(P list | grep -c '^docked')" "Ctrl+Alt+Shift+Right docks beside it"
read -r GX GW1 GW2 <<<"$(P rects | awk 'NR==1 {x=$2; w=$4} NR==2 {print x+w+($2-(x+w))/2, w, $4; exit}')"
[ -n "${GX:-}" ] || fail "no draggable gap between the two tiles"
check_near "$GW1" "$GW2" 2 "the two tiles split evenly before the drag"
xdotool mousemove "$GX" 400; sleep 0.2
xdotool mousedown 1; sleep 0.1
for x in $((GX+30)) $((GX+70)) $((GX+120)); do xdotool mousemove "$x" 400; sleep 0.05; done
xdotool mouseup 1; sleep 0.5
NEW_W1=$(P rects | awk 'NR==1 {print $4}')
if [ "$NEW_W1" -gt $((GW1+80)) ]; then ok "dragging the gap resizes the split ($GW1 -> $NEW_W1)"; else bad "gap drag did not resize ($GW1 -> $NEW_W1)"; fi

# A gap lights up under the pointer even with the overlay off: the strip is only
# as wide as the gap and nothing else says it can be dragged (UX-NOTES row 1).
# The hint is its own layer, never the overlay, and it takes its window away
# when the pointer leaves.
read -r HX _ _ <<<"$(P rects | awk 'NR==1 {x=$2; w=$4} NR==2 {print x+w+($2-(x+w))/2, w, $4; exit}')"
xdotool mousemove $((HX + 250)) 400; sleep 0.3
# Capture the status first: `P status | grep -c` under `set -o pipefail` fails
# when the pipeline does, and a failing pipeline in the `if` condition takes the
# negative branch — which would report a false pass.
STATUS=$(P status) || bad "status failed while checking for a hint"
if grep -c 'hint' <<<"$STATUS" >/dev/null; then bad "a hint appeared with the pointer on a tile"; else ok "no hint with the pointer on a tile"; fi
xdotool mousemove "$HX" 400; sleep 0.3
STATUS=$(P status)
if grep -c 'hint hover' <<<"$STATUS" >/dev/null; then ok "a hovered gap reports a hint"; else bad "a hovered gap reported no hint: $STATUS"; fi
# The hint is one window of its own, tagged on the server, so a test can see
# it without trusting the daemon's own bookkeeping.
check_eq "1" "$(python3 "$XH" hints)" "the hover hint is one tagged window"
xdotool mousemove $((HX + 250)) 400; sleep 0.3
STATUS=$(P status)
if grep -c 'hint' <<<"$STATUS" >/dev/null; then bad "the hint stayed after the pointer left the gap"; else ok "leaving the gap removes the hint"; fi
check_eq "0" "$(python3 "$XH" hints)" "the hover hint leaves no window behind"

# While a window is dragged, a frame shows the rectangle it will land in. The
# grab is the window's middle: grabbing 12 px from its right edge gives ~9 px of
# travel to the screen edge, which the daemon's 40 px drag threshold never sees
# (measured 2026-11-15: openbox dropped the gesture, so the window never moved).
P float --window "$W3" >/dev/null 2>&1; sleep 0.4
eval "$(xdotool getwindowgeometry --shell "$W3")"
GRABX=$((X + WIDTH / 2))
xdotool windowactivate "$W3" >/dev/null 2>&1; sleep 0.3
xdotool mousemove "$GRABX" $((Y + 12)) sleep 0.15 keydown alt sleep 0.1 mousedown 1 sleep 0.15
for x in 1200 1400 1520 1592; do xdotool mousemove "$x" $((Y + 12)); sleep 0.12; done
# The frame is four override-redirect windows tagged on the server, so the
# assertion reads the screen rather than the daemon's own status text.
#
# Only xfwm4 streams ConfigureNotify during a drag. Openbox reports the gesture
# as one event at the end, so the daemon has nothing to draw a frame *from* —
# the daemon cannot know a window is being dragged until the WM says it moved.
# That is a WM capability, not a tolerance: the frame is asserted where the
# events exist, and the drop itself is asserted on both.
if [ "$WM" = "xfwm4" ]; then
  saw_hint=0
  for _ in $(seq 1 15); do
    if [ "$(python3 "$XH" hints)" = 4 ]; then saw_hint=1; break; fi
    sleep 0.1
  done
  STATUS=$(P status)
  if [ "$saw_hint" = 1 ]; then ok "a dragged window shows its drop target (4 frame bars on the server)"; else bad "no drop target during a drag: $(python3 "$XH" hints) hint windows, status: $STATUS"; fi
  if grep -c 'hint drop' <<<"$STATUS" >/dev/null; then ok "the daemon reports the drop hint"; else bad "the daemon did not report the drop hint: $STATUS"; fi
else
  say "       (drop frame not asserted on $WM: it reports one ConfigureNotify per drag)"
fi
xdotool mouseup 1 sleep 0.1 keyup alt
sleep 1.6
check_eq "0" "$(python3 "$XH" hints)" "the drop hint leaves no window behind"
if P list | grep -c "^docked.*$W3" >/dev/null; then ok "a window dropped on the right edge docks"; else bad "the drop did not dock $W3"; fi

# A docked tile dragged to the opposite edge re-docks against it in one
# gesture, instead of the old float-then-dock two-step (UX-NOTES row 7).
eval "$(xdotool getwindowgeometry --shell "$W3")"
GRABX=$((X + WIDTH - 12))
xdotool windowactivate "$W3" >/dev/null 2>&1; sleep 0.3
xdotool mousemove "$GRABX" $((Y + 12)) sleep 0.15 keydown alt sleep 0.1 mousedown 1 sleep 0.15
for x in $((GRABX - 200)) 900 400 60 2; do xdotool mousemove "$x" $((Y + 12)); sleep 0.1; done
xdotool mouseup 1 sleep 0.1 keyup alt
sleep 2
eval "$(xdotool getwindowgeometry --shell "$W3")"
check_near 0 "$X" 8 "a docked tile dragged to the opposite edge re-docks on the left"
if P list | grep -c "^docked.*$W3" >/dev/null; then ok "the re-docked tile is still docked"; else bad "the re-docked tile floated"; fi

# Mouse tiling: *drag* a floating window to the left screen edge and let go.
# The gesture, not a geometry poke: wmctrl -e is interpreted per WM (openbox
# coalesces it into one ConfigureNotify where xfwm4 emits a stream, and it
# re-places the window at 0,0 — the top-left corner — which the daemon then
# reads as a real drag to the left edge and docks before the gesture starts).
# The daemon's edge detection exists for a real drag, so the test must be one,
# starting from wherever the window already floats.
P float --window "$W3" >/dev/null 2>&1; sleep 0.4
EDGE_BEFORE=$(P list | awk '{print $1,$2}' | sort)
eval "$(xdotool getwindowgeometry --shell "$W3")"   # X, Y, WIDTH, HEIGHT
xdotool windowactivate "$W3" >/dev/null 2>&1; sleep 0.3
xdotool mousemove $((X + WIDTH / 2)) $((Y + 12)) sleep 0.15 \
  keydown alt sleep 0.1 mousedown 1 sleep 0.15
for x in 700 500 300 120 20 2; do xdotool mousemove "$x" $((Y + 12)); sleep 0.08; done
xdotool mouseup 1 sleep 0.1 keyup alt
sleep 1.5
if P list | grep -q "^docked.*$W3"; then ok "a window dragged to the left edge is docked"; else bad "edge drag did not dock $W3"; fi
# Undo immediately after a mouse drop must keep the restored layout. A stale
# movement watch plus the WM's one-pixel apply echo once re-floated another tile.
P undo >/dev/null; sleep 2.5
if [ "$EDGE_BEFORE" = "$(P list | awk '{print $1,$2}' | sort)" ]; then
  ok "undo after an edge drop stays restored"
else
  bad "undo after an edge drop changed the restored layout"
fi
P dock left --window "$W3" >/dev/null; sleep 0.4
# Pulling that tile back off the screen edge floats it where it was dropped.
for x in 320 400 480; do wmctrl -i -r "$W3" -e "0,$x,220,320,240" >/dev/null 2>&1; sleep 0.15; done
sleep 2.5
if P list | grep -q "^floating.*$W3"; then ok "a docked tile dragged away floats again"; else bad "drag-out did not float $W3"; fi
P arrange >/dev/null; sleep 0.4

# Holding a chord must act once. The X server auto-repeats a held key ~11
# times a second, so a bound command would race ("back" held for a second
# would walk eleven history entries). The daemon latches the chord on press
# and clears it on release, so a hold is one press.
to_xdotool() { # binding label -> xdotool key spec
  # Lowercase only the modifier tokens: xdotool takes "ctrl" or "Ctrl" but
  # needs the key name exactly as X spells it. Lowercasing the whole label
  # turned "Page_Down" into "page_down", which xdotool rejects, so the chord
  # silently never fired.
  printf '%s' "$1" | awk -F+ '{
    n = NF; out = "";
    for (i = 1; i < n; i++) out = out tolower($i) "+";
    key = $n;
    if (key == "[") key = "bracketleft";
    if (key == "]") key = "bracketright";
    print out key;
  }'
}
chord_for() { # command -> xdotool key spec, from the bindings actually installed
  local label
  label=$(P keys | awk -v c="$1" '$2==c {print $1; exit}')
  [ -n "$label" ] || return 1
  to_xdotool "$label"
}
command_chord() { # a multi-word command ("tag-view 1") -> xdotool key spec
  local label
  # The daemon pads the label to a column and then prints the command, which
  # may contain a space, so match the tail of the line instead of a field.
  label=$(P keys | awk -v c="$1" '
    { n = length($0); if (substr($0, n - length(c) + 1) == c) { print $1; exit } }')
  [ -n "$label" ] || return 1
  to_xdotool "$label"
}
build_history() {
  P everything >/dev/null; sleep 0.4
  for w in "$W1" "$W2" "$W3"; do
    xdotool windowactivate "$w" >/dev/null 2>&1; sleep 0.35
    P frame-window >/dev/null; sleep 0.35
  done
}
BACK=$(chord_for back) || fail "back is not bound"
build_history
PRE=$(json_field 'd["camera"]["framed"]')
xdotool key "$BACK"
# The hold below is only meaningful if one tap moved the camera: without this
# precondition a camera history that cannot go back reads as "the held chord
# did nothing" (measured 2026-11-19, #231).
ONE=""
for _ in $(seq 1 40); do
  ONE=$(json_field 'd["camera"]["framed"]')
  [ -n "$ONE" ] && [ "$ONE" != "$PRE" ] && break
  sleep 0.1
done
TAP_MOVED=1
if [ -z "$ONE" ] || [ "$ONE" = "$PRE" ]; then
  TAP_MOVED=0
  bad "one back tap did not move the camera (still $PRE), so the hold cannot be measured"
else
  ok "one back tap moves the camera ($PRE -> $ONE)"
fi
build_history
xdotool keydown "$BACK"
# Hold until the camera actually moves (up to 3 s), then release and let the
# server's repeats settle. Fixed 1.2 s/0.6 s windows read as "the chord did
# nothing" on a loaded machine.
for _ in $(seq 1 30); do
  [ "$(json_field 'd["camera"]["framed"]')" != "$PRE" ] && break
  sleep 0.1
done
xdotool keyup "$BACK"
sleep 0.2
HELD=""
for _n in $(seq 1 20); do
  HELD=$(json_field 'd["camera"]["framed"]')
  [ "$HELD" = "$ONE" ] && break
  sleep 0.1
done
[ -n "$HELD" ] || HELD=$(json_field 'd["camera"]["framed"]')
# The hold must move the camera (not be dropped) and move it exactly as far as
# one deliberate tap — the server's ~11 repeats must not each step back.
if [ "$TAP_MOVED" = 0 ]; then
  say "       (hold check skipped: the tap it is compared against never moved)"
elif [ "$HELD" = "$PRE" ]; then bad "a held chord did nothing (still $PRE)"; else
  check_eq "$ONE" "$HELD" "holding a chord goes back once, not eleven times"
fi

# Undo is a layout history, not the camera's: a keyboard dock can be taken back
# to the exact tree, floating rects and geometry it replaced.
P arrange >/dev/null; sleep 0.4
BEFORE_TREE=$(json_field 'json.dumps(d["doc"]["root"], sort_keys=True)')
BEFORE_FLOAT=$(json_field 'len(d["doc"]["floating"])')
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.4
xdotool key ctrl+alt+shift+Left; sleep 0.8
AFTER_TREE=$(json_field 'json.dumps(d["doc"]["root"], sort_keys=True)')
if [ "$BEFORE_TREE" = "$AFTER_TREE" ]; then bad "a keyboard dock left the tree unchanged"; else ok "a keyboard dock changed the tree"; fi
UNDO=$(P undo); echo "       ($UNDO)"
GOT_TREE=$(json_field 'json.dumps(d["doc"]["root"], sort_keys=True)')
# A mismatch has two causes and they are not the same defect: the undo restored
# the layout the dock produced (an undo entry that undoes nothing), or it
# restored some third state. Name which one was seen instead of only printing
# two JSON trees.
if [ "$GOT_TREE" = "$BEFORE_TREE" ]; then
  ok "undo restores the tree"
elif [ "$GOT_TREE" = "$AFTER_TREE" ]; then
  bad "undo restored the docked tree: the dock left an undo entry that restores the state already on screen (a pending drag watch, or two entries for one action)"
else
  bad "undo restored a third tree: $GOT_TREE"
fi
check_eq "$BEFORE_FLOAT" "$(json_field 'len(d["doc"]["floating"])')" "undo restores the floating rects"
check_geometry "after undo"
for _ in $(seq 1 20); do P undo >/dev/null 2>&1; done
if P undo | grep -c 'nothing to undo' >/dev/null; then ok "undo with an empty history says so"; else bad "undo did not report an empty history"; fi

say
say "9. config validation and error paths"
if "$BIN" --socket "$SOCK" dock sideways >/dev/null 2>&1; then bad "a bad edge is rejected"; else ok "a bad edge is rejected"; fi
if "$BIN" --socket "$SOCK" view 0 >/dev/null 2>&1; then bad "a bad view slot is rejected"; else ok "a bad view slot is rejected"; fi
if "$BIN" sim --json | python3 -c 'import json,sys; json.load(sys.stdin)' >/dev/null; then ok "sim --json is valid JSON"; else bad "sim --json is not valid JSON"; fi
if P tree | python3 -c 'import json,sys; d=json.load(sys.stdin); d["doc"]["root"]["kind"]' >/dev/null; then ok "tree is valid JSON"; else bad "tree is not valid JSON"; fi

# Config-driven keybindings: a `bindings` map overrides the default chords per
# command, and `reload` re-grabs without a daemon restart. Written last so the
# key presses above ran against the default plan.
mkdir -p "$XDG_CONFIG_HOME/pergola"
printf '%s' '{"bindings":{"view 3":["Super+9"],"overlay":[]}}' \
  > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1
sleep 0.4
if P keys | grep -c 'Super+9 .*view 3' >/dev/null; then
  ok "a config binding overrides a default chord and reload re-grabs"
else
  bad "config binding did not take effect: $(P keys | grep 'view 3' || echo 'not bound')"
fi
if P keys --check | grep -c '^off .*overlay' >/dev/null; then
  ok "an empty candidate list shows as a deliberate unbind"
else
  bad "an empty candidate list was not reported as off: $(P keys --check | grep overlay || echo 'not listed')"
fi

# `hotkeys: false` at reload must release the grabs so the desktop gets its
# chords back; `hotkeys: true` must take them again. Both without a restart.
printf '%s' '{"hotkeys":false}' > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1
sleep 0.3
if [ -z "$(P keys)" ]; then
  ok "hotkeys: false at reload releases every grab"
else
  bad "hotkeys: false did not release the grabs: $(P keys | head -3 | tr '\n' ' ')"
fi
printf '%s' '{"hotkeys":true}' > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1
sleep 0.3
if [ "$(P keys | grep -c 'everything')" -ge 1 ]; then
  ok "hotkeys: true at reload takes the grabs back"
else
  bad "hotkeys: true did not re-grab"
fi

# auto_dock is a tri-state. `off` (the default) leaves a new window floating;
# `new` docks a window that appears after the daemon started. This is the
# backlog's "no window is adopted into a space it was not opened in": off never
# adopts, and the three startup windows stay where they were.
printf '%s' '{"auto_dock":"off","hotkeys":true}' > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1; sleep 0.3
xterm -T pergola-e2e-autodock-off -geometry 60x18+320+320 -e sh -c 'exec sleep 3600' >/dev/null 2>&1 & AUTO_OFF_PID=$!; XPIDS+=($AUTO_OFF_PID)
# reconcile runs on a 2 s timer, so poll for the seat instead of guessing a sleep.
AUTO_OFF=""
for _ in $(seq 1 40); do
  AUTO_OFF=$(xdotool search --name pergola-e2e-autodock-off 2>/dev/null | head -1)
  [ -n "$AUTO_OFF" ] && P list | grep -c "^floating.*$(printf '0x%08x' "$AUTO_OFF")" >/dev/null && break
  sleep 0.1
done
if [ -n "$AUTO_OFF" ] && P list | grep -c "^floating.*$(printf '0x%08x' "$AUTO_OFF")" >/dev/null; then
  ok "auto_dock off leaves a new window floating"
else
  bad "auto_dock off adopted a new window"
fi
kill "$AUTO_OFF_PID" 2>/dev/null; sleep 0.4
printf '%s' '{"auto_dock":"new","hotkeys":true}' > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1; sleep 0.3
xterm -T pergola-e2e-autodock-new -geometry 60x18+340+340 -e sh -c 'exec sleep 3600' >/dev/null 2>&1 & AUTO_NEW_PID=$!; XPIDS+=($AUTO_NEW_PID)
AUTO_NEW=""
for _ in $(seq 1 40); do
  AUTO_NEW=$(xdotool search --name pergola-e2e-autodock-new 2>/dev/null | head -1)
  [ -n "$AUTO_NEW" ] && P list | grep -c "^docked.*$(printf '0x%08x' "$AUTO_NEW")" >/dev/null && break
  sleep 0.1
done
if [ -n "$AUTO_NEW" ] && P list | grep -c "^docked.*$(printf '0x%08x' "$AUTO_NEW")" >/dev/null; then
  ok "auto_dock new docks a window opened after startup"
else
  bad "auto_dock new did not dock a new window"
fi
kill "$AUTO_NEW_PID" 2>/dev/null; sleep 0.4
printf '%s' '{"hotkeys":true}' > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1

# The shell commands: `bind` and `unbind` write config.json and reload, so a
# user never has to edit JSON by hand. They must preserve other settings.
printf '%s' '{"gap":6.0,"hotkeys":true}' > "$XDG_CONFIG_HOME/pergola/config.json"
BIND_OUT=$("$BIN" --socket "$SOCK" bind "gap 12" Super+9 2>&1)
case "$BIND_OUT" in
  *"bound 'gap 12' to Super+9"*) ok "bind writes the chord and reloads" ;;
  *) bad "bind did not report the binding: $BIND_OUT" ;;
esac
if grep -q '"gap": *6.0' "$XDG_CONFIG_HOME/pergola/config.json"; then
  ok "bind preserves the other settings in config.json"
else
  bad "bind lost the other settings: $(cat "$XDG_CONFIG_HOME/pergola/config.json")"
fi
if [ "$(P keys | grep -c 'Super+9')" -ge 1 ]; then
  ok "a bound chord is live after reload"
else
  bad "bind did not take effect: $(P keys | head -3 | tr '\n' ' ')"
fi
UNBIND_OUT=$("$BIN" --socket "$SOCK" unbind "gap 12" 2>&1)
case "$UNBIND_OUT" in
  *"unbound 'gap 12'"*) ok "unbind clears the chord" ;;
  *) bad "unbind did not report: $UNBIND_OUT" ;;
esac
if [ "$(P keys --check | grep -c '^off .*gap 12')" -ge 1 ]; then
  ok "an unbound command shows as off"
else
  bad "unbind did not show as off: $(P keys --check | grep 'gap 12' || echo 'not listed')"
fi
if "$BIN" --socket "$SOCK" bind "not-a-command" Super+9 >/dev/null 2>&1; then
  bad "bind accepts an unknown command"
else
  ok "bind rejects an unknown command"
fi
if "$BIN" --socket "$SOCK" bind "gap 12" NotAChord >/dev/null 2>&1; then
  bad "bind accepts an unparseable chord"
else
  ok "bind rejects an unparseable chord"
fi
DEFAULT_OUT=$("$BIN" --socket "$SOCK" bind "gap 12" --default 2>&1)
case "$DEFAULT_OUT" in
  *"restored the default chords"*) ok "bind --default removes the override" ;;
  *) bad "bind --default did not report: $DEFAULT_OUT" ;;
esac
if grep -q '"bindings"' "$XDG_CONFIG_HOME/pergola/config.json"; then
  bad "bind --default left an empty bindings map behind"
else
  ok "bind --default leaves a clean config"
fi
# A malformed config is never silently regenerated: the user's typo stays.
printf '%s' '{"gap": 6.0, "bindings": {"back": ["Super+b"}' > "$XDG_CONFIG_HOME/pergola/config.json"
if "$BIN" --socket "$SOCK" bind "back" Super+z >/dev/null 2>&1; then
  bad "bind rewrote a malformed config"
else
  ok "bind refuses to rewrite a malformed config"
fi
if grep -q 'Super+b' "$XDG_CONFIG_HOME/pergola/config.json"; then
  ok "the malformed config is left untouched"
else
  bad "the malformed config was overwritten"
fi

# `pergola config` reads the file itself, so it reports a typo while the daemon
# is happy with its defaults — and names the line the typo is on.
printf '%s\n' '{' '  "gap": 6,' '  "gaap": 12' '}' > "$XDG_CONFIG_HOME/pergola/config.json"
CHECK=$("$BIN" config --check 2>&1); CHECK_RC=$?
if [ "$CHECK_RC" -ne 0 ]; then ok "config --check rejects an unknown key"; else bad "config --check accepted an unknown key: $CHECK"; fi
if printf '%s' "$CHECK" | grep -c 'line 3: unknown key "gaap"' >/dev/null; then
  ok "the unknown key is reported with its line"
else
  bad "the unknown key was not reported with line 3: $CHECK"
fi
printf '%s\n' '{' '  "gap": -4,' '  "terminal": "/nonexistent/term"' '}' > "$XDG_CONFIG_HOME/pergola/config.json"
CHECK=$("$BIN" config --check 2>&1); CHECK_RC=$?
if [ "$CHECK_RC" -ne 0 ] && printf '%s' "$CHECK" | grep -c 'gap must be between 0 and 400' >/dev/null; then
  ok "config --check rejects an out-of-range gap"
else
  bad "the out-of-range gap was not reported: $CHECK"
fi
if printf '%s' "$CHECK" | grep -c 'terminal "/nonexistent/term"' >/dev/null; then
  ok "config --check rejects a terminal that does not exist"
else
  bad "the missing terminal was not reported: $CHECK"
fi
printf '%s\n' '{"gap": 9, "overlay": true, "terminal": "/bin/sh"}' > "$XDG_CONFIG_HOME/pergola/config.json"
if "$BIN" config --check >/dev/null 2>&1; then ok "config --check accepts a good file"; else bad "config --check rejected a good file: $("$BIN" config --check 2>&1)"; fi
RESOLVED=$("$BIN" config 2>&1)
if printf '%s' "$RESOLVED" | grep -c 'gap            9  ' >/dev/null; then
  ok "config prints the resolved value"
else
  bad "config did not print the resolved gap: $RESOLVED"
fi
if printf '%s' "$RESOLVED" | grep -c 'config.json:1' >/dev/null; then
  ok "config names where a value came from"
else
  bad "config did not name the key's source: $RESOLVED"
fi
printf '%s' '{"gap": 9}' > "$XDG_CONFIG_HOME/pergola/config.json"
if "$BIN" config 2>&1 | head -2 | grep -c 'panicked' >/dev/null; then
  bad "config panicked when its output was piped"
else
  ok "config survives a closed pipe instead of panicking"
fi

# `pergola settings` is the same validation driven by a menu. The primitive is
# `settings <key> <value>`; the menu path is proven with a stand-in dmenu so the
# test never needs a GUI.
printf '%s\n' '{"gap": 6, "gaap": 1}' > "$XDG_CONFIG_HOME/pergola/config.json"
if "$BIN" settings gap 12 >/dev/null 2>&1; then ok "settings sets one key"; else bad "settings gap 12 failed"; fi
if grep -c '"gap": 12' "$XDG_CONFIG_HOME/pergola/config.json" >/dev/null; then
  ok "settings wrote the value"
else
  bad "settings did not write gap 12: $(cat "$XDG_CONFIG_HOME/pergola/config.json")"
fi
if grep -c '"gaap"' "$XDG_CONFIG_HOME/pergola/config.json" >/dev/null; then
  ok "settings keeps a key it does not know"
else
  bad "settings dropped the unknown key"
fi
if "$BIN" settings minimum_share 2 >/dev/null 2>&1; then bad "settings accepted an out-of-range value"; else ok "settings rejects an out-of-range value"; fi
if grep -c '"minimum_share"' "$XDG_CONFIG_HOME/pergola/config.json" >/dev/null; then
  bad "the rejected value was written anyway"
else
  ok "the rejected value was not written"
fi
LIST=$("$BIN" settings --list)
if printf '%s' "$LIST" | grep -c '^gap = ' >/dev/null; then ok "settings --list names the gap"; else bad "settings --list has no gap row"; fi
if printf '%s' "$LIST" | grep -c '^chord: dock left = ' >/dev/null; then ok "settings --list shows the chord map"; else bad "settings --list has no chord row: $(printf '%s' "$LIST" | head -3)"; fi
# Each row explains itself: what the key does, its range and its default. The
# menu is opened from a chord, with no manual next to it.
if printf '%s' "$LIST" | grep -c '^gap = .* — pixels between tiles; 0 to 400 (default 6)' >/dev/null; then
  ok "a settings row explains what it changes"
else
  bad "no help on the gap row: $(printf '%s' "$LIST" | grep '^gap = ')"
fi
if printf '%s' "$LIST" | grep -c '^chord: dock left = .* — .*' >/dev/null; then
  ok "a chord row explains what it binds"
else
  bad "no help on a chord row: $(printf '%s' "$LIST" | grep '^chord: dock left' | head -1)"
fi
# A rejected value is the moment the person needs to know: the menu process is
# detached, so stderr goes to the journal. The sandbox-wide stub notifier (see
# the sandbox setup) records the call, and the value must still be refused and
# not written.
: > "$PERGOLA_NOTIFY_LOG"
if "$BIN" settings gap banana >/dev/null 2>&1; then
  bad "settings accepted a non-numeric gap"
else
  ok "settings still rejects a bad value"
fi
# spawn() is asynchronous; give the stub a moment to be reaped.
for _ in $(seq 1 20); do [ -s "$PERGOLA_NOTIFY_LOG" ] && break; sleep 0.1; done
if grep -c 'gap must be a number' "$PERGOLA_NOTIFY_LOG" >/dev/null; then
  ok "a rejected setting also reaches a desktop notification"
else
  bad "no notification for the rejected value: $(cat "$PERGOLA_NOTIFY_LOG" 2>/dev/null)"
fi
if grep -c '"gap": "banana"' "$XDG_CONFIG_HOME/pergola/config.json" >/dev/null; then
  bad "the notified rejection was written anyway"
else
  ok "the notified rejection was not written"
fi
cat > "$TMP/fake-dmenu" <<'EOS'
#!/bin/sh
# Stand-in dmenu: list mode picks the first row, entry mode types 9.
case "$*" in
  *"-p pergola"*) IFS= read -r first; printf '%s\n' "$first" ;;
  *) IFS= read -r _; printf '9\n' ;;
esac
EOS
chmod +x "$TMP/fake-dmenu"
printf '%s\n' '{"gap": 6}' > "$XDG_CONFIG_HOME/pergola/config.json"
PERGOLA_MENU="$TMP/fake-dmenu" "$BIN" settings-run >/dev/null 2>&1
check_eq '{"gap":9.0}' "$(tr -d '\n ' < "$XDG_CONFIG_HOME/pergola/config.json")" "a menu pick writes the row"
# The chord row prefills `Ctrl+Alt+Shift+Left, Super+Shift+Left`; typing a
# comma-separated pair has to bind both, not fail to parse its own prefill.
cat > "$TMP/fake-dmenu-chord" <<'EOS'
#!/bin/sh
# Stand-in dmenu: list mode picks the `dock left` chord row, entry mode types a pair.
case "$*" in
  *"-p pergola"*) while IFS= read -r row; do
      case "$row" in "chord: dock left = "*) printf '%s\n' "$row"; break;; esac
    done ;;
  *) IFS= read -r _; printf 'Super+9, Super+8\n' ;;
esac
EOS
chmod +x "$TMP/fake-dmenu-chord"
printf '%s' '{"gap": 6}' > "$XDG_CONFIG_HOME/pergola/config.json"
PERGOLA_MENU="$TMP/fake-dmenu-chord" "$BIN" settings-run >/dev/null 2>&1
# Parse the file instead of grepping it: a pretty-printed array and the space
# inside `"dock left"` defeat both a naive grep and `tr -d ' '`.
if python3 - "$XDG_CONFIG_HOME/pergola/config.json" <<'EOS'
import json, sys
cfg = json.load(open(sys.argv[1]))
raise SystemExit(0 if cfg.get("bindings", {}).get("dock left") == ["Super+9", "Super+8"] else 1)
EOS
then
  ok "a comma-separated entry binds both chords"
else
  bad "the chord row did not honour its own prefill syntax: $(cat "$XDG_CONFIG_HOME/pergola/config.json")"
fi
printf '%s' '{}' > "$XDG_CONFIG_HOME/pergola/config.json"

say
say "10. tab groups"
# A tab group is one panel holding several windows: every view is mapped at the
# panel rectangle and only the selected one is raised. The document model has
# always supported this; before 0.4.0-dev nothing could create, switch or
# repair a group, so a closed tab took the whole panel with it.
printf '%s' '{"gap":6.0,"hotkeys":true}' > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1
sleep 0.3
P float-all >/dev/null; sleep 0.4
P arrange >/dev/null; sleep 0.4

# The first panel with more than one view, as "id count selected".
tab_panel() {
  P tree | python3 -c '
import json, sys
d = json.load(sys.stdin)
def walk(n):
    if n.get("kind") == "panel":
        yield n
    for c in n.get("children", []):
        yield from walk(c)
for p in walk(d["doc"]["root"]):
    if len(p["views"]) > 1:
        print(p["id"], len(p["views"]), p.get("selected") or "-")
        break
'
}

# Fold W2 into W1's panel: `tab` with no --target joins the panel that had
# focus before the tabbed window.
xdotool windowactivate "$W1" >/dev/null 2>&1; sleep 0.4
TAB_OUT=$("$BIN" --socket "$SOCK" tab --window "$W2" 2>&1)
case "$TAB_OUT" in
  *"tabbed"*) ok "tab folds a window into a panel" ;;
  *) bad "tab did not report the fold: $TAB_OUT" ;;
esac
sleep 0.5
S_W1=$(printf 's-%x' "$((W1))")
S_W2=$(printf 's-%x' "$((W2))")
read -r TAB_ID TAB_COUNT TAB_SEL <<EOF
$(tab_panel)
EOF
check_eq "2" "${TAB_COUNT:-0}" "the panel now holds both windows"
check_eq "$S_W2" "${TAB_SEL:-}" "the tabbed window is the shown view"

# Both views are mapped at the panel rectangle: the X server must agree.
eval "$(xdotool getwindowgeometry --shell "$W1")"; read -r A_X A_Y A_W A_H <<<"$X $Y $WIDTH $HEIGHT"
eval "$(xdotool getwindowgeometry --shell "$W2")"; read -r B_X B_Y B_W B_H <<<"$X $Y $WIDTH $HEIGHT"
check_near "$A_X" "$B_X" 2 "both tabs sit at the same x"
check_near "$A_Y" "$B_Y" 2 "both tabs sit at the same y"
check_near "$A_W" "$B_W" 2 "both tabs have the same width"
check_near "$A_H" "$B_H" 2 "both tabs have the same height"

# Exactly one is raised: in _NET_CLIENT_LIST_STACKING (bottom to top) the
# shown view must come after the hidden one.
stack_rank() { # window -> 1-based position in the stacking order (bottom = 1)
  # Compare numeric ids: the WM prints 0x60000c, the test has 0x0060000c.
  xprop -root _NET_CLIENT_LIST_STACKING 2>/dev/null | python3 -c '
import sys
want = int(sys.argv[1], 16)
tail = sys.stdin.read().split("=", 1)[-1]
ids = [int(t, 16) for t in tail.replace(",", " ").split() if t.startswith("0x")]
print(ids.index(want) + 1 if want in ids else "")
' "$1"
}
R_W1=$(stack_rank "$W1"); R_W2=$(stack_rank "$W2")
if [ -n "$R_W1" ] && [ -n "$R_W2" ] && [ "$R_W2" -gt "$R_W1" ]; then
  ok "only the shown tab is raised ($W2 above $W1)"
else
  bad "the shown tab is not the top one: ranks W1=${R_W1:-?} W2=${R_W2:-?}"
fi

# Cycling moves the shown view and raises the other one.
P tab-next >/dev/null || fail "tab-next"
sleep 0.5
read -r _ _ TAB_SEL <<EOF
$(tab_panel)
EOF
check_eq "$S_W1" "${TAB_SEL:-}" "tab-next shows the other view"
R_W1=$(stack_rank "$W1"); R_W2=$(stack_rank "$W2")
if [ -n "$R_W1" ] && [ -n "$R_W2" ] && [ "$R_W1" -gt "$R_W2" ]; then
  ok "tab-next raises the newly shown view"
else
  bad "tab-next did not raise the shown view: ranks W1=${R_W1:-?} W2=${R_W2:-?}"
fi
P tab-prev >/dev/null || fail "tab-prev"
sleep 0.4
read -r _ _ TAB_SEL <<EOF
$(tab_panel)
EOF
check_eq "$S_W2" "${TAB_SEL:-}" "tab-prev returns to the previous view"

# The default chord must do the same thing. Cycling by key is the whole point:
# a tab group the user cannot switch from the keyboard is not a feature.
if P keys --check | grep -c 'tab-next' >/dev/null; then
  NEXT_CHORD=$(chord_for tab-next) || fail "tab-next has no chord"
  xdotool key "$NEXT_CHORD"; sleep 0.5
  read -r _ _ TAB_SEL <<EOF
$(tab_panel)
EOF
  check_eq "$S_W1" "${TAB_SEL:-}" "the tab-next chord shows the other view"
  xdotool key "$(chord_for tab-prev)"; sleep 0.4
  read -r _ _ TAB_SEL <<EOF
$(tab_panel)
EOF
  check_eq "$S_W2" "${TAB_SEL:-}" "the tab-prev chord returns to the first view"
else
  bad "tab-next is not bound by default"
fi

# Closing the shown tab must not collapse the panel: the sibling stays put.
# The daemon reconciles every 2 s, so give the destroy time to land.
P tab-select "$S_W1" >/dev/null || fail "tab-select"
sleep 0.4
read -r _ _ TAB_SEL <<EOF
$(tab_panel)
EOF
check_eq "$S_W1" "${TAB_SEL:-}" "tab-select shows a named view"
xdotool windowkill "$W1" >/dev/null 2>&1
sleep 2.6
if P tree | grep -c "$S_W2" >/dev/null; then
  ok "closing one tab leaves its sibling in the layout"
else
  bad "closing one tab removed the whole panel"
fi
if P tree | grep -c "$S_W1" >/dev/null; then
  bad "the closed tab is still in the layout"
else
  ok "the closed tab is dropped from the layout"
fi
if P list | grep -c "^docked.*$W2" >/dev/null; then
  ok "the surviving tab is still docked and mapped"
else
  bad "the surviving tab was lost: $(P list | tr '\n' ' ')"
fi

# A floating window can be folded into a panel as a tab too: `tab` accepts a
# seat off the desktop layer and docks it into the target. W3 is a separate
# panel after the group above collapsed, so float it and tab it into W2's.
# Under load the 2 s reconcile may not have adopted W3 yet (measured
# 2026-11-16: one flaked run out of three failed this precondition), so wait
# for it and dock it explicitly instead of assuming the timer fired.
for _ in $(seq 1 20); do
  P list | grep -c "^docked.*$W3" >/dev/null && break
  sleep 0.1
done
if ! P list | grep -c "^docked.*$W3" >/dev/null; then
  P dock left --window "$W3" >/dev/null 2>&1
  sleep 0.5
fi
if P list | grep -c "^docked.*$W3" >/dev/null; then
  P float --window "$W3" >/dev/null 2>&1 || fail "float W3"
  sleep 0.5
  P tab --window "$W3" >/dev/null 2>&1 || fail "tab a floating window"
  sleep 0.5
  read -r _ TAB_COUNT TAB_SEL <<EOF
$(tab_panel)
EOF
  check_eq "2" "${TAB_COUNT:-0}" "a floating window folds into a tab group"
  S_W3=$(printf 's-%x' "$((W3))")
  check_eq "$S_W3" "${TAB_SEL:-}" "the folded floating window is the shown view"
  if P list | grep -c "^docked.*$W3" >/dev/null; then
    ok "the folded window is docked, not floating"
  else
    bad "the folded window is still floating: $(P list | tr '\n' ' ')"
  fi
else
  bad "W3 was not a docked panel to float"
fi

say
say "11. event stream (pergola subscribe)"
# A status bar should not have to poll `pergola tree` and re-implement the diff:
# the daemon pushes committed transitions, starting with a state snapshot so a
# client that joins late is not blind (bspwm's startup gap).
printf '%s' '{"gap":6.0,"hotkeys":true}' > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1
sleep 0.3
SUBLOG="$TMP/sub.jsonl"
"$BIN" --socket "$SOCK" subscribe >"$SUBLOG" 2>"$TMP/sub.err" & SUB_PID=$!
PIDS+=("$SUB_PID")
for _ in $(seq 1 30); do [ -s "$SUBLOG" ] && break; sleep 0.1; done
if head -1 "$SUBLOG" 2>/dev/null | grep -c '"type":"state"' >/dev/null; then
  ok "a subscriber is sent the state first"
else
  bad "the first record was not a state snapshot: $(head -1 "$SUBLOG" 2>/dev/null)"
fi

# The three transitions the backlog names, on a real window: undock, dock,
# close. W2 is a tab view from section 10, so it starts docked; make sure of it
# rather than assuming the reconcile under load.
for _ in $(seq 1 20); do
  P list | grep -c "^docked.*$W2" >/dev/null && break
  sleep 0.1
done
if ! P list | grep -c "^docked.*$W2" >/dev/null; then
  P dock left --window "$W2" >/dev/null 2>&1; sleep 0.5
fi
P float --window "$W2" >/dev/null 2>&1; sleep 0.6
P dock left --window "$W2" >/dev/null 2>&1; sleep 0.6
xdotool windowkill "$W2" >/dev/null 2>&1; sleep 2.6
kill "$SUB_PID" 2>/dev/null
wait "$SUB_PID" 2>/dev/null || true

# Count the records, and the ones that name this window. Prints eight numbers.
SUB_COUNTS=$(python3 - "$SUBLOG" "$W2" <<'EOS'
import json, sys
path, want = sys.argv[1], int(sys.argv[2], 16)
types, bad = {}, 0
for line in open(path):
    line = line.strip()
    if not line:
        continue
    try:
        record = json.loads(line)
    except Exception:
        bad += 1
        continue
    kind = record.get("type", "")
    types[kind] = types.get(kind, 0) + 1
    if record.get("window") == want:
        key = "w2:" + kind
        types[key] = types.get(key, 0) + 1
print(types.get("state", 0), types.get("dock", 0), types.get("undock", 0),
      types.get("window-closed", 0), types.get("w2:dock", 0),
      types.get("w2:undock", 0), types.get("w2:window-closed", 0), bad)
EOS
)
read -r N_STATE N_DOCK N_UNDOCK N_CLOSED W2_DOCK W2_UNDOCK W2_CLOSED N_BAD <<<"$SUB_COUNTS"
check_eq "1" "$N_STATE" "one state record on connect"
check_eq "0" "$N_BAD" "every record is valid JSON"
if [ "${N_DOCK:-0}" -ge 1 ]; then ok "a dock produced a record"; else bad "no dock record: $(cat "$SUBLOG" | head -3 | tr '\n' ' ')"; fi
if [ "${N_UNDOCK:-0}" -ge 1 ]; then ok "an undock produced a record"; else bad "no undock record"; fi
if [ "${N_CLOSED:-0}" -ge 1 ]; then ok "a window close produced a record"; else bad "no window-closed record"; fi
if [ "${W2_DOCK:-0}" -ge 1 ]; then ok "the dock record names the window"; else bad "the dock record does not name $W2"; fi
if [ "${W2_UNDOCK:-0}" -ge 1 ]; then ok "the undock record names the window"; else bad "the undock record does not name $W2"; fi
if [ "${W2_CLOSED:-0}" -ge 1 ]; then ok "the close record names the window"; else bad "the close record does not name $W2"; fi

# A bar that stops reading must never stall the window manager. The socket is
# non-blocking and the queue is bounded, so the daemon keeps answering commands
# while a stalled subscriber has thousands of bytes waiting for it.
mkfifo "$TMP/sub.fifo"
exec 9<>"$TMP/sub.fifo"
"$BIN" --socket "$SOCK" subscribe >"$TMP/sub.fifo" 2>"$TMP/sub3.err" & SUB3=$!
PIDS+=("$SUB3")
sleep 0.5
for i in $(seq 1 60); do P gap $((6 + i % 3)) >/dev/null 2>&1; done
START=$(date +%s%N)
P status >/dev/null 2>&1
END=$(date +%s%N)
STALLED_MS=$(( (END - START) / 1000000 ))
exec 9<&-
if [ "$STALLED_MS" -lt 2000 ]; then
  ok "a stalled subscriber does not stall the daemon (${STALLED_MS}ms)"
else
  bad "the daemon took ${STALLED_MS}ms while a subscriber was stalled"
fi
kill "$SUB3" 2>/dev/null
wait "$SUB3" 2>/dev/null || true
P gap 6 >/dev/null 2>&1

# The daemon going away ends the stream cleanly, so a supervisor can restart the
# bar; a client hung on a dead socket would be a leak.
"$BIN" --socket "$SOCK" subscribe >"$TMP/sub-end.jsonl" 2>"$TMP/sub-end.err" & SUB_END=$!
sleep 0.5
P stop >/dev/null 2>&1
for _ in $(seq 1 40); do kill -0 "$SUB_END" 2>/dev/null || break; sleep 0.1; done
if kill -0 "$SUB_END" 2>/dev/null; then
  bad "the subscriber hung after the daemon stopped"
  kill "$SUB_END" 2>/dev/null
else
  ok "the subscriber exits when the daemon stops"
fi
SUB_RC=0
wait "$SUB_END" 2>/dev/null || SUB_RC=$?
check_eq "0" "$SUB_RC" "the subscriber exits cleanly"
if grep -c 'closed the event stream' "$TMP/sub-end.err" >/dev/null; then
  ok "the subscriber says why it stopped"
else
  bad "the subscriber did not say why: $(cat "$TMP/sub-end.err")"
fi

say
say "12. activation requests (_NET_ACTIVE_WINDOW)"
# A taskbar's "click to focus" is an EWMH `_NET_ACTIVE_WINDOW` client message to
# the root window. The daemon only hears it because it selects the root's
# substructure events; without a handler the click is a silent no-op, and a
# request for a hidden tab view focuses a window nobody can see.
# Section 11 stopped the daemon, so start it again. Debug on: the positive
# "handled an activation" line is the only proof the daemon acted, because
# `status active` reads the root property the *desktop* sets whether or not a
# handler exists — an assertion on it alone would pass with the handler deleted.
PERGOLA_DEBUG=1 "$BIN" --socket "$SOCK" daemon >>"$TMP/daemon.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 50); do P status >/dev/null 2>&1 && break; sleep 0.1; done
sleep 0.3
xterm -T pergola-e2e-activate -geometry 60x18+300+260 -e sh -c 'exec sleep 3600' >/dev/null 2>&1 & ACT_PID=$!; XPIDS+=("$ACT_PID")
W4=""
# A pager request for a window the daemon has not adopted yet is ignored (the
# daemon has no seat to focus), and the assertion below would blame the
# handler. The window exists in the X server before the daemon has received an
# event for it, so wait for the daemon's own `list` to name it, not for
# `xdotool search` alone (measured 2026-11-19: the run sent the activation for
# a window the daemon had never seen and failed the very assertion the product
# satisfies).
for _ in $(seq 1 100); do
  [ -n "$W4" ] || W4=$(xdotool search --name '^pergola-e2e-activate$' 2>/dev/null | head -1)
  [ -n "$W4" ] || { sleep 0.1; continue; }
  P list | grep -q "0x$(printf '%08x' "$W4")" && break
  sleep 0.1
done
if [ -z "$W4" ] || ! P list | grep -q "0x$(printf '%08x' "$W4")"; then
  fail "the activation test window was never adopted by the daemon: $(P list | tail -2 | tr '\n' ' ')"
fi
W4=$(printf '0x%08x' "$W4")
P float-all >/dev/null 2>&1; sleep 0.4
P arrange >/dev/null 2>&1; sleep 0.5
# arrange folds floating windows back into the tree, so the window must have a
# panel before the request can be handled; poll rather than assume 0.5 s.
for _ in $(seq 1 30); do
  P tree | grep -q "p-$(printf '%x' "$((W4))")" && break
  sleep 0.1
done
if ! P tree | grep -q "p-$(printf '%x' "$((W4))")"; then
  fail "the activation test window is not in the tree after arrange: $(P list | grep "$W4" | tr '\n' ' ')"
fi

# Focusing a window from outside the daemon must be visible in `status`.
xdotool windowactivate "$W3" >/dev/null 2>&1
wait_until 5 active_is "$W3" || true
BEFORE=$(P status | sed -n 's/.*active \(0x[0-9a-f]*\).*/\1/p')
# The WM may drop a focus request made while it is busy tiling; retry once and
# poll the daemon's view rather than assuming 0.5 s is enough.
xdotool windowactivate "$W4" >/dev/null 2>&1
if ! wait_until 2 active_is "$W4"; then
  xdotool windowactivate "$W4" >/dev/null 2>&1
  wait_until 3 active_is "$W4" || true
fi
AFTER=$(P status | sed -n 's/.*active \(0x[0-9a-f]*\).*/\1/p')
# `status` prints the window id unpadded (`0x60000c`); the e2e's W4 is padded
# (`0x0060000c`). Compare in the daemon's own format.
check_eq "$(printf '0x%x' "$((W4))")" "$AFTER" "a pager's activation request is the daemon's active window"
if [ "$BEFORE" != "$AFTER" ]; then
  ok "the activation request changed the active window ($BEFORE -> $AFTER)"
else
  bad "the activation request did not change the active window ($AFTER)"
fi
# The daemon's own line, not the desktop's property: this is what fails when
# the ClientMessage arm is missing.
if grep -c "activation 0x$(printf '%x' "$((W4))") source .* -> seat" "$TMP/daemon.log" >/dev/null; then
  ok "the daemon handled the activation request itself"
else
  bad "the daemon never logged the activation request for $W4"
fi

# A request for a hidden tab view must show it first: the desktop can focus a
# window that is mapped under its sibling, but the user would see nothing.
xdotool windowactivate "$W3" >/dev/null 2>&1; sleep 0.4
P tab --window "$W4" >/dev/null 2>&1; sleep 0.5
read -r _ TAB_COUNT TAB_SEL <<EOF
$(tab_panel)
EOF
S_W3=$(printf 's-%x' "$((W3))")
S_W4=$(printf 's-%x' "$((W4))")
check_eq "2" "${TAB_COUNT:-0}" "the activation test has a two-view tab group"
check_eq "$S_W4" "${TAB_SEL:-}" "the newly tabbed window is the shown view"
xdotool windowactivate "$W3" >/dev/null 2>&1; sleep 0.6
read -r _ _ TAB_SEL <<EOF
$(tab_panel)
EOF
check_eq "$S_W3" "${TAB_SEL:-}" "a request for a hidden tab view shows that view"
ACTIVE=$(P status | sed -n 's/.*active \(0x[0-9a-f]*\).*/\1/p')
check_eq "$(printf '0x%x' "$((W3))")" "$ACTIVE" "the hidden view is the daemon's active window"
# And the daemon knew the view was hidden, i.e. it switched the tab rather than
# letting the desktop focus a window nobody can see.
if grep -c "activation 0x$(printf '%x' "$((W3))") source .* hidden true" "$TMP/daemon.log" >/dev/null; then
  ok "the daemon saw the hidden view and showed it"
else
  bad "the daemon did not treat the tab view as hidden: $(grep 'activation' "$TMP/daemon.log" 2>/dev/null | tail -2 | tr '\n' ' ')"
fi

say
say "13. named workspaces (tags)"
# A named workspace owns a set of views; the active tag decides which views are
# laid out, and a view on another tag stays in the document and keeps its tile,
# so switching back is exact. This answers the survey's "there is no way to
# say: move this window to view 3". The assertions are geometry, not only the
# daemon's own bookkeeping: a parked window has to be off the work area.
xterm -T pergola-e2e-tag-a -geometry 60x18+380+320 -e sh -c 'exec sleep 3600' >/dev/null 2>&1 & XPIDS+=($!)
xterm -T pergola-e2e-tag-b -geometry 60x18+440+320 -e sh -c 'exec sleep 3600' >/dev/null 2>&1 & XPIDS+=($!)
sleep 1.5
TAG_A=$(xdotool search --name '^pergola-e2e-tag-a$' 2>/dev/null | head -1)
TAG_B=$(xdotool search --name '^pergola-e2e-tag-b$' 2>/dev/null | head -1)
[ -n "$TAG_A" ] && [ -n "$TAG_B" ] || fail "the tag test windows did not appear"
TAG_A=$(printf '0x%08x' "$TAG_A")
TAG_B=$(printf '0x%08x' "$TAG_B")
# auto_dock is off in this sandbox, so dock them by hand — and wait for each to
# have a seat first: `list` names an unadopted window too, and a dock for one
# fails with "not managed".
for _ in $(seq 1 50); do
  [ "$(P list | grep -cE "^(docked|floating) +$TAG_A")" -ge 1 ] \
    && [ "$(P list | grep -cE "^(docked|floating) +$TAG_B")" -ge 1 ] && break
  sleep 0.1
done
P dock left --window "$TAG_A" >/dev/null 2>&1; sleep 0.5
P dock right --window "$TAG_B" >/dev/null 2>&1; sleep 0.5
if [ "$(P list | grep -c '^docked')" -ge 2 ]; then
  ok "the two tag test windows are docked tiles"
else
  bad "the tag test windows did not dock: $(P list | head -4 | tr '\n' ' ')"
fi

# Before any tag exists, the command explains itself instead of failing.
if P tag-view | grep -c 'no tags' >/dev/null; then
  ok "tag-view explains the empty workspace set"
else
  bad "tag-view did not explain itself: $(P tag-view)"
fi
P window-tag work --window "$TAG_A" >/dev/null 2>&1; sleep 0.4
P window-tag play --window "$TAG_B" >/dev/null 2>&1; sleep 0.4
if P list | grep -c "^docked.*$TAG_A.*\[tag work\]" >/dev/null; then
  ok "the first window belongs to tag work"
else
  bad "the first window is not on tag work: $(P list | grep "$TAG_A")"
fi
if P list | grep -c "^docked.*$TAG_B.*\[tag play\]" >/dev/null; then
  ok "the second window belongs to tag play"
else
  bad "the second window is not on tag play: $(P list | grep "$TAG_B")"
fi

# Moving a live window between tile and floating layers must keep its
# workspace; only closing the window removes that membership.
if P float --window "$TAG_A" >/dev/null 2>&1; then
  sleep 0.4
  if P list | grep -q "^floating.*$TAG_A.*\[tag work\]"; then
    ok "floating preserves the window's workspace"
  else
    bad "floating erased the window's workspace: $(P list | grep "$TAG_A")"
  fi
else
  bad "floating the tagged window failed"
fi
if P dock left --window "$TAG_A" >/dev/null 2>&1; then
  sleep 0.4
else
  bad "re-docking the tagged window failed"
fi
if P list | grep -q "^docked.*$TAG_A.*\[tag work\]"; then
  ok "re-docking preserves the window's workspace"
else
  bad "moving a window erased its workspace: $(P list | grep "$TAG_A")"
fi

# Switching to a tag maps only its views; the others leave the screen.
P tag-view work >/dev/null 2>&1; sleep 0.6
check_eq "1" "$(P rects | grep -c .)" "tag work lays out one panel"
if P list | grep -c "^docked.*$TAG_A" >/dev/null; then
  ok "tag work's window is docked"
else
  bad "tag work's window is not docked: $(P list | grep "$TAG_A")"
fi
if P list | grep -c "^parked.*$TAG_B" >/dev/null; then
  ok "tag play's window is parked off screen"
else
  bad "tag play's window is not parked: $(P list | grep "$TAG_B")"
fi
if P status | grep -c 'tag work' >/dev/null; then
  ok "status names the active workspace"
else
  bad "status does not name the workspace: $(P status)"
fi
PARK_PCT=$(onscreen_pct "$TAG_B")
if [ "$PARK_PCT" -le 25 ]; then
  ok "the parked window is off the workspace (${PARK_PCT}% of its client on screen)"
else
  bad "the parked window is still on screen (${PARK_PCT}% visible)"
fi
check_geometry "on tag work"

P tag-view play >/dev/null 2>&1; sleep 0.6
check_eq "1" "$(P rects | grep -c .)" "tag play lays out one panel"
if P list | grep -c "^parked.*$TAG_A" >/dev/null; then
  ok "switching tags parks the window that left"
else
  bad "switching tags left the old window on screen: $(P list | grep "$TAG_A")"
fi
if P list | grep -c "^docked.*$TAG_B" >/dev/null; then
  ok "tag play's window is docked"
else
  bad "tag play's window is not docked: $(P list | grep "$TAG_B")"
fi
PARK_PCT=$(onscreen_pct "$TAG_A")
if [ "$PARK_PCT" -le 25 ]; then
  ok "the previous tag's window is off the workspace (${PARK_PCT}% visible)"
else
  bad "the previous tag's window stayed on screen (${PARK_PCT}% visible)"
fi
check_geometry "on tag play"

# The escape hatch: every view again, with nothing hidden.
P tag-view all >/dev/null 2>&1; sleep 0.6
check_eq "0" "$(P list | grep -c '^parked')" "tag-view all shows every window"
if P status | grep -c '· tag ' >/dev/null; then
  bad "status still names a workspace after tag-view all"
else
  ok "status reports no active workspace after tag-view all"
fi
check_geometry "after tag-view all"

# The document round-trips: a restart keeps both tags and the active one.
P tag-view work >/dev/null 2>&1; sleep 0.5
P stop >/dev/null 2>&1
for _ in $(seq 1 40); do P status >/dev/null 2>&1 || break; sleep 0.1; done
PERGOLA_DEBUG=1 "$BIN" --socket "$SOCK" daemon >>"$TMP/daemon.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 50); do P status >/dev/null 2>&1 && break; sleep 0.1; done
sleep 0.4
if P tag-view | grep -c '^\* work' >/dev/null; then
  ok "the active tag survives a daemon restart"
else
  bad "the active tag was lost: $(P tag-view)"
fi
if P list | grep -c "^parked.*$TAG_B" >/dev/null; then
  ok "the other tag's window is still parked after a restart"
else
  bad "the tag sets did not survive a restart: $(P list | grep "$TAG_B")"
fi

# A status bar learns about a workspace switch from the event stream.
"$BIN" --socket "$SOCK" subscribe >"$TMP/tag-sub.jsonl" 2>/dev/null & TAG_SUB=$!
PIDS+=("$TAG_SUB")
sleep 0.4
P tag-view play >/dev/null 2>&1; sleep 0.5
kill "$TAG_SUB" 2>/dev/null
wait "$TAG_SUB" 2>/dev/null || true
if grep -c '"type":"tag"' "$TMP/tag-sub.jsonl" >/dev/null; then
  ok "a workspace switch is published to subscribers"
else
  bad "no tag record in the event stream: $(head -2 "$TMP/tag-sub.jsonl" 2>/dev/null | tr '\n' ' ')"
fi
check_eq "1" "$(P rects | grep -c .)" "the subscriber switch landed on tag play"

# Removing a workspace frees its windows without closing them.
P tag-view all >/dev/null 2>&1; sleep 0.4
P tag-remove play >/dev/null 2>&1; sleep 0.4
if P list | grep -c "^docked.*$TAG_B" >/dev/null; then
  ok "a removed tag leaves its windows open and untagged"
else
  bad "removing the tag lost its window: $(P list | grep "$TAG_B")"
fi
check_geometry "after tag-remove"

say
say "14. workspaces one key away"
# `tags: true` puts the nine workspace chords on the default plan, so a person
# who has used any tiling desktop already knows the keys. The chords are pressed
# for real: the assertions read the daemon's answer and the screen, not the
# keymap the config claims.
printf '%s' '{"tags":true,"hotkeys":true,"auto_dock":"off"}' \
  > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1; sleep 0.4
if P keys | grep -c 'tag-view 1$' >/dev/null; then
  ok "tags: true puts the workspace chords on the plan"
else
  bad "no tag-view chord after tags: true: $(P keys | tail -3 | tr '\n' ' ')"
fi
if P keys | grep -c 'window-tag 9$' >/dev/null; then
  ok "every workspace number is bound, not only the first"
else
  bad "the ninth workspace has no chord: $(P keys | grep window-tag)"
fi
TAG1_CHORD=$(command_chord 'tag-view 1') || fail "no chord installed for tag-view 1"
MOVETAG1_CHORD=$(command_chord 'window-tag 1') || fail "no chord installed for window-tag 1"

# Every window is shown before the chords run, so the switch has something to
# park and the numbers mean what the tag report says.
P tag-view all >/dev/null 2>&1; sleep 0.4
xdotool windowactivate "$TAG_A" >/dev/null 2>&1; sleep 0.4
: > "$PERGOLA_NOTIFY_LOG"
xdotool key "$TAG1_CHORD"; sleep 0.7
if P status | grep -c '· tag 1' >/dev/null; then
  ok "the workspace chord switched to tag 1"
else
  bad "$TAG1_CHORD did not switch workspaces: $(P status)"
fi
if P list | grep -c '^docked' >/dev/null; then
  bad "the new workspace shows a window it does not own: $(P list | grep '^docked')"
else
  ok "a brand-new workspace starts empty"
fi
for _ in $(seq 1 20); do [ -s "$PERGOLA_NOTIFY_LOG" ] && break; sleep 0.1; done
if grep -c 'tag 1 is empty' "$PERGOLA_NOTIFY_LOG" >/dev/null; then
  ok "the workspace switch names itself in a notification"
else
  bad "no notification for the switch: $(cat "$PERGOLA_NOTIFY_LOG" 2>/dev/null)"
fi

# Moving a window onto the workspace you are on brings it straight back on
# screen: the same key with Shift is the "take this window with me" form.
: > "$PERGOLA_NOTIFY_LOG"
xdotool key "$MOVETAG1_CHORD"; sleep 0.7
if P list | grep -c "^docked.*$TAG_A.*\[tag 1\]" >/dev/null; then
  ok "the move chord put the focused window on tag 1"
else
  bad "the move chord did not move $TAG_A: $(P list | grep "$TAG_A")"
fi
# A keypress is handled asynchronously: the move is committed when the daemon
# answers `list`, but the *next* query can still race the apply. Give the move
# a moment to reach the screen; `check_geometry` names its own cause if the
# active tag still has nothing to measure.
for _ in $(seq 1 20); do
  [ -n "$(P rects)" ] && break
  sleep 0.1
done
check_geometry "after moving onto tag 1 by chord"

# And the neighbouring workspace is one key away in both directions.
# And the neighbouring workspace is one key away in both directions.
xdotool key "$(command_chord 'tag-view 2')"
for _ in $(seq 1 40); do
  P tag-view | grep -q '^\* 2 ' && break
  sleep 0.1
done
if P tag-view | grep -c '^\* 2 ' >/dev/null; then
  ok "the next number is its own workspace"
else
  bad "tag 2 is not active: $(P tag-view)"
fi
PARK_PCT=""
for _ in $(seq 1 30); do
  PARK_PCT=$(onscreen_pct "$TAG_A")
  [ -n "$PARK_PCT" ] && [ "$PARK_PCT" -le 25 ] && break
  sleep 0.1
done
if [ "$PARK_PCT" -le 25 ]; then
  ok "the window from tag 1 is off screen (${PARK_PCT}% visible)"
else
  bad "the window from tag 1 stayed on screen (${PARK_PCT}% visible)"
fi
xdotool key "$TAG1_CHORD"
# The chord is handled asynchronously; the window has to be back on screen
# before `docked` is claimed. Poll the daemon instead of sleeping 0.6 s
# (measured 2026-11-19, #231: a slow apply read as "the window was lost").
for _ in $(seq 1 40); do
  P list | grep -q "^docked.*$TAG_A" && break
  sleep 0.1
done
if P list | grep -c "^docked.*$TAG_A" >/dev/null; then
  ok "the same chord returns to the window that was left"
else
  bad "returning to tag 1 lost its window: $(P list | grep "$TAG_A")"
fi

# Turning the key off releases exactly the workspace chords and nothing else.
printf '%s' '{"tags":false,"hotkeys":true,"auto_dock":"off"}' \
  > "$XDG_CONFIG_HOME/pergola/config.json"
"$BIN" --socket "$SOCK" reload >/dev/null 2>&1; sleep 0.4
if P keys | grep -c 'tag-view' >/dev/null; then
  bad "tags: false left a workspace chord bound: $(P keys | grep tag-view)"
else
  ok "tags: false releases the workspace chords"
fi
if P keys | grep -c 'Super+Left' >/dev/null && P keys | grep -c 'dock left' >/dev/null; then
  ok "the rest of the plan survives the workspace chords leaving"
else
  bad "turning tags off damaged the plan: $(P keys | head -3 | tr '\n' ' ')"
fi
P tag-view all >/dev/null 2>&1; sleep 0.4
check_geometry "after the workspace chords are released"

say
say "15. a window on another host desktop is left alone"
# The host WM's desktops are not pergola's tags: a window the user sends to
# another desktop belongs to the WM there. `arrange` must not drag it back —
# the measured `pi-tile --park` failure (#114) was exactly this. i3's workspaces
# are its own model, so the check is skipped there.
if [ "$WM" = "i3" ]; then
  say "  (i3 workspaces are not host desktops; skipping)"
else
  DESKS=$(xprop -root _NET_NUMBER_OF_DESKTOPS 2>/dev/null | awk '{print $NF}')
  if [ "${DESKS:-0}" -ge 2 ] 2>/dev/null; then
    xdotool windowactivate "$TAG_A" >/dev/null 2>&1; sleep 0.3
    wmctrl -i -r "$TAG_A" -t 1 >/dev/null 2>&1; sleep 0.6
    DESK=$(xprop -id "$TAG_A" _NET_WM_DESKTOP 2>/dev/null | awk '{print $NF}')
    if [ "${DESK:-0}" = "1" ]; then
      P arrange >/dev/null 2>&1; sleep 0.6
      DESK2=$(xprop -id "$TAG_A" _NET_WM_DESKTOP 2>/dev/null | awk '{print $NF}')
      check_eq "1" "${DESK2:-0}" "arrange leaves a tiled window on another host desktop"
      # The harder case is a *floating* window: `arrange` re-docks it, and the
      # raise that follows used to make xfwm4 map it on this desktop again
      # (measured 2026-11-17, the `pi-tile --park` repro). Float it first, then
      # park it, then arrange: it must stay where the WM put it.
      wmctrl -i -r "$TAG_A" -t 0 >/dev/null 2>&1; sleep 0.5
      P float --window "$TAG_A" >/dev/null 2>&1; sleep 0.4
      wmctrl -i -r "$TAG_A" -t 1 >/dev/null 2>&1; sleep 0.6
      DESK3=$(xprop -id "$TAG_A" _NET_WM_DESKTOP 2>/dev/null | awk '{print $NF}')
      if [ "${DESK3:-0}" = "1" ]; then
        P arrange >/dev/null 2>&1; sleep 0.6
        DESK4=$(xprop -id "$TAG_A" _NET_WM_DESKTOP 2>/dev/null | awk '{print $NF}')
        check_eq "1" "${DESK4:-0}" "arrange leaves a floating window on another host desktop"
      else
        say "  (the WM did not move the floating window to desktop 1; skipping)"
      fi
      # Bring it back: the tile is waiting, and the next arrange lands it there.
      wmctrl -i -r "$TAG_A" -t 0 >/dev/null 2>&1; sleep 0.5
      P arrange >/dev/null 2>&1; sleep 0.6
      check_geometry "after the window returns to this desktop"
    else
      say "  (the WM did not move the window to desktop 1; skipping)"
    fi
  else
    say "  (the WM has no second desktop; skipping)"
  fi
fi

say
say "16. a failed hotkey action reaches a desktop notification"
# A chord runs inside the daemon, where stderr goes to the journal — the one
# place a person does not look. A command that fails (here: a dock edge the
# daemon does not know) must still reach the person, so the daemon also sends
# it as a desktop notification. The sandbox-wide stub notifier records the call.
: > "$PERGOLA_NOTIFY_LOG"
"$BIN" --socket "$SOCK" bind "dock sideways" Super+Escape >/dev/null 2>&1 || fail "cannot bind the failing command"
sleep 0.4
FAIL_CHORD=$(command_chord 'dock sideways') || fail "the failing command has no chord"
xdotool key "$FAIL_CHORD"; sleep 0.7
for _ in $(seq 1 20); do [ -s "$PERGOLA_NOTIFY_LOG" ] && break; sleep 0.1; done
if grep -c 'dock sideways' "$PERGOLA_NOTIFY_LOG" >/dev/null; then
  ok "a failed hotkey action also reaches a desktop notification"
else
  bad "no notification for the failed hotkey: $(cat "$PERGOLA_NOTIFY_LOG" 2>/dev/null)"
fi
# Leave the plan as it was: the screenshot and anyone reading the config must
# not find a chord that only this test wanted.
"$BIN" --socket "$SOCK" unbind "dock sideways" >/dev/null 2>&1 || fail "cannot unbind the failing command"
sleep 0.3

say
say "17. screenshot"
import -window root "$TMP/shot.png" 2>/dev/null && cp "$TMP/shot.png" /tmp/pergola-e2e-shot.png && ok "screenshot at /tmp/pergola-e2e-shot.png"

# Undo can restore a snapshot made before a window existed. The new window
# must remain managed and retain its workspace even if a prior snapshot is
# restored. Run this after the other workspace checks so undo's history does
# not alter their starting layout.
P tag-view work >/dev/null 2>&1; sleep 0.4
P tag-view play >/dev/null 2>&1; sleep 0.4
xterm -T pergola-e2e-undo-tag -e sh -c 'exec sleep 3600' >/dev/null 2>&1 & NEW_TAG_PID=$!
XPIDS+=("$NEW_TAG_PID")
sleep 1
NEW_TAG_WINDOW=$(xdotool search --name '^pergola-e2e-undo-tag$' 2>/dev/null | head -1)
[ -n "$NEW_TAG_WINDOW" ] || fail "the post-snapshot workspace window did not appear"
NEW_TAG_WINDOW=$(printf '0x%08x' "$NEW_TAG_WINDOW")
for _ in $(seq 1 50); do
  P list | grep -q "^floating.*$NEW_TAG_WINDOW.*\[tag play\]" && break
  sleep 0.1
done
for _ in $(seq 1 8); do
  P undo >/dev/null 2>&1; sleep 0.3
  P status | grep -q 'tag work' && break
done
if P status | grep -q 'tag work' &&
   P list | grep -q "^parked.*$NEW_TAG_WINDOW.*\[tag play\]"; then
  ok "undo keeps a newer live window on its original workspace"
else
  bad "undo moved or lost the newer workspace window: $(P status); $(P list | grep "$NEW_TAG_WINDOW")"
fi
kill "$NEW_TAG_PID" 2>/dev/null || true
sleep 0.4

say
printf 'e2e: %d passed, %d failed (wm=%s)\n' "$PASS" "$FAIL" "$WM"
[ "$FAIL" -eq 0 ]

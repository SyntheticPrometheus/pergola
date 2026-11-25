#!/usr/bin/env bash
# Pergola health check: the soak test in one command.
#
#   scripts/health.sh              check the running daemon and its layout
#   scripts/health.sh --e2e        also run the full regression suite (~3 min)
#   scripts/health.sh --e2e --wm openbox   run that suite under a second WM
#   scripts/health.sh --quiet      print nothing when healthy (exit code still tells)
#
# With --e2e the suite must judge the tree's HEAD: if the binary under test was
# built from another commit it is rebuilt first (cargo build --release, logged to
# /tmp/pergola-health-build.log). PERGOLA_NO_BUILD=1 or an explicit PERGOLA_BIN
# turns that off, so a release artifact can be measured without being rebuilt.
#   scripts/health.sh --json       one machine-readable line on stdout
#
# Checks, in order: daemon answers, CPU under load, layout invariants, geometry
# drift against what the X server reports, docked windows that no longer exist.
# Exits 1 on any failure, so a timer or a scheduler can act on it.
set -uo pipefail

# When installed as `pergola-health` the source tree is not next to the script;
# find it by walking up, then by PERGOLA_REPO, then the usual checkout path.
find_repo() {
  local d
  d="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" 2>/dev/null && pwd)"
  while [ -n "$d" ] && [ "$d" != "/" ]; do
    [ -x "$d/scripts/e2e.sh" ] && { printf '%s' "$d"; return 0; }
    d="$(dirname "$d")"
  done
  for cand in "${PERGOLA_REPO:-}" "$HOME/Projects/pergola"; do
    [ -n "$cand" ] && [ -x "$cand/scripts/e2e.sh" ] && { printf '%s' "$cand"; return 0; }
  done
  return 1
}
ROOT="$(find_repo || printf '%s' "$(pwd)")"
# Measure the tree being tested, not whatever was installed last. 2026-11-17: an e2e run
# against merged main used ~/.local/bin/pergola built six hours before the merge and
# reported a tag regression that the merge had already fixed (INC in pergola's TODO).
if [ -n "${PERGOLA_BIN:-}" ]; then
  BIN="$PERGOLA_BIN"
elif [ -x "$ROOT/target/release/pergola" ]; then
  BIN="$ROOT/target/release/pergola"
else
  BIN="$(command -v pergola || echo "$ROOT/target/release/pergola")"
fi
LOG="${PERGOLA_HEALTH_LOG:-${XDG_STATE_HOME:-$HOME/.local/state}/pergola/health.jsonl}"
E2E=0
QUIET=0
JSON=0
# The e2e suite is the only check that needs a WM; record which one ran so a
# green line is not mistaken for "measured on every desktop".
WM="${PERGOLA_TEST_WM:-xfwm4}"
while [ $# -gt 0 ]; do
  case "$1" in
    --e2e) E2E=1 ;;
    --wm) WM="${2:-}"; shift ;;
    --wm=*) WM="${1#--wm=}" ;;
    --quiet) QUIET=1 ;;
    --json) JSON=1 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "health: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

FAILURES=()
CHECKS=()
WARNINGS=()
note() { [ "$QUIET" -eq 1 ] || printf '%s\n' "$*"; }
pass() { CHECKS+=("$1"); }
fail() { FAILURES+=("$1"); CHECKS+=("FAIL $1"); note "  FAIL $1"; }
# A live-desktop observation (another session may be dragging windows right now) is
# not this suite's verdict. On 2026-11-15 a drift/CPU failure at 09:00 failed the
# weekly e2e obligation while two sessions were tiling the same display, and the
# suite itself passed 43/43 an hour later. With --e2e these become warnings; the
# 12-hour soak (no --e2e) keeps the strict verdict it exists for.
live_fail() {
  if [ "$E2E" -eq 1 ]; then
    WARNINGS+=("$1"); CHECKS+=("WARN $1")
    note "  WARN $1 (live desktop, not the e2e verdict)"
  else
    fail "$1"
  fi
}
check() { # name, command
  if eval "$2" >/dev/null 2>&1; then pass "$1"; note "  ok   $1"; else fail "$1"; fi
}

[ -x "$BIN" ] || { echo "health: no pergola binary (set PERGOLA_BIN)" >&2; exit 1; }
note "pergola health: $BIN"

# 1. the daemon answers ------------------------------------------------------
STATUS="$(timeout 5 "$BIN" status 2>&1)"
if [ $? -ne 0 ] || [ -z "$STATUS" ]; then
  fail "daemon does not answer: $STATUS"
  STATUS=""
else
  pass "daemon answers"
  note "  ok   daemon answers"
fi

# 2. CPU ---------------------------------------------------------------------
PID="$(pgrep -x pergola | head -1)"
CPU="0.0"
if [ -n "${PID:-}" ]; then
  read -r utime1 stime1 <<<"$(awk '{print $14, $15}' /proc/"$PID"/stat)"
  sleep 2
  read -r utime2 stime2 <<<"$(awk '{print $14, $15}' /proc/"$PID"/stat)"
  CPU=$(python3 -c "print(round((($utime2+$stime2)-($utime1+$stime1))/2.0,1))")
  if python3 -c "import sys; sys.exit(0 if $CPU < 10.0 else 1)"; then
    pass "cpu ${CPU}%"
    note "  ok   cpu ${CPU}% of one core"
  else
    live_fail "cpu ${CPU}% of one core (>10%)"
  fi
else
  fail "no pergola daemon process"
fi

# 3. layout invariants -------------------------------------------------------
TREE="$("$BIN" tree 2>/dev/null)"
INVARIANTS="$(printf '%s' "$TREE" | python3 -c '
import json, sys
problems, ids, desktops = [], [], 0
try:
    doc = json.load(sys.stdin)
except Exception as exc:
    print("unreadable layout json: %s" % exc); raise SystemExit(0)
root = doc.get("doc", {}).get("root")
def walk(n):
    global desktops
    nid = n.get("id")
    ids.append(nid)
    kind = n.get("kind")
    if kind == "desktop":
        desktops += 1
    elif kind == "split":
        kids, weights = n.get("children") or [], n.get("weights") or []
        if len(kids) != len(weights):
            problems.append("%s: %d children vs %d weights" % (nid, len(kids), len(weights)))
        if kids and abs(sum(weights) - 1.0) > 1e-6:
            problems.append("%s: weights sum to %.4f" % (nid, sum(weights)))
        if any(w < 0 for w in weights):
            problems.append("%s: negative weight" % nid)
        for k in kids:
            walk(k)
    elif kind == "panel":
        views = n.get("views") or []
        if not views:
            problems.append("%s: panel with no views" % nid)
if root is None:
    problems.append("no doc.root")
else:
    walk(root)
if desktops != 1:
    problems.append("%d desktop leaves, expected 1" % desktops)
if len(ids) != len(set(ids)):
    problems.append("duplicate node ids")
print("; ".join(problems) if problems else "")
')"
INV_RC=$?
if [ "$INV_RC" -eq 0 ] && [ -z "$INVARIANTS" ]; then
  pass "layout invariants"
  note "  ok   layout invariants"
else
  fail "layout invariants: ${INVARIANTS:-checker failed (rc=$INV_RC)}"
fi

# 3b. tiles stay inside the work area ---------------------------------------
# The class this guards: laying out in the whole display instead of
# _NET_WORKAREA, which puts every title bar under a top panel.
if command -v xprop >/dev/null; then
  DESKTOP=$(xprop -root _NET_CURRENT_DESKTOP 2>/dev/null | sed -n 's/.*= \([0-9]*\).*/\1/p')
  WA=$(xprop -root _NET_WORKAREA 2>/dev/null | sed -n 's/.*= //p' | tr ',' '\n' | tr -d ' ' | sed -n "$(( ${DESKTOP:-0} * 4 + 1 )),$(( ${DESKTOP:-0} * 4 + 4 ))p" | tr '\n' ' ')
  read -r wx wy ww wh <<<"${WA:-0 0 0 0}"
  if [ "${ww:-0}" -gt 0 ] && [ "${wh:-0}" -gt 0 ]; then
    OUTSIDE=""
    while read -r id x y w h; do
      [ -z "${id:-}" ] && continue
      if [ "$x" -lt "$wx" ] || [ "$y" -lt "$wy" ] \
         || [ $((x+w)) -gt $((wx+ww)) ] || [ $((y+h)) -gt $((wy+wh)) ]; then
        OUTSIDE="$OUTSIDE $id($x,$y ${w}x$h)"
      fi
    done < <("$BIN" rects 2>/dev/null)
    if [ -z "$OUTSIDE" ]; then
      pass "tiles inside the work area"
      note "  ok   tiles inside the work area (${wx},${wy} ${ww}x${wh})"
    else
      live_fail "tiles outside the work area (${wx},${wy} ${ww}x${wh}):$OUTSIDE"
    fi
  else
    note "  --   work-area check skipped (no _NET_WORKAREA)"
  fi
fi

# 4. geometry drift ----------------------------------------------------------
DRIFT=""
DRIFT_COUNT=0
if command -v xdotool >/dev/null && command -v xprop >/dev/null; then
  SCREEN="$(printf '%s\n' "$STATUS" | grep -oE '[0-9]+x[0-9]+' | head -1)"
  SCREEN_W="${SCREEN%x*}"
  SCREEN_H="${SCREEN#*x}"
  : "${SCREEN_W:=99999}" "${SCREEN_H:=99999}"
  "$BIN" rects 2>/dev/null > /tmp/pergola-health-rects.$$
  # A window on another desktop is not placed by the daemon at all — only the
  # visible desktop is laid out — so its X rect is wherever the WM last left
  # it, and `rects` still reports the rect its tree would give it. Comparing
  # the two calls a healthy window drift (measured 2026-11-20: a terminal on
  # desktop 2 failed the check after a daemon restart, and the failure repeats
  # on every run until the desktop is shown). The desktop of a window is read
  # from the X server, not from pergola, so this skip cannot hide drift the
  # daemon itself invented on the desktop it is laying out.
  HERE=$(xprop -root _NET_CURRENT_DESKTOP 2>/dev/null | sed -n 's/.*= \([0-9]*\).*/\1/p')
  while read -r id x y w h; do
    [ -z "${id:-}" ] && continue
    win="0x${id#p-}"
    wd=$(xprop -id "$win" _NET_WM_DESKTOP 2>/dev/null | sed -n 's/.*= \([0-9]*\).*/\1/p')
    if [ -n "${HERE:-}" ] && [ -n "${wd:-}" ] && [ "$wd" != "$HERE" ]; then
      continue
    fi
    pos=$(xdotool getwindowgeometry "$win" 2>/dev/null | sed -n 's/.*Position: \([-0-9]*\),\([-0-9]*\).*/\1 \2/p')
    size=$(xdotool getwindowgeometry "$win" 2>/dev/null | sed -n 's/.*Geometry: \([0-9]*\)x\([0-9]*\).*/\1 \2/p')
    [ -z "$pos" ] && { DRIFT_COUNT=$((DRIFT_COUNT+1)); continue; }
    ext=$(xprop -id "$win" _NET_FRAME_EXTENTS 2>/dev/null | sed -n 's/.*= \([0-9]*\), \([0-9]*\), \([0-9]*\), \([0-9]*\)/\1 \2 \3 \4/p')
    read -r px py <<<"$pos"; read -r sw sh <<<"$size"; read -r l r t b <<<"${ext:-0 0 0 0}"
    fx=$((px-l)); fy=$((py-t)); fw=$((sw+l+r)); fh=$((sh+t+b))
    # Only assert tiles that are fully on screen: parked tiles are clamped by
    # the WM. Compare against the *work area* when it is known: rects live in
    # work-area coordinates, so with a top panel y starts at wy and y+h equals
    # wy+wh — a display-height check would call every full-height tile off-screen
    # and silently skip the drift it exists to catch.
    if [ -n "${ww:-}" ] && [ "${ww:-0}" -gt 0 ]; then
      if [ "$x" -lt "${wx:-0}" ] || [ "$y" -lt "${wy:-0}" ] \
         || [ $((x+w)) -gt $(( ${wx:-0} + ww )) ] || [ $((y+h)) -gt $(( ${wy:-0} + wh )) ]; then
        continue
      fi
    elif [ "$x" -lt 0 ] || [ "$y" -lt 0 ] || [ $((x+w)) -gt "$SCREEN_W" ] || [ $((y+h)) -gt "$SCREEN_H" ]; then
      continue
    fi
    # A client with a character grid (every terminal) snaps its size to
    # WM_NORMAL_HINTS resize increments, so the honest tolerance for a tile's
    # width/height is its own increment, never less than 2 px.
    inc=$(xprop -id "$win" WM_NORMAL_HINTS 2>/dev/null | sed -n 's/.*resize increment: \([0-9]*\) by \([0-9]*\).*/\1 \2/p')
    read -r iw ih <<<"${inc:-0 0}"
    [ "${iw:-0}" -gt 2 ] || iw=2
    [ "${ih:-0}" -gt 2 ] || ih=2
    # Compare the four edges, not the origin: a resize-increment client can be
    # snapped to a size one increment larger, and the WM then anchors it at the
    # requested client position, so the frame origin moves by up to one
    # increment while the tile still covers its area. Edge tolerance = the
    # client's own increment keeps that honest and still catches real drift
    # (a maximised window is off by the whole tile width).
    failed=0
    for pair in \
        "$((fx-x)):$iw" "$((fy-y)):$ih" \
        "$(((fx+fw)-(x+w))):$iw" "$(((fy+fh)-(y+h))):$ih"; do
      d="${pair%:*}"; tol="${pair##*:}"
      if [ "${d#-}" -gt "$tol" ]; then failed=1; fi
    done
    if [ "$failed" -eq 1 ]; then
      DRIFT_COUNT=$((DRIFT_COUNT+1))
      DRIFT="$DRIFT $id(expected $x,$y ${w}x$h got $fx,$fy ${fw}x$fh)"
    fi
  done < /tmp/pergola-health-rects.$$
  rm -f /tmp/pergola-health-rects.$$
  if [ "$DRIFT_COUNT" -eq 0 ]; then
    pass "no geometry drift"
    note "  ok   no geometry drift"
  else
    live_fail "geometry drift on $DRIFT_COUNT panel(s):$DRIFT"
  fi
else
  note "  --   drift skipped (xdotool/xprop missing)"
fi

# 5. docked windows that no longer exist ------------------------------------
if command -v wmctrl >/dev/null && [ -n "$STATUS" ]; then
  LIVE="$(wmctrl -l | awk '{print tolower($1)}' | sort -u)"
  ORPHANS=""
  for win in $("$BIN" list 2>/dev/null | awk '$1=="docked"{print tolower($2)}'); do
    printf '%s\n' "$LIVE" | grep -qx "$win" || ORPHANS="$ORPHANS $win"
  done
  if [ -z "$ORPHANS" ]; then
    pass "no orphaned docked windows"
    note "  ok   no orphaned docked windows"
  else
    live_fail "docked panels whose window is gone:$ORPHANS"
  fi
else
  note "  --   orphan check skipped (wmctrl missing)"
fi

# 6. optional regression suite ----------------------------------------------
if [ "$E2E" -eq 1 ]; then
  HEAD="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  # Provenance: the binary's build commit must be the tree's HEAD, or the suite
  # is judging a different tree than it names. Say which binary and commit ran.
  stamp_of() { "$1" --version 2>/dev/null | sed -n 's/.*(\([^)]*\)).*/\1/p'; }
  BIN_STAMP="$(stamp_of "$BIN")"
  STALE=""
  if [ -z "$BIN_STAMP" ]; then
    STALE="$BIN carries no build commit"
  elif [ "$HEAD" != "unknown" ] && [ "$BIN_STAMP" != "$HEAD" ]; then
    STALE="$BIN is commit $BIN_STAMP, tree is $HEAD"
  fi
  # A stale binary is this script's problem, not the owner's memory. The check
  # refuses to judge the wrong tree (2026-11-17); refusing is still a failure, so
  # the weekly row filed a priority-1 task for a tree nobody had built after the
  # last commit (task #497, 2026-11-25). Build the tree under test instead. Not
  # when the caller named a binary or opted out: those say what is measured.
  if [ -n "$STALE" ] && [ -z "${PERGOLA_BIN:-}" ] && [ "${PERGOLA_NO_BUILD:-}" != "1" ] \
     && [ -f "$ROOT/Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
    note "  ..   $STALE — building the tree under test"
    if ( cd "$ROOT" && timeout 900 cargo build --release ) >/tmp/pergola-health-build.log 2>&1; then
      BIN="$ROOT/target/release/pergola"
      BIN_STAMP="$(stamp_of "$BIN")"
      if [ "$BIN_STAMP" = "$HEAD" ]; then
        STALE=""
        note "  ok   built $BIN at $BIN_STAMP"
      else
        STALE="$BIN still carries $(stamp_of "$BIN" || echo no) commit after a build, tree is $HEAD"
      fi
    else
      STALE="$STALE — cargo build --release failed (see /tmp/pergola-health-build.log)"
    fi
  fi
  if [ ! -x "$ROOT/scripts/e2e.sh" ]; then
    fail "e2e suite not found (set PERGOLA_REPO to the checkout)"
  elif [ -n "$STALE" ]; then
    fail "e2e refused: $STALE — build the tree under test (cargo build --release) or point PERGOLA_BIN at its build"
  elif PERGOLA_TEST_WM="$WM" PERGOLA_BIN="$BIN" "$ROOT/scripts/e2e.sh" >/tmp/pergola-e2e-health.log 2>&1; then
    SUMMARY="$(grep -E '^e2e:' /tmp/pergola-e2e-health.log | tail -1)"
    pass "e2e suite ($SUMMARY) on $HEAD with $BIN"
    note "  ok   e2e suite ($SUMMARY) on $HEAD with $BIN"
  else
    fail "e2e suite failed on $HEAD with $BIN (see /tmp/pergola-e2e-health.log)"
  fi
fi

# record ---------------------------------------------------------------------
TS=$(date +%s)
if [ -n "$STATUS" ]; then
  mkdir -p "$(dirname "$LOG")"
  printf '{"ts":%s,"ok":%s,"cpu":%s,"drift":%s,"failures":%s,"warnings":%s,"e2e":%s,"wm":%s}\n' \
    "$TS" "$([ ${#FAILURES[@]} -eq 0 ] && echo true || echo false)" \
    "${CPU:-0}" "$DRIFT_COUNT" \
    "$(printf '%s\n' "${FAILURES[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))')" \
    "$(printf '%s\n' "${WARNINGS[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))')" \
    "$E2E" "$(printf '%s' "$WM" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')" >>"$LOG"
fi

if [ "$JSON" -eq 1 ]; then
  printf '{"ok":%s,"cpu":%s,"drift":%s,"checks":%s,"failures":%s,"warnings":%s,"wm":%s}\n' \
    "$([ ${#FAILURES[@]} -eq 0 ] && echo true || echo false)" "${CPU:-0}" "$DRIFT_COUNT" \
    "$(printf '%s\n' "${CHECKS[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))')" \
    "$(printf '%s\n' "${FAILURES[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))')" \
    "$(printf '%s\n' "${WARNINGS[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))')" \
    "$(printf '%s' "$WM" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')" 
fi

if [ ${#FAILURES[@]} -gt 0 ]; then
  [ "$QUIET" -eq 1 ] && printf 'pergola health: %d failure(s), %d warning(s)\n' "${#FAILURES[@]}" "${#WARNINGS[@]}" >&2
  exit 1
fi
[ "$QUIET" -eq 1 ] || { [ ${#WARNINGS[@]} -eq 0 ] && echo "pergola health: ok" || printf 'pergola health: ok (%d warning(s))\n' "${#WARNINGS[@]}"; }
exit 0

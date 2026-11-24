#!/usr/bin/env bash
# Run the e2e suite N times and fail if any run fails.
#
#   scripts/e2e-soak.sh                       xfwm4, 3 runs
#   scripts/e2e-soak.sh --wm openbox          the second-WM gate
#   scripts/e2e-soak.sh --wm=openbox --runs 5
#
# Why repetition is the gate rather than a nice-to-have: the openbox failures
# this suite once had moved between runs and moved between assertions
# (2026-11-20: {hold-chord, undo-third-tree, tag-windows} in one run,
# {ratio-nudge, hold-chord, activation-request} in the next). A suite whose
# failures migrate is reporting state churn in the daemon, and a single green
# run cannot tell a fix from a lucky schedule. `docs/VERSIONING.md` condition 4
# asks for zero failures, so it asks for this script.
#
# Each run gets its own X display (and its own sandbox), because a kept or
# stray sandbox on the same display is indistinguishable from a product bug.
# The per-run log is kept; a failing run prints its FAIL lines before exiting.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WM="xfwm4"
RUNS=3
while [ $# -gt 0 ]; do
  case "$1" in
    --wm) WM="${2:-}"; shift 2 ;;
    --wm=*) WM="${1#--wm=}"; shift ;;
    --runs) RUNS="${2:-}"; shift 2 ;;
    --runs=*) RUNS="${1#--runs=}"; shift ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    *) echo "e2e-soak: unknown argument $1" >&2; exit 2 ;;
  esac
done
case "$RUNS" in ''|*[!0-9]*) echo "e2e-soak: --runs needs a positive integer" >&2; exit 2 ;; esac
[ "$RUNS" -ge 1 ] || { echo "e2e-soak: --runs needs a positive integer" >&2; exit 2; }

TMP="$(mktemp -d /tmp/pergola-soak.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT
FAILED=0

for i in $(seq 1 "$RUNS"); do
  # A display is free when Xvfb left no socket for it; the suite refuses a
  # display that answers xset, so this only has to avoid the obvious collision.
  DISP=""
  for n in $(seq 80 99); do
    [ -S "/tmp/.X11-unix/X$n" ] || { DISP=":$n"; break; }
  done
  [ -n "$DISP" ] || { echo "e2e-soak: no free X display in :80..:99" >&2; exit 2; }

  LOG="$TMP/run-$i.log"
  START=$SECONDS
  PERGOLA_TEST_DISPLAY="$DISP" "$ROOT/scripts/e2e.sh" --wm "$WM" >"$LOG" 2>&1
  STATUS=$?
  ELAPSED=$((SECONDS - START))
  if [ "$STATUS" -eq 0 ]; then
    printf 'soak %d/%d  %s  %ds\n' "$i" "$RUNS" "$(grep -E '^e2e: ' "$LOG" | tail -1)" "$ELAPSED"
  else
    printf 'soak %d/%d  FAILED (exit %d)  %s  %ds\n' "$i" "$RUNS" "$STATUS" \
      "$(grep -E '^e2e: ' "$LOG" | tail -1)" "$ELAPSED"
    grep -E '^  FAIL|^e2e-soak' "$LOG" | sed 's/^/    /'
    # Keep the failing run's log even though the sandbox is gone.
    KEEP="/tmp/pergola-soak-fail-$i.log"
    cp "$LOG" "$KEEP"
    printf '    log: %s\n' "$KEEP"
    FAILED=$((FAILED + 1))
  fi
done

if [ "$FAILED" -eq 0 ]; then
  printf 'soak: %d/%d runs green (wm=%s)\n' "$RUNS" "$RUNS" "$WM"
  exit 0
fi
printf 'soak: %d of %d runs failed (wm=%s)\n' "$FAILED" "$RUNS" "$WM"
exit 1

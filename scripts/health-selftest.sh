#!/usr/bin/env bash
# Selftest for scripts/health.sh: does --e2e judge the tree's HEAD even when the
# binary on disk was built from an older commit?
#
#   scripts/health-selftest.sh
#
# It builds a throw-away checkout whose binary embeds the commit it was built
# from, moves HEAD one commit ahead (the exact state the weekly row hit on
# 2026-11-25: tree bbc2f7d, binary 61c6a54), and asserts that health.sh rebuilds
# instead of refusing. Exit 0 on pass, 1 on failure. ~10 s, no display needed.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d /tmp/pergola-health-selftest.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT INT TERM

fails=0
ok()   { printf 'ok   %s\n' "$1"; }
bad()  { printf 'FAIL %s\n' "$1"; fails=$((fails+1)); }

mkdir -p "$TMP/scripts" "$TMP/src"
cp "$ROOT/scripts/health.sh" "$TMP/scripts/health.sh"
chmod +x "$TMP/scripts/health.sh"

# The e2e suite is stubbed: this test is about which binary the suite is given,
# not about the suite itself (that is a 2-minute X run, scripts/e2e.sh).
cat >"$TMP/scripts/e2e.sh" <<'EOF'
#!/bin/sh
printf 'e2e: 1 passed, 0 failed (wm=stub)\n'
exit 0
EOF
chmod +x "$TMP/scripts/e2e.sh"

cat >"$TMP/Cargo.toml" <<'EOF'
[package]
name = "pergola"
version = "0.5.0-dev"
edition = "2021"
EOF

# build.rs embeds the commit being built, the way the real crate stamps its
# version string, so a binary can be stale in exactly the measured way.
cat >"$TMP/build.rs" <<'EOF'
fn main() {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=GIT_COMMIT={out}");
    println!("cargo:rerun-if-changed=.git");
}
EOF

cat >"$TMP/src/main.rs" <<'EOF'
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        Some("--version") => println!("pergola 0.5.0-dev ({})", env!("GIT_COMMIT")),
        Some("status") => println!("pergola live 1600x900"),
        Some("tree") => println!(r#"{{"doc":{{"root":{{"id":"d0","kind":"desktop"}}}}}}"#),
        Some("rects") | Some("list") => {}
        _ => {}
    }
}
EOF

git -C "$TMP" init -q
git -C "$TMP" -c user.name=t -c user.email=t@e add -A >/dev/null
git -C "$TMP" -c user.name=t -c user.email=t@e commit -qm first
(cd "$TMP" && cargo build --release -q) || { bad "could not build the fixture"; exit 1; }
git -C "$TMP" -c user.name=t -c user.email=t@e commit -q --allow-empty -m second
HEAD="$(git -C "$TMP" rev-parse --short HEAD)"
BUILT="$(git -C "$TMP" rev-parse --short HEAD~1)"
[ "$HEAD" != "$BUILT" ] || { bad "fixture HEAD did not move"; exit 1; }
"$TMP/target/release/pergola" --version | grep -q "($BUILT)" \
  || { bad "fixture binary does not carry $BUILT"; exit 1; }

# 1. the stale binary triggers a build, and the suite runs on the rebuilt one
out="$(cd "$TMP" && ./scripts/health.sh --e2e --wm openbox 2>&1)"
printf '%s\n' "$out" | grep -q "building the tree under test" \
  && ok "a stale binary triggers a build" || bad "no build attempt for a stale binary"
printf '%s\n' "$out" | grep -q "e2e refused" \
  && bad "health still refused instead of building" || ok "health does not refuse a stale tree it can build"
printf '%s\n' "$out" | grep -q "e2e: 1 passed, 0 failed (wm=stub)" \
  && ok "the suite ran after the rebuild" || bad "the suite did not run"
"$TMP/target/release/pergola" --version | grep -q "($HEAD)" \
  && ok "the rebuilt binary carries HEAD ($HEAD)" || bad "rebuilt binary is not at $HEAD"

# 2. PERGOLA_NO_BUILD=1 keeps the refusal (a release artifact is not rebuilt)
git -C "$TMP" -c user.name=t -c user.email=t@e commit -q --allow-empty -m second-stale
out="$(cd "$TMP" && PERGOLA_NO_BUILD=1 ./scripts/health.sh --e2e 2>&1)"
printf '%s\n' "$out" | grep -q "e2e refused" \
  && ok "PERGOLA_NO_BUILD=1 refuses a stale binary instead of building" \
  || bad "PERGOLA_NO_BUILD=1 did not refuse"

# 3. a matching binary is not rebuilt (no wasted cargo run on the weekly row)
git -C "$TMP" -c user.name=t -c user.email=t@e commit -q --allow-empty -m third
(cd "$TMP" && cargo build --release -q >/dev/null 2>&1)
before="$(cd "$TMP" && ./scripts/health.sh --e2e 2>&1)"
printf '%s\n' "$before" | grep -q "building the tree under test" \
  && bad "rebuilds a binary that already matches HEAD" || ok "a matching binary is not rebuilt"

if [ "$fails" -ne 0 ]; then
  printf 'health-selftest: %d failure(s)\n' "$fails"
  exit 1
fi
printf 'health-selftest: ok\n'

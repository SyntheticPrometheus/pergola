//! Stamp the binary with the commit it was built from.
//!
//! Measured 2026-11-17: an e2e run on merged main used `~/.local/bin/pergola`
//! built six hours earlier and reported a tag regression the merge had already
//! fixed — the suite judged a different tree than the one it named. A binary
//! that cannot say which commit it is cannot be a measurement, so `--version`
//! carries it and `scripts/health.sh` refuses a mismatch.
use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=PERGOLA_COMMIT={commit}");
    // A commit on a branch updates `.git/refs/heads/*`, not `.git/HEAD` (which
    // still says `ref: refs/heads/main`), so watching HEAD alone missed every
    // commit: `cargo build --release` then reported "Finished" without
    // re-stamping, and the binary kept claiming the previous commit (measured
    // 2026-11-25, task #497). Watch the refs too; `index` churn is left out on
    // purpose so `git status` does not force a rebuild.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}

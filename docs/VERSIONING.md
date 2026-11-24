# Versioning rules

Pergola starts at **`0.0.0-dev`** and uses `MAJOR.MINOR.PATCH[-dev]`.

A version **without** `-dev` is a *stable release*. A version **with** `-dev` is
a development build: it may change behaviour between commits and makes no
compatibility promise, even within the same minor.

```
0.0.0-dev → 0.1.0-dev → 0.1.1-dev → … → 1.0.0        ← first stable release
                                        ↓
1.1.0-dev → 1.2.0-dev → … → 2.0.0                    ← next stable release
                                        ↓
2.1.0-dev → … → 3.0.0
```

## What each number means

| Change | Bump | Example |
|---|---|---|
| **Fix** — a bug fix, a corrected message, an internal refactor, docs, dependency bump with no behaviour change | `PATCH` | `0.1.0-dev` → `0.1.1-dev` |
| **Feature** — a new command, a new key binding, a new config key, a new document field, a behaviour change users can notice | `MINOR` | `0.1.1-dev` → `0.2.0-dev` |
| **Breaking** — a command removed or renamed, a document/CLI/IPC format that older files or scripts cannot read, a changed default that alters existing layouts | `MAJOR` (after 1.0.0) | `1.4.0-dev` → `2.0.0` |

While `MAJOR` is `0`, breaking changes are allowed at any `MINOR` bump — that is
the point of `0.x`. After `1.0.0`, breaking changes are *only* allowed at a
`MAJOR` bump, and a `MAJOR` bump is only ever cut as a stable release.

## The 1.0.0 gate

`MAJOR` becomes `1` — and the first `-dev` suffix is dropped — only when all of
the following hold:

1. **Scope complete**: every command in `README.md` works on at least two
   different EWMH window managers (checked: xfwm4, plus a second WM from
   mutter/kwin_x11/i3/openbox).
2. **No known reproducible bugs**: no open issue that can be reproduced on a
   clean installation.
3. **Frozen interfaces**: the CLI, the Unix-socket IPC request/response shape,
   and `layout.json` (`version: 2`) are documented and stable; a document written
   by `1.0.0` loads in every later `1.x`.
4. **Green gates**: `cargo test` and `scripts/e2e.sh` pass with zero failures on
   the release commit, from a clean checkout — and the second-WM run is
   repeated: `scripts/e2e-soak.sh --wm openbox --runs 3` exits 0. One green
   second-WM run is not evidence, because the failures this suite has had under
   Openbox moved between runs and assertions (2026-11-20: `back`, `undo`,
   `tag-windows` in one run, `ratio`, `back`, activation in the next).
5. **Documented limitations** in `README.md` are accurate (no silent gaps).

Until then the project stays `0.x.y-dev`.

## After 1.0.0

* Each `MAJOR` line has exactly **one** stable tag (`2.0.0`, `3.0.0`, …).
* Everything between two stable tags is a **dev** build: `MINOR` for features,
  `PATCH` for fixes. There are no `2.0.1`-style stable patches; a fix after
  `2.0.0` is `2.1.1-dev`, and the next stable release is `3.0.0`.
* The document format carries its own `version` field, independent of the crate
  version. A document format bump is always a `MAJOR` change; the loader keeps
  reading older formats.

## Checklist for a version bump

1. Decide the size using the table above (fix → feature → breaking).
2. Bump `[workspace.package] version` in `Cargo.toml` — nothing else.
3. Update the version shown in `README.md` and `CHANGELOG.md`.
4. Run `cargo test` and `scripts/e2e.sh`; a bump with a red gate is not a bump.
5. Tag only stable releases: `git tag -s v1.0.0`. Dev builds are commits, not
   tags.

# Funding Pergola

Pergola is MIT-licensed and stays free. It is developed and maintained by
SyntheticPrometheus; there is no company behind it and no paid tier.

If it saves you time, you can send something to the address below. Nothing is
gated on it: no feature, fix or issue reply is behind a payment, and a donation
does not buy priority.

## Where to send it

| rail | address |
| --- | --- |
| USDC on Base (ERC-20) | `0x1cfeb7eba73c1c5e71604c24f4f07e276d54a43a` |
| anything else on Base / Ethereum | same address, `0x1cfeb7eba73c1c5e71604c24f4f07e276d54a43a` |

That is a plain externally-owned account, not a contract. Verify the address on
the chain before sending: this file and the repository history are the only
place it is published, and it will not change without a commit here.

## What it pays for

- Time on the end-to-end suite: every WM × desktop combination in
  `docs/COMPATIBILITY.md` is a real X server, a real WM and real windows
  (`scripts/e2e.sh --wm <wm>`), and the ones marked not-green are the work.
- The camera and nesting model (`docs/USER-GUIDE.md`, `crates/pergola`).
- Hardware this project does not have: the matrix is measured on one Ivy Bridge
  laptop, so a HiDPI or multi-head result is worth more than money.

## What it does not pay for

Pergola accepts no proprietary patches. Contributions are reviewed on the same
terms whether or not the author has ever sent anything: an issue or a PR is the
way in, and `BACKLOG.md` says what done looks like.

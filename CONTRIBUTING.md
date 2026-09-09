# Contributing

Thanks for considering a contribution. Kynoptic is a local-first
Windows activity layer; the review bar is correspondingly high on two
points: nothing may send data off the machine, and the raw event layer
must stay append-only.

## Getting started

Requirements: Windows 10/11 and a stable Rust toolchain (MSVC target).

```bash
git clone https://github.com/ardss/kynoptic
cd kynoptic
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

CI runs `fmt --check`, `clippy -D warnings`, and `test --workspace` on
`windows-latest`. Please make sure all three pass locally before
opening a PR.

## Ground rules

- No network calls from the collector. Local-only bindings
  (127.0.0.1) for the dashboard and MCP.
- Do not weaken input privacy: keyboard/mouse stay per-minute counts
  by default; per-key detail must remain an explicit opt-in.
- Do not add raw-layer cleanup, truncation, or re-keying. Aggregates
  are derived caches and may be rebuilt freely.
- No emoji in code, docs, or commit messages.
- Warnings are denied at the workspace level; do not add `#[allow]`
  without a comment explaining why.

## Commit style

History uses short prefixed subjects, e.g.:

- `feat:` new user-visible capability
- `fix:` bug fix
- `perf:`, `perf2:`, `perf3:` performance rounds (or one-off `perf:`)
- `qa:` tests, probes, flake fixes
- `law:` raw-data iron-law enforcement changes
- `dash:` dashboard work
- `docs:` documentation only
- `monitors:`, `privacy:`, `monitors:`/`v0.1` scope prefixes where a
  single area dominates

Keep the subject one line, specific, and factual; put measurement
numbers in the body when relevant.

## Pull requests

- Target the `main` branch.
- Include tests for behavior changes; benchmark harness changes should
  note the measured before/after in the description.
- Update `BENCHMARKS.md` and `APP.md` if numbers or user-facing
  behavior change, and add an entry under `Unreleased` in
  `CHANGELOG.md`.

## Reporting issues

Use the issue templates (`.github/ISSUE_TEMPLATE`). Security issues go
through [SECURITY.md](SECURITY.md), not public issues.

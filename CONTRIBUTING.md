# Contributing & Engineering Standards

Kynoptic is a local-first Windows activity layer; the review bar is high
on two points: nothing may send data off the machine, and the raw event
layer must stay append-only.

Effective since v0.2.2: **`main` is push-protected — every change goes
through a pull request**, including maintainer changes. This document is
the normative process; CI is the gate.

## 1. Branch & PR flow

1. Branch from `main`; name it `feat/<topic>`, `fix/<topic>`,
   `docs/<topic>`, `chore/<topic>`.
2. Follow Conventional Commits (section 2).
3. Open a PR against `main` and fill in
   `.github/PULL_REQUEST_TEMPLATE.md`.
4. CI must be green: `cargo fmt --check`, `cargo clippy --workspace
   --all-targets -- -D warnings`, `cargo test --workspace`,
   `node scripts/check-js.mjs`.
5. Merge with **squash merge**; the squash subject is the PR title in
   Conventional Commits form.
6. Hotfixes also go through a PR (open and merge immediately) — there is
   no bypass lane.

## 2. Conventional Commits

Format: `<type>(<scope>): <one-line summary>`

- Types: `feat` / `fix` / `perf` / `refactor` / `test` / `docs` /
  `chore` / `design` (frontend visuals & copy) / `release`.
- Scopes: crate names (`dash`, `core`, `cli`, `tray`, `mcp`), `ci`,
  `iss`, `deps`, or a narrow area when one dominates.
- Body (optional): why the change was made; include measured numbers
  when relevant (e.g. "8760 buckets measured 3-4s").
- One logical change per commit. Batch review fixes are committed in
  batches, each reported by its commit hash.
- Forbidden: missing type prefix; unrelated changes in one commit;
  emoji anywhere.

## 3. Versioning & release

SemVer (`MAJOR.MINOR.PATCH`); the single source is
`[workspace.package] version` in the root `Cargo.toml`, inherited by all
five crates via `version.workspace = true`.

- PATCH: fixes only. MINOR: new user-visible capability. MAJOR:
  breaking change (data format incompatibility, behavior contract).

Release checklist (execute verbatim, in order):

1. Bump `[workspace.package] version`.
2. Add `## [x.y.z] - YYYY-MM-DD` at the top of `CHANGELOG.md`
   (Added/Changed/Fixed/Removed; user-perceivable changes only).
3. `cargo test --workspace` and `cargo clippy` green; commit as
   `release x.y.z: <summary>`.
4. Tag `vx.y.z` on that commit and push the tag.
5. Wait for the `release.yml` workflow to succeed.
6. Verify: all 10 release assets present — the 4 bare exes
   (`kynoptic.exe`, `kynoptic-watchdog.exe`, `kynoptic-tray.exe`,
   `kynoptic-ctl.exe`), `kynoptic-aggrepair.exe`, `SKILL.md`, the
   portable zip, `Kynoptic-Setup-*.exe`, `SHA256SUMS.txt`, and the
   target-triple zip (authoritative list: the `files:` block in
   `release.yml`); the release-page download link
   matches the actual asset name
   (`releases/download/vx.y.z/Kynoptic-Setup-x.y.z.exe`);
   SHA256SUMS.txt downloadable.
7. Deploy the same commit's build locally and smoke-test
   (`GET /api/status` returns 200).
8. Sync the website: the landing page (`index.html`, served by GitHub
   Pages from the repo root) must show the released version in its
   badge and point the primary CTA at the release — done in the same
   release cycle via its own PR.

## 4. Ground rules (checked in every review)

- No network calls from the collector. Local-only bindings (127.0.0.1)
  for the dashboard and MCP.
- Raw event layer is append-only: no cleanup, truncation, or re-keying;
  retention cleanup requires explicit `--yes` and prunes aggregate
  tables in the same transaction as raw rows. Aggregates are derived
  caches and may be rebuilt freely.
- Do not weaken input privacy: keyboard/mouse stay per-minute counts by
  default; per-key detail remains an explicit opt-in.
- Tests never read the real clock or depend on the local timezone
  (inject instants/anchors).
- Bug fixes ship with a regression test or a measured reproduction.
- Warnings are denied at the workspace level; no `#[allow]` without a
  comment explaining why. No emoji in code, docs, or commit messages.

## 5. CI gates

`.github/workflows/ci.yml` runs on push to `main` and on all PRs:
fmt / clippy / workspace tests / dashboard-JS syntax gate. Releases are
built by `.github/workflows/release.yml` on `v*` tags. Branch
protection requires the CI check to pass before merging.

## 6. Reporting issues

Use the issue templates (`.github/ISSUE_TEMPLATE`). Security issues go
through [SECURITY.md](SECURITY.md), not public issues.

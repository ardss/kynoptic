# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-09-18

### Added

- Corrupt-database visibility: collector startup failures now persist a
  diagnosis (including `PRAGMA quick_check` result) to
  `data\collector-error.log` and auto-retry every 60 s instead of
  failing silently behind a healthy-looking tray icon.
- Watchdog collector-liveness detection: the heartbeat carries the last
  successful DB flush timestamp and a `stalled` flag — a hung collector
  behind a live tray is now killed and restarted like a dead one.
- Watchdog hardening: state-file mutation lock, sleep-window guard
  against wrongful failure counting across suspend/resume, spawn-failure
  backoff (missing exe / blocked taskkill are no longer silent infinite
  retry loops), and log-rotation fallback.
- Self-update ships and refreshes `SKILL.md` (verified against
  SHA256SUMS), relaunches a tray it killed to unlock files, filters
  pre-release tags, and never kills its own process image.
- Input statistics in event time: minute buckets land in the minute the
  keystrokes happened (robust across aggregator stalls and DST repeated
  hours), with a `final` flag distinguishing complete minutes from
  per-second snapshots so restarts merge instead of discarding.
- First-run experience: collecting hints on all overview cards,
  first-day provisional note, heatmap "no data yet" state.
- Settings page: daily-goal minutes and dashboard-port controls.

### Fixed

- Silent autostart loss: the installer task is default-checked and
  upgrades no longer remember the old unchecked state; the watchdog
  scheduled task is always (re)created.
- Installer packaged stale binaries from `dist\`; packaging now follows
  the freshly built exes (build-order hygiene).
- Unattended metric double-subtracted human/automation mixed minutes.
- Presence/active-minute metrics: raw mode now reports first/last
  activity; all-time active minutes include minute-mode data; DST
  fall-back no longer collapses the repeated hour.
- MCP: batch requests answered, client responses absorbed silently,
  UTF-8 vs terminal IO errors distinguished, `wait_for` concurrency
  capped with prompt EOF shutdown, non-RFC3339 time bounds rejected
  readably, `days` wrong-type hard error, negative idle clamped.
- Insufficient-capacity file watcher channels are bounded with visible
  drop counters; clipboard polling no longer fabricates change events.
- Dashboard: transient disconnection indicator now clears, report/apps
  stale-response races guarded, Input tab tolerant of missing fields,
  settings edits survive re-entering the tab.

## [Unreleased]

### Added

- Automatic update discovery: the tray checks GitHub once daily
  (report-only); a new stable version surfaces as a tray menu item
  (one-click install for portable copies; installed copies open the
  download page) and a dashboard banner.
- `update --check`: report-only update check (used by the tray).
- Tray: single left-click opens the dashboard; About menu item;
  bilingual tooltip; dashboard-health now drives the Error/Running
  tray icons (previously dead state).
- Dashboard: three-state status line (collecting / stale / no events),
  overview vs-yesterday deltas + daily-goal percentage, monitor
  presets (Essential / Full / Minimal), confirmation dialog before
  enabling raw per-keystroke logging.
- Installer: release page now carries real release notes; SmartScreen
  guidance in README; bilingual uninstall dialogs; desktop icon
  checked by default.
- Foreground-dwell intervals spanning collector downtime (>2h) are no
  longer attributed to the frontmost app.

### Fixed

- Update-check failure no longer clears a previously-found update
  notice; manual update output is logged to `data\update.log`.
- Heartbeat stalled threshold raised to 30 min (above the slowest
  periodic writer) so idle machines are not kill-looped.
- `update` writes the tray exit flag before force-killing, preventing
  watchdog respawn races mid-upgrade.

### Added

- 40 system monitors (registry-driven; 14 enabled by default, 26 opt-in),
  with live-probe verification harness (`kynoptic-ctl probe`).
- CLI: `now` / `query` / `stats` / `collect` / `dashboard` / `mcp`
  subcommands, `kynoptic` and `kynoptic-ctl` binaries.
- MCP server (stdio JSON-RPC 2.0) with six tools: `get_current_status`,
  `get_summary`, `get_timeline`, `get_top_apps`, `get_anomalies`,
  `wait_for`.
- Dashboard settings: `settings.json` store, local-only settings tab
  (per-monitor toggles, input granularity, autostart), `GET/POST
  /api/settings` with registry id validation; `collect` now reads
  `enabled_monitors` from settings on restart (`--all` overrides).
- Tray shell (`kynoptic-tray`): pure Win32 tray process hosting the
  collector and the local dashboard together.
- Update path: background chunked aggregate backfill for large legacy
  databases (1M-row DB open: 631 s to ~8 ms).
- `presence` subcommand: daily human presence / automation / foreground
  dwell summary, sharing the single authoritative implementation in
  `kynoptic-core::queries::presence::classify_minutes` with the
  dashboard overview.
- `skill install` subcommand: syncs the bundled SKILL.md to AI client
  skill directories; the installer keeps it in sync automatically.
- Watchdog heartbeat with exponential backoff circuit breaker.
- `update` subcommand: self-update from GitHub releases with SHA256
  checksum verification, backup of the existing binary trio, and
  rollback on failure.
- `export`: writes `window_title` verbatim by default; `--redact`
  strips URL query strings from titles (opt-in).
- Input granularity defaults to minute (`input_counts_only` default
  true): keyboard/mouse stored as per-minute counts.
- Aggregate repair tool `kynoptic-aggrepair` (`--db X --from A --to B`,
  dry-run by default, `--apply` to execute).
- Native `wifi` / `power_plan` collection (native WiFi API +
  `PowerGetActiveScheme`): the default monitor set no longer spawns
  netsh/powercfg subprocesses — zero subprocesses by default.
- `vk` per-key frequency counts for the dashboard heatmap (on by
  default, opt-out in settings).
- Dashboard security headers: `X-Content-Type-Options`,
  `X-Frame-Options`, `Referrer-Policy`, and CSP.
- Benchmark harness (`perf-write` / `perf-startup` / `perf-hook` /
  `perf-query` / `perf-idle` / `perf3-*`); methodology and numbers in
  [BENCHMARKS.md](BENCHMARKS.md).
- Dashboard port fallback: when the default port 8422 is occupied the
  dashboard tries the next free ports (up to 8422+10) and writes the
  actual port to `data\dashboard-port.txt`; an explicit `--port 0`
  (random free port) keeps its semantics and skips the fallback.
- Unattended-card warmup gate: the "machine unattended" card shows an
  empty-state note (`has_full_day=false`) instead of a misleading number
  until a full day of data is collected.
- CLI `--db <PATH>` is consumed from any argument position (paired
  extraction, last occurrence wins) instead of only the fixed positions.
- `report --date` accepts `today` / `yesterday` aliases (empty = today,
  local timezone).
- `db recompute-agg` subcommand: rebuild derived aggregate tables from
  the raw layer.
- Write-failure counter with escalating alarm on persistent store write
  failures.
- Junction/symlink guards for `skill install` target directories
  (reparse-point check before writing) and atomic tmp+rename writes.
- Strict argument validation for `skill` / `analyze` subcommands;
  `analyze --days N`.

### Fixed

- `kynoptic mcp --db PATH` now reaches the MCP server: the resolved path
  is exported as `KYNOPTIC_DB` before startup (previously the server
  silently connected to the default database). Both `--db` and
  `KYNOPTIC_DB` work.
- MCP correctness pass: `get_summary` gains an explicit `compared_to`
  baseline date (default date-1; empty comparison instead of a misleading
  growth rate when the baseline day has no data); `get_timeline` parses
  bare dates by the local day boundary and returns `total_segments` plus
  a `truncation` strategy (`oldest-dropped`) when segments exceed the
  limit; `get_top_apps` added for in-window per-app dwell ranking;
  stricter parameter validation across tools.
- Aggregate (agg_minute/daily_agg) writes are transactional; under-filled
  aggregate days self-heal on subsequent reads instead of serving stale
  buckets.
- Ghost tray/watchdog processes from a previous session are detected and
  closed automatically at startup.
- Foreground self-exclusion: the collector's own process (including the
  installed `KYNOPTIC.EXE` path) no longer counts as the foreground app.
- `file_activity` monitor rewritten on `ReadDirectoryChangesW` (native
  change notifications instead of polling).
- `ime` monitor switched to native registry polling (no PowerShell
  subprocess).
- Foreground window/switch events now record the process name in
  `app_name` (was hardcoded empty).
- Writer thread idle busy-spin removed (idle CPU from ~100% of one core
  to ~0.4%).
- Network monitor: typed `GetIfTable2` iteration replaces a
  hardcoded-offset FFI read that returned dead counters on recent
  Windows 11 builds; `netstat -e` demoted to fallback.
- Hook lifecycle: per-collector shutdown flags replace a process-global
  static that resurrected prior collectors' threads as zombies;
  hook-path start-before-wait ordering fixed in the probe harness.
- Windows locale robustness: forced UTF-8 stdout for PowerShell
  helpers; GBK/OEM output handled via lossy UTF-8.
- Bluetooth enumeration on Windows 11 (class-GUID empty);
  `audio_output` empty friendly-name fallback; explicit no-battery log.
- Read-only open fallback for non-WAL databases; invalid
  `ALTER TABLE ... IF EXISTS` in migration 0002.

### Security / Privacy

- Keyboard and mouse input is stored as per-minute counts only by
  default (`InputGranularity::Minute`); per-key detail requires an
  explicit opt-in via `input_counts_only` in settings.
- Retention default is 0 (never delete); migration 0002 renames legacy
  tables instead of dropping them — the raw event layer is append-only.

## [0.1.0] - 2026-09

### Added

- Initial workspace: `kynoptic-core` (14 default monitors, numbered SQL
  migrations), `kynoptic-cli`, `kynoptic-mcp`, later `kynoptic-tray`.
- Performance work: sargable day-range queries, covering indexes,
  aggregate read caches, 30 s flush interval, per-minute input
  aggregation (opt-in).

[Unreleased]: https://github.com/ardss/kynoptic/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ardss/kynoptic/releases/tag/v0.1.0

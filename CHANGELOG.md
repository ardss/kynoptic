# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- 40 system monitors (registry-driven; 14 enabled by default, 26 opt-in),
  with live-probe verification harness (`kynoptic-ctl probe`).
- CLI: `now` / `query` / `stats` / `collect` / `dashboard` / `mcp`
  subcommands, `kynoptic` and `kynoptic-ctl` binaries.
- MCP server (stdio JSON-RPC 2.0) with five tools: `get_current_status`,
  `get_summary`, `get_timeline`, `get_anomalies`, `wait_for`.
- Dashboard settings: `settings.json` store, local-only settings tab
  (per-monitor toggles, input granularity, autostart), `GET/POST
  /api/settings` with registry id validation; `collect` now reads
  `enabled_monitors` from settings on restart (`--all` overrides).
- Tray shell (`kynoptic-tray`): pure Win32 tray process hosting the
  collector and the local dashboard together.
- Update path: background chunked aggregate backfill for large legacy
  databases (1M-row DB open: 631 s to ~8 ms).
- Benchmark harness (`perf-write` / `perf-startup` / `perf-hook` /
  `perf-query` / `perf-idle` / `perf3-*`); methodology and numbers in
  [BENCHMARKS.md](BENCHMARKS.md).

### Fixed

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

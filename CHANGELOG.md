# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Dashboard: `/api/overview` no longer waits on the `nvidia-smi`
  subprocess — GPU utilization now refreshes on a background thread
  (stale-while-revalidate, 30s TTL, single-flight), so request threads
  return the last known value immediately instead of blocking up to 3s+.
- Dashboard: read endpoints (`/api/trends`, `/api/apps`, `/api/daily_top`,
  `/api/heatmap`) map database-level failures to `400` with the root cause
  instead of degrading to `200` with silently empty payloads (which was
  indistinguishable from a legitimate empty result).
- Dashboard: the report page's `data_since` now uses the local calendar
  date (`datetime(timestamp,'localtime')`), consistent with the
  "a day = local day" convention everywhere else; in UTC+8 the calendar no
  longer opens one extra blank day.
- Dashboard settings: hand-edited `settings.json` with more than 100
  categories or a pattern longer than 200 characters is clamped on load
  (with a warning) instead of amplifying match cost; category rules
  precompile their lowercase tokens once at load so per-segment matching no
  longer re-lowercases the pattern for every rule.
  **Migration note:** category-rule matching is now genuinely
  case-insensitive. Previously a pattern token containing uppercase
  letters (e.g. "GitHub", "VSCode") never matched — the haystack was
  lowercased but the token was not — so such rules were silently dead.
  After upgrading, existing rules with uppercase tokens may start
  matching, and report/category results can differ from previous runs.
  Lowercase your patterns if you need the old (dead-token) behavior.
- Dashboard page: light-color-scheme palette and `forced-colors` support;
  the heatmap SVG colors are read from CSS variables at render time instead
  of hardcoded dark hex values, so canvas/SVG pixels follow the system
  theme and keep contrast in Windows high-contrast mode.

### Changed

- Dashboard: the HTTP service keeps a small pool (3) of read-only
  connections instead of one shared mutex-guarded connection, so a slow
  endpoint no longer serializes every other card request behind it.
- Dashboard: `/api/input` aggregates keys/clicks/button/scroll/move
  scalars and per-key frequency in SQL (`SUM`/`json_each`) instead of
  pulling every `input_agg` row into memory and serde-parsing each one;
  results are unchanged, cost no longer grows with the window's row count.
- Dashboard: `/api/timeline` results are cached for 60s in the long-running
  server (keyed by hours/bridge/db path) to avoid re-running the
  expensive multi-day aggregation on every panel poll; pure
  `api_timeline_at` used by tests is unaffected.
- Single-instance: while upgrading from the session-scoped
  `Local\KynopticTrayMutex` to the per-user `Global\KynopticTrayMutex\<SID>`
  name, new builds also acquire and probe the legacy `Local\` name
  (bridge mutex), so an old tray/collect still running during the upgrade
  window is detected by new binaries and vice versa — the no-double-writer
  guarantee no longer lapses across version mixes.
- Dashboard: new `GET /api/diagnostics` enumerates the known log/archive
  files (collector-error/dashboard-error/watchdog/tray/update/
  dashboard-port) across the data and exe directories with existence,
  size, mtime and a sanitized 20-line tail (no paths exposed); the
  settings page consumes it in a collapsible "diagnostics" block.

### Fixed

- Dashboard: report page "today" goal card now recomputes the local date
  per render, so a page left open across midnight no longer applies the
  server's today presence to yesterday's report.
- Dashboard: `/api/timeline` DST caveat (`local_offset_note`) is now
  rendered as a small note when the hour-window crosses a local offset
  change (or the server offset differs from the browser's).
- Dashboard: keyboard heatmap uses five distinct color steps (the bottom
  two quantiles previously shared one color); zero-activity days no longer
  draw a 2px phantom bar in the trend chart; `fmtBytes` supports TB;
  per-app colors are derived from a stable name hash so the apps grid and
  daily-top charts agree; monitors KPI falls back to `0 / 0` on missing
  fields; unparsable `last_event_ts` no longer claims "collector down".
- Security: POST /api/settings additionally requires a per-session random
  token injected into the served page (the previous three CSRF checks were
  all client-controlled headers); settings-audit.log refuses to follow
  symlink/reparse-point targets on open.

### Changed

- Docs: README/README.zh-CN clarify the 8422→8432→18422/28422 port
  fallback applies to the tray entry only (the CLI dashboard exits on a
  taken port); APP.md updated to the v0.2.x state (six MCP tools incl.
  `get_top_apps`, `--metric`/`--join` still not implemented).

## [0.2.2] - 2026-09-19

### Fixed

- Timeline: bridged presence minutes were shifted by the timezone offset
  (most hourly buckets showed 0 presence despite activity), and the
  current in-progress hour was missing from the response.
- Dashboard UX: terminology unified (presence/automation/unattended/
  foreground time) in plain language; mixed hours no longer rendered as
  unattended; stuck "loading..." placeholders replaced with proper empty
  states; KPI values no longer truncated; date buttons follow the
  selected day; design tokens unified (one orange/green/gray per
  meaning, presence-blue exclusive); anomalies tab alert badge;
  accessibility labels on views and charts.

## [0.2.1] - 2026-09-19

### Fixed

Eleven review waves (16-26) of parallel deep audits: about 120 fixes with
zero data-loss findings left open. Highlights:

- Session identity: events.session_id is now stamped (was 100% NULL).
- Watchdog safety: heartbeat pid extraction hardened and the pid's image
  name verified before any kill; the image-name global-kill fallback was
  removed (it could kill unrelated kynoptic-tray processes); system sleep
  no longer looks like a stall.
- Timeline API: /api/timeline hours clamped to 744 (31 days); year-scale
  views belong to the heatmap (daily_agg, milliseconds).
- Retention cleanup prunes pre-aggregation tables in the same
  transaction as raw events so heatmap/trends can never show deleted
  months.
- MCP: read-only connections no longer run WAL pragma (all tools failed
  on non-WAL databases); SQL errors are no longer reported as empty
  results.
- Release: fixed the double v-prefix download link that 404'd on every
  release; workspace-level version; aggrepair/ctl now ship in the
  installer.
- Settings: POST /api/settings rejects non-object bodies; UTF-8 BOM in
  settings.json no longer silently resets all settings.

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

## [0.1.0] - 2026-09

### Added

- Initial workspace: `kynoptic-core` (14 default monitors, numbered SQL
  migrations), `kynoptic-cli`, `kynoptic-mcp`, later `kynoptic-tray`.
- Performance work: sargable day-range queries, covering indexes,
  aggregate read caches, 30 s flush interval, per-minute input
  aggregation (opt-in).

[Unreleased]: https://github.com/ardss/kynoptic/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ardss/kynoptic/releases/tag/v0.1.0

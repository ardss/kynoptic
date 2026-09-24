# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Tray: the watchdog scheduled task now self-checks and self-heals — on
  tray startup the scheduled task is verified and re-registered if it is
  missing or broken, so stall alerts keep working without manual repair.
- MCP: `get_summary` responses now include a `baseline_semantic` field
  identifying the baseline window's semantics, so AI clients can tell
  what the baseline numbers actually mean (API contract addition).
- Dashboard: `/api/anomalies` now rejects `days=0` with 400, matching
  the MCP `get_anomalies` contract (`days` must be >= 1) — previously it
  was silently clamped to a 1-day window.
- CLI (probe): `kynoptic probe` accepts a new
  `--first-collect-timeout <SECS>` option to override the hard cap on
  waiting for a monitor's first collection (0, the default, derives it
  from `--secs`; any value is still capped at 420 s).

### Changed

- CLI (probe): the default first-collection timeout is no longer a fixed
  420 s — when not set explicitly it is now `min(--secs * 10, 420)`, so
  `--secs 15` skips a slow monitor after ~150 s and still produces a
  result instead of idling up to 7 minutes. Pass
  `--first-collect-timeout` to keep the old ceiling for a given run.
- Core (privacy): window-title redaction (opt-in) no longer matches only
  `http://`/`https://` URLs — it truncates at any RFC 3986-shaped
  `scheme://` prefix (e.g. `ws://`, `wss://`, `vscode://`, `file://`),
  so titles containing those schemes are now redacted too when the
  switch is on. The switch default remains off.
- CLI (probe): the probe subcommand no longer pre-installs the global
  env_logger, so registering a CaptureLogger (e.g. the polling monitor
  watchdog) can no longer fail and get misreported as a FAIL after the
  420 s wait.

- Core (privacy): the location monitor no longer performs any outbound
  network calls — its former geolocation path was removed and the
  monitor is local-only now; this tightens the "data never leaves the
  machine" guarantee for a default-on surface.
- CLI (read commands): read-only subcommands (`stats`, `query`,
  `report`, `export`, `analyze`, …) no longer create a database file
  when none exists, reject future dates instead of silently returning
  empty results, and cap WAL growth on long read sessions.

### Fixed

- Docs: the status lines in `README.md`, `README.zh-CN.md`, and `APP.md` now
  say v0.3.0 (they still said v0.2 / v0.2.x while `Cargo.toml`, the website,
  and the GitHub release already said 0.3.0).
- Dashboard (security, opt-in token): the optional access token is no longer
  accepted via the `?token=` URL query string (a query string ends up in
  browser history and bookmarks); pass it via the `X-Kynoptic-Access-Token`
  header or `Authorization: Bearer` instead. The dashboard page likewise no
  longer picks a token up from the URL — it keeps the sessionStorage flow and
  the on-page prompt. When no token is enabled, behavior is unchanged.

- Dashboard (high-contrast/forced-colors): the 24h timeline three-state
  bands, their legend markers, and both five-swatch heatmap legends are now
  drawn via CSS classes/`--hm-*` variables with distinct system-color
  mappings (plus a stripe pattern on the away band), instead of inline
  backgrounds that Windows high contrast collapses into one indistinguishable
  color; the forced-colors heatmap levels are five distinct system colors
  (previously 5 levels degraded to 3) and the legend swatches follow the same
  variables as the chart (the fifth swatch no longer shows a stale blue).
- Dashboard (accessibility): the daily heatmap and today's hourly input SVGs
  now have accessible names and the heatmap exposes its per-day values as a
  collapsible text summary (values previously lived only in hover tooltips,
  unreachable by keyboard, touch, and screen readers); the keyboard/mouse
  device SVGs got accessible names too.
- Dashboard (accessibility): the tab bar now implements the ARIA APG tabs
  keyboard contract (roving tabindex, Arrow/Home/End to move and activate,
  `aria-controls`/`aria-labelledby` linkage); heatmap range buttons use
  radiogroup semantics with `aria-checked` and arrow-key support; the
  today/yesterday/2-days-ago buttons expose `aria-pressed`; the status line,
  update banner, and anomaly badge are live regions that no longer re-announce
  on every 30s poll when the text is unchanged.
- Dashboard (error messages): API errors now carry a stable `code` field;
  English UIs map validation codes to English text instead of showing raw
  Chinese; 404/405 return bilingual text with a refresh hint; serde JSON
  parse errors report only line/column (details go to the log); SQLite errors
  are downgraded to a friendly bilingual message with the original logged;
  the 403 write-block and session-token responses are bilingual and
  distinguishable, and the settings save path offers a token re-entry prompt
  on 401.
- Dashboard (`/api/input`): requesting more than the 90-day cap is no longer
  silently truncated — the response includes `days_requested` and a note.
- Dashboard (reflow): settings/cards/host grids use `minmax(min(Npx,100%),1fr)`
  and monitor rows wrap, so a 320px viewport no longer gets a horizontal
  scrollbar (WCAG 1.4.10).
- Dashboard (settings): a failed hardware-info load now shows an error with a
  retry button instead of silently leaving placeholder dashes.

- Dashboard (insights): the human-side row filter now counts scroll-only
  input_agg minutes (keys/clicks/scroll_ticks each minus their injected
  counterpart, aligned with the presence definition), so scroll-only
  reading time shows up in longest-presence / late-night / golden-hours
  cards; the empty state now reports the real gate (`gate.events` /
  `gate.required`) instead of promising "after a day of collection".
- Dashboard (report): the goal card for a historical date now looks up the
  selected day in the trends `daily` array instead of always showing the
  last row (today), and the raw input-minute fallback is explicitly
  labelled instead of silently posing as presence minutes.
- Dashboard (`/api/hours`): the hourly values are now input counts
  (keystrokes + clicks; raw press/click fallback), not total event counts
  that were dominated by heartbeat/sampling noise; the response and the AI
  skill doc no longer describe it as "golden hours".
- Dashboard (wording): timeline legend "无人" renamed to "无输入" (away) to
  match what the band actually computes; heatmap top color no longer
  shares the presence-blue value and the daily heatmap is titled "incl.
  automation-injected input"; the trends note is phrased in plain language;
  the presence-grace hint mentions scroll; nav label is "今日概览" and the
  presence card says first/last "在场" (presence).
- Dashboard (overview): the "vs yesterday" delta is suppressed in the first
  3 hours after local midnight (yesterday is a full day; early-morning
  comparisons were systematically misleading).
- Dashboard (API): `/api/summary` accepts `today` like report/hours/
  apps_grid; `/api/input` accepts `date=` for the hourly series (previously
  silently ignored; an empty or invalid `date` now returns 400 instead of
  falling back to today) and returns `hourly_date`.
- Core (anomalies): marathon-session detection now reads the same
  human-presence minute source as the overview "presence" number
  (`human_minutes_by_date`: excludes automation-injected input, includes
  scroll ticks) instead of raw keyboard/mouse input minutes; sessions with
  only injected or scroll-only activity are no longer counted as marathons.
- Dashboard (report) / MCP: foreground dwell segments are now capped at
  2 hours per switch (gaps longer than the stall cap — shutdown, sleep,
  leaving the desk — are no longer attributed to the app). This applies to
  the report page dwell breakdown and MCP `get_top_apps`; values for a
  day with one long uninterrupted session are lower than before and now
  agree with overview/CLI presence.
- Core (sessions): `idle_seconds` is now backfilled from the
  `idle_start`/`idle_end` event pairs inside a session on both close paths
  (startup sweep and ghost-session cleanup); it was always written as 0
  before.
- Core (daily_agg): recompute no longer writes an all-zero row for days
  without any input and clears legacy all-zero rows (the trend chart used
  to show phantom zero points for those days). To keep consumers aligned,
  `/api/trends` now zero-fills missing calendar days so `daily` always
  covers the full 28-day window.
- CLI (watchdog): stall alerts are now recorded in the events table as
  `system`/`notification` rows and surfaced as a system notification, so
  watchdog alerts appear in the timeline/event views like other
  notifications.
- Dashboard (API): the `/api/hours` note and the AI skill doc now state
  that the hourly input counts include automation-injected input (the
  values themselves are unchanged; only the labeling, matching the daily
  heatmap).
- Core (privacy, salted digests): the clipboard/notification summary salt
  was persisted to the database metadata (`clipboard_salt`) but never used —
  `salt()` and `set_salt()` each declared their own `OnceLock`, so digests
  were computed with a fresh per-process random salt. The same clipboard
  text produced a different digest after every restart, breaking
  digest-based matching across sessions, and the stored salt was dead data.
  The two locks are now one module-level `OnceLock`: the salt is loaded
  from (or first created in) the database metadata at collection start and
  is the one actually used, so a given plaintext yields a stable digest
  within a library and a different digest across libraries, as documented.
- Core (privacy, title redaction): the collection-side title-redaction
  switch (`CollectorSettings::redact_titles`, `monitors::title_privacy`)
  is documented as reserved but **not yet wired** — no dashboard setting,
  tray, or CLI path can turn it on, so titles are still stored verbatim.
  The previously misleading "already wired" code comments now say so
  explicitly; behavior is unchanged (default off).
- Core (storage): a corrupted database is now surfaced to the caller with
  a clear error plus a degraded flag instead of being swallowed, so the
  dashboard/tray can show an explicit degraded state instead of
  pretending data is fine.
- Installer: the failure path is now idempotent — a failed install cleans
  up its own partial changes so re-running the installer does not stack
  duplicate state, and the installer writes the install directory to the
  user `PATH` so the CLI is reachable from a fresh terminal.
- Self-update: the updater now avoids file-in-use conflicts (backs off /
  renames targets that are locked) and swaps only the files that actually
  differ in a release, instead of touching every asset.
- Docs: the AI skill doc (`SKILL.md`) now states the data-retention cap
  in the same terms the code enforces, so AI clients no longer promise a
  different retention window than the product keeps.
- Dashboard (API): `/api/insights` no longer returns the internal
  `dbg` debug field (raw intermediate counts); the endpoint contract did
  not declare it and no frontend consumed it.
- Dashboard (security): the per-session CSRF token is now compared with
  the same constant-time comparison as the access token (previously a
  plain string compare), closing the timing-contrast inconsistency.
- Dashboard (security): the CSP nonce now mixes the high-resolution clock
  into both halves of its 128-bit value (previously only one), removing
  the theoretical correlation between the two `RandomState` outputs.
- Docs: `llms.txt` corrected — status line now says v0.3.0, the privacy
  section acknowledges the tray's once-a-day version check (the `update`
  subcommand remains user-invoked only), and the CLI subcommand list now
  matches the actual 18-command dispatch table; `CONTRIBUTING.md`'s
  release checklist step 6 now counts 10 release assets (matching the
  `release.yml` upload list); `README.md`'s quick-start `collect` comment
  now says foreground; `APP.md`'s CLI section version labels unified.

## [0.3.0] - 2026-09-23

### Fixed

- Collector: the writer connection now applies the same pragmas on open
  (WAL + busy_timeout) as the read paths. The background aggregation
  backfill thread holds its own write connection (`BEGIN IMMEDIATE`), so a
  freshly restarted writer with no busy timeout could immediately hit
  `SQLITE_BUSY` after a hot reload and silently drop the batch it was
  about to write; that race window is closed.
- Dashboard (access control): all responses now also send
  `Referrer-Policy: no-referrer`, so a token accidentally embedded in a
  bookmarked `/api/*?token=…` URL is not leaked to other origins if the
  JSON error page is ever loaded in a browser tab; the 401 body now hints
  at using the dashboard page (which strips the token from the URL) instead
  of linking directly to `/api/*`.
- Dashboard: invalid calendar dates such as `2026-13-45` now report
  "date format error" instead of the misleading "date is in the future"
  (the old string comparison ran before any real date parsing).
- Dashboard: an explicitly empty `?date=` on read endpoints now returns
  the same 400 "date format error" as other invalid values, instead of
  silently falling back to today (only a fully omitted parameter falls
  back).
- Dashboard: the "unknown monitor id" error no longer echoes the raw
  attacker-controlled string (response amplification / log-injection
  surface); it is sanitized the same way as date inputs.
- Dashboard: settings.json writes clean up their temp file when the final
  rename fails (file locked by antivirus/backup, ACL denial, disk full) —
  previously each failed save leaked one `json.tmp.<pid>.<nanos>` file.
  The tray now logs a warning when the startup autostart sync-back fails
  instead of discarding the error.
- Dashboard: hourly buckets for `/api/input`, `/api/apps_grid` and
  `/api/daily_top` are localized with SQLite `localtime` (timezone
  resolved per event timestamp) instead of a fixed offset captured at
  query time, matching every other view; historical hour distributions no
  longer shift by one hour in timezones with past DST transitions.
- Dashboard (hardening): the inline dashboard script is now served with a
  per-request CSP nonce and all inline `onclick`/`onchange` attributes
  were replaced by delegated listeners; `script-src` no longer allows
  `unsafe-inline`, restoring a second line of defense behind `esc()` for
  the 30+ `innerHTML` render paths.

- Dashboard (web): `loadApps` now guards the second `await`
  (`/api/daily_top`) with the same sequence check as the first, so a
  stale response can no longer overwrite the "daily Top 3" table with
  data from a previously selected date.
- Dashboard (web): non-numeric input in the settings numeric fields
  (daily goal / bridge minutes / port) no longer silently saves `0`
  via `parseInt(...) || 0`; the previous saved value (or the default)
  is kept and a note is shown. A changed port now also surfaces the
  "takes effect after tray restart" hint (previously dead i18n copy).
- Dashboard (web): with per-keystroke (raw) granularity the input page
  no longer renders the misleading "N unattributed clicks from an old
  version" banner — raw mode never produces per-button rows, so all
  clicks were falsely reported as unattributed.
- Dashboard (web): a last-event timestamp in the future (dirty data /
  clock skew) no longer reports "collecting" forever; it is shown as a
  timestamp anomaly instead.
- Dashboard (web): the 24h timeline no longer draws a full 60-minute
  "away" span for the in-progress current hour; away is capped at the
  minutes elapsed since the hour start (computed from the response's
  `generated_at`, not the client clock).
- Dashboard (web): `jget` now surfaces the backend `{"error": ...}`
  reason on non-200 responses (e.g. future-date validation) instead of
  only "URL → HTTP 400".
- Dashboard (web): the Top Apps list uses the same display names as the
  timeline / app grid / daily Top 3 (`.exe` suffix stripped; full name
  kept in the tooltip).

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
  `Local\KynopticTrayMutex` to the per-user `Global\KynopticTrayMutex-<SID>`
  name (single-level name — kernel named objects do not support path-like
  nesting; the nested `Global\A\B` form fails with ERROR_PATH_NOT_FOUND,
  the root cause of the 2026-09-23 deployment incident), new builds also
  acquire and probe the legacy `Local\` name (bridge mutex), so an old
  tray/collect still running during the upgrade window is detected by new
  binaries and vice versa — the no-double-writer guarantee no longer lapses
  across version mixes.
- Dashboard: new `GET /api/diagnostics` enumerates the known log/archive
  files (collector-error/dashboard-error/watchdog/tray/update/
  dashboard-port) across the data and exe directories with existence,
  size, mtime and a sanitized 20-line tail (no paths exposed); the
  settings page consumes it in a collapsible "diagnostics" block.
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

[Unreleased]: https://github.com/ardss/kynoptic/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/ardss/kynoptic/compare/v0.2.2...v0.3.0
[0.1.0]: https://github.com/ardss/kynoptic/releases/tag/v0.1.0

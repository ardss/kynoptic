# Kynoptic App (v0.3.1)

> **TL;DR (English)** — Kynoptic is a local-first Windows activity awareness
> layer: a Rust workspace (`core` collectors + SQLite storage
> with numbered-SQL migrations, `cli`, and an `mcp` server; the dashboard and
> tray shell live in their own crates). 40 monitors cover the system, 14 are
> enabled by default (12 polling
> monitors + 2 low-level input hooks; pure Win32, zero subprocesses —
> wifi/power_plan use native APIs). Input is recorded as per-minute counts only —
> never key contents — with injected (LLKHF_INJECTED) events separated from
> human presence, and the raw event log is append-only: cleanup is opt-in and
> aggregates are rebuildable derived caches. The dashboard exposes a
> three-metric model (human presence / automation / foreground dwell) over a
> local-only HTTP API, and an MCP server lets a local AI assistant ask the same
> questions without any data leaving the machine.
> **Full document in Chinese below.**

Kynoptic 的本机感知层（采集器 + CLI + MCP 工具面）Rust workspace，位于仓库 `crates/` 下。网页（`index.html` 等）与本目录无关。

## Workspace 布局

| Crate | 说明 |
|-------|------|
| `crates/core` (`kynoptic-core`) | 采集核心：40 个监控器（14 个默认启用：纯 windows-sys、零 PowerShell 子进程；26 个恢复自上游、默认关闭）、事件通道/写入线程、SQLite 存储层（编号 SQL 迁移）、查询/分析 |
| `crates/cli` (`kynoptic-cli`) | 命令行工具：统计/导出/报告/分析/数据库维护/实时状态/事件查询/MCP 启动（bin 名 `kynoptic` 与别名 `kynoptic-ctl`） |
| `crates/dash` (`kynoptic-dash`) | dashboard 服务与页面（CLI 与托盘共用） |
| `crates/mcp` (`kynoptic-mcp`) | MCP server（stdio JSON-RPC 2.0）：`get_current_status` / `get_summary` / `get_timeline` / `get_top_apps` / `get_anomalies` / `wait_for` |
| `crates/tray` (`kynoptic-tray`) | 托盘壳（纯 Win32，宿主采集器与本地面板） |

监控器全集 40 个（注册表：`crates/core/src/registry.rs`；配置模板：`crates/core/config/monitors.json`）。**默认启用 14 个**（纯 windows-sys、零子进程，与 v0.1 相同）：`system` `window` `keyboard_hook` `mouse_hook` `idle` `session` `battery` `network` `device` `process` `audio` `brightness` `wifi` `power_plan`。

恢复自上游、**默认关闭**的 26 个：

| 监控器 | 敏感度 | 依赖 | 采集内容 |
|--------|--------|------|---------|
| `browser` | 中 | 原生 | 浏览器标签页标题 |
| `clipboard` | 中 | 原生 | 剪贴板文本哈希 |
| `file_activity` | 中 | 原生 | 文件系统活动 |
| `media` | 中 | 原生 | 媒体设备使用 |
| `screen_capture` | 中 | 原生 | 截屏/录屏检测 |
| `usb_device` | 中 | 原生 | USB 设备插拔 |
| `bluetooth` | 中 | 原生 | 蓝牙设备 |
| `display` | 中 | PS* | 显示器配置变化 |
| `external_display` | 中 | PS* | 外接显示器插拔 |
| `audio_input` | 中 | PS* | 音频输入设备插拔 |
| `audio_output` | 中 | PS* | 音频输出设备插拔 |
| `ime` | 中 | PS* | 输入法切换 |
| `location` | 高 | PS* | 位置快照（IP 粗定位） |
| `notification` | 高 | PS* | Windows 通知中心 |
| `calendar` | 高 | PS* | 日历事件（Outlook） |
| `dns` | 高 | PS* | DNS 查询记录 |
| `security` | 高 | PS* | 安全事件日志 |
| `firewall` | 高 | PS* | 防火墙事件日志 |
| `uac` | 高 | PS* | UAC 提权事件 |
| `windows_update` | 高 | PS* | 系统更新状态 |
| `driver` | 高 | PS* | 驱动变化 |
| `vpn` | 高 | PS* | VPN 状态 |
| `print` | 高 | PS* | 打印任务 |
| `stylus` | 高 | PS* | 触控笔设备 |
| `thermal` | 高 | PS* | 温度传感器 |
| `gpu` | 高 | PS* | GPU 状态快照 |

\* PS = 每次采集 spawn 一个 PowerShell 子进程，默认一律关闭，代码标注"PS 子进程，待原生重写"。另外 `device`（默认启用）的磁盘 I/O 速率子功能同为 PS 依赖，默认关闭、仅显式开启后采集（容量/内存字段不受影响）。

## 构建

```bash
cargo build --release   # 产物在 target/release/
cargo test              # 全部单元/集成测试
cargo clippy --workspace --all-targets   # 零警告（workspace 级 deny warnings）
```

Windows 专用（依赖 windows-sys）；需要 MSVC 工具链。

## 数据库

SQLite，迁移为 `crates/core/src/db/migrations/` 下的编号 SQL 文件（事务执行，幂等）。
概览：`0001_init`（采集基础表）、`0002_bucket_model`（开放 bucket 模型：schema_meta / buckets / event_types / agg_minute / agg_daily / current_state）、`0003` 起（perf/聚合读缓存覆盖索引等性能与派生缓存迁移）至 `0010`，逐个文件见 `crates/core/src/db/migrations/`。

### event_type/action → event_data JSON 键（采集器写入的主要负载）

事件行主列为 `timestamp / event_type / event_action / app_name / window_title`，结构化负载在 `event_data` JSON 中（键随采集器版本演进，以代码为准）。高频 action 一览：

| event_type / action | event_data 主要键 | 来源 |
|---------------------|-------------------|------|
| `keyboard` / `input_agg` | `keys`, `samples`, `keys_samples`, `vk`（逐键频次 map，需显式开启）, `injected_keys`（有注入时） | `core/src/input_agg.rs` |
| `mouse` / `input_agg` | `clicks`, `scroll_ticks`, `moves`, `move_distance_px`, `samples`, `clicks_left/right/middle/side1/side2`, `injected_clicks`（有注入时） | `core/src/input_agg.rs` |
| `window` / `switch` | `hwnd`, `title`, `pid`, `proc` | `monitors/window.rs` |
| `system` / `heartbeat` | `memory`, `cpu_percent` | `monitors/system.rs` |
| `system` / `battery_status` | `percent`, `status_code`, `status_text`, `charging`, `prev_charge`（变化时） | `monitors/battery.rs` |
| `system` / `idle_start` / `idle_end` | `idle_seconds`, `threshold` | `monitors/idle.rs` |
| `system` / `audio_state` | `volume`, `muted` | `monitors/audio.rs` |
| `system` / `volume_change` | `old_volume`, `new_volume`, `muted` | `monitors/audio.rs` |
| `system` / `brightness_change` | `brightness`, `old_brightness`, `new_brightness` | `monitors/brightness.rs` |
| `session` / `lock` / `unlock` | `locked` | `monitors/session.rs` |
| `session` / `display_change` | `prev_count`, `current_count` | `monitors/session.rs` |
| `device` / `device_snapshot` | 硬件快照（含 `memory` 总量/可用、`disks` 容量等序列化字段） | `monitors/device.rs` |
| `network` / `conn_snapshot` | 网络连接快照（序列化结构） | `monitors/network.rs` |
| `process` / `process_snapshot` | 进程列表快照（序列化结构） | `monitors/process.rs` |

数据库路径解析：环境变量 `KYNOPTIC_DB` > exe 同级 `data/kynoptic.db` > cwd 候选。

## CLI（实际可用面，截至 v0.3.0）

两个 bin 同源：`kynoptic`（网站 MCP 配置示例的 command）与 `kynoptic-ctl`（别名）。

| 子命令 | 状态 | 说明 |
|--------|------|------|
| `stats` / `export` / `report` / `db` / `analyze` / `ghost` / `autostart` / `migrate` | ✅ | 同上一版（v0.1 起可用） |
| `now [--json]` | ✅ | 当前机器状态紧凑视图（cpu/mem/前台应用/idle/APM/电量），与 MCP `get_current_status` 同数据面 |
| `query --from T --to T --bucket B --limit N --json` | ✅（营销口径的子集） | 时间范围事件查询。`--from/--to` 接受 `today`/`yesterday`/`YYYY-MM-DD`/RFC3339；`--bucket` 接受 bucket id（`activity/keys`、`activity/mouse`、`app/window`、`system/*`、`network/*`、`session/*`、`device/*`）或裸 event_type。网站的 `--metric gpu` / `--join window` 依赖 GPU/窗口聚合层，**至今未落地**（截至 v0.3.0 仍报"未知选项"；`current_state`/`agg_*` 表已建，采集器未写入） |
| `mcp` | ✅ | 启动 MCP server（stdio，阻塞到 stdin 关闭） |
| `collect` | ✅ | 前台运行采集器，Ctrl+C 优雅停止（`--db PATH` / `--all` 覆盖 settings） |
| `dashboard [--port N] [--db PATH]` | ✅ | 仅本机可访问的只读网页面板（127.0.0.1） |
| `presence [--days N]` | ✅ | 三指标日报（presence/automation/foreground + mixed），与 dashboard overview 同一权威实现 |
| `skill install` | ✅ | 把内置 SKILL.md 同步到 AI 客户端 skill 目录 |
| `probe [--monitor ID] [--secs N] [--all]` | ✅ | 逐监控器硬件实测探针 |
| `watchdog [--db PATH] [--once]` | ✅ | 看门狗心跳（供计划任务调用，确保托盘存活） |
| `update` | ✅ | 自更新（GitHub releases，SHA256 校验 + 三件套备份 + 失败回滚） |

### 用脚本写设置（POST /api/settings）

面板写接口带三道防线（回环 Origin + `X-Kynoptic: 1` + 页面注入的 `X-Kynoptic-Token`
会话令牌；启用了访问令牌时另需 `X-Kynoptic-Access-Token`），脚本调用四个头缺一
不可。完整 curl 配方与各头作用见 README「用脚本写设置 / Writing settings from
scripts」一节；会话令牌实现在 `crates/dash/src/lib.rs`（`__KYN_CSRF_TOKEN__`
注入首页），校验顺序：csrf → 访问令牌 → 413 → 会话令牌。

### Claude Desktop / 任意 MCP 客户端接入

```json
{
  "mcpServers": {
    "kynoptic": { "command": "kynoptic", "args": ["mcp"] }
  }
}
```

协议：换行分隔 JSON-RPC 2.0；实现 `initialize` / `tools/list` / `tools/call` /
`ping` 与通知吸收；六工具定义见《mcp-tool-spec-v1》。数据面偏差见 `CODE_NOTES.md` §5。

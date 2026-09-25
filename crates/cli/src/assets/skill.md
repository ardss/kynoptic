---
name: kynoptic
description: Use when the user or an agent needs to query, export, or analyze local activity data from Kynoptic (Windows local-first activity tracker) — presence/automation/foreground metrics, timeline, insights, anomalies, raw event export, or MCP wiring. Read-mostly; the only writes are dashboard settings and explicit user-approved cleanup.
---

# Kynoptic CLI（本机活动追踪）

通过 CLI / HTTP API / MCP 读取用户的本地活动追踪数据（Rust 单机，SQLite append-only）。
Kynoptic 的核心模型：**电脑活动 ≠ 人的活动**。三个权威指标：
- **人在场 presence** = 过滤 AI agent 注入输入（LLKHF_INJECTED）后的真实输入分钟 + 无输入宽限桥接（默认 2 分钟，可配 0-15）
- **自动化活动 automation** = 被判定为注入的输入分钟数
- **前台应用时长 fg_dwell** = 窗口驻留推算，剔除超过 2 小时的停摆间隔（含人不在场时段）
- 派生卡：**机器值班 unattended** = 前台 − 人在场 − 自动化

## 定位二进制与数据（按序尝试）

1. `%LOCALAPPDATA%\Programs\Kynoptic\kynoptic.exe`（安装版默认位；开始菜单快捷方式指向同处）
2. `kynoptic` 在 PATH（0.3.x 起安装器会把安装目录写入用户 PATH；旧版本安装的用户需升级安装一次或自配 PATH）
3. 便携版=解压/拷贝目录下的 kynoptic.exe（与 kynoptic-tray.exe 同目录，data\ 就在其旁）

**两种形态的区别**：安装版=Setup.exe 装的，带计划任务看门狗+自启，卸载走 unins000；便携版=直接拷三个 exe（kynoptic.exe/kynoptic-tray.exe/kynoptic-watchdog.exe）+ data\ 目录，没有计划任务，要自启需手动。数据一律在 exe 同级 data\kynoptic.db（安装与便携相同），dashboard 实际端口看 data\dashboard-port.txt。

数据约定：db 与 settings.json 在**运行目录的 data\ 下**（如 `D:\Kynoptic\data\kynoptic.db`；便携版=exe 旁）。
CLI 解析顺序：`--db <PATH>` 全局参数 > `KYNOPTIC_DB` 环境变量 > exe 同级 data\（`--db` 可写在子命令后任意位置，成对消费）。
CLI 默认路径推导时会打印 `使用数据库: <path>` 到 stderr——**注意核对**，防静默读到错误库。
仪表盘：http://127.0.0.1:8422/（只读 HTTP；**端口三段回退仅托盘入口生效**——CLI `kynoptic dashboard` 端口被占会直接报错退出，需 `--port` 另指；实际端口写在 data\dashboard-port.txt；服务没起来 panel 000 时先找托盘进程）。

## 核心命令

```bash
kynoptic presence --days 7        # 三指标：每日 presence/automation/foreground + mixed（权威口径）
kynoptic now                      # 即时状态：APM/CPU/前台应用/idle
kynoptic stats                    # 输入统计（raw 口径）
kynoptic report --date today         # 单日报告（report 的 --date 也接受 yesterday / YYYY-MM-DD）
kynoptic analyze --days 7           # 近 7 天逐日专注/碎片/异常分析
kynoptic analyze --date YYYY-MM-DD  # 单日专注/碎片/异常分析（analyze 的 --date 仅接受 YYYY-MM-DD，不支持 today/yesterday；昨天可改用 --days 1）
kynoptic export --days 7 --out FILE [--format csv|json|jsonl] [--redact]
kynoptic db stats                 # 行数/库大小
kynoptic mcp [--db PATH]           # 启动 MCP 服务器（stdio）；--db 与 KYNOPTIC_DB 均可（--db 内部即经 KYNOPTIC_DB 传递）
kynoptic update --check            # 只查不装：stdout "UPDATE <ver>" 或 "UP TO DATE (<cur>)"；托盘每日自动检查并把新版本写入 data\update-available.txt（菜单/面板同步提示，绝不自动安装）
```

HTTP API（GET 全部只读）：
`/api/overview`（三指标+机器值班+硬件）、`/api/timeline?hours=24`（全小时补零三色桶；hours 有效范围 1–744（31 天），超出范围的取值会被收拢到该区间）、
`/api/insights`（6 张叙事卡）、`/api/report`、`/api/heatmap`、`/api/anomalies`（含 message_en）、
`/api/settings`、`/api/status`、`/api/summary`、`/api/input?date=`（逐时输入序列，date 指定统计哪一天，缺省今天；空值或非法日期返回 400）、
`/api/apps?days=N`（应用使用排行，按窗口切换事件数）、`/api/hours?date=`（每小时输入次数，含自动化注入输入；非黄金时段；黄金时段洞察取 /api/insights）、
`/api/apps_grid`、`/api/daily_top`、`/api/trends`（本周/上周对比，注意 active_minutes 是 raw 口径）、`/api/diagnostics`（运行诊断信息）。
写设置仅 `POST /api/settings`：需头 `X-Kynoptic: 1` + `Origin: http://127.0.0.1:<实际端口>`（Origin 必须与 dashboard-port.txt 里的实际端口一致）；**另有每会话 `X-Kynoptic-Token` 校验**——该令牌由服务端随机生成、仅注入面板页面，外部脚本拿不到，直连 POST 会被 403。外部脚本无法写设置，请引导用户在面板里改，或提示用户手动操作。

## 口径铁律（引用数字前必读）

- **人在场的唯一权威实现**在 `kynoptic-core::queries::presence::classify_minutes`，dashboard overview 与 CLI `presence` 共用。两者数字必须一致；不一致=bug，直接报告不要解释。
- `stats`/`heatmap`/`summary` 的 active/输入分钟是 **raw 口径**（含注入、不桥接），与 presence 语义不同，**不要混用或互相换算**。
- timeline 的 `human_min` 是桥接后值（另有 `human_min_unbridged`）。
- MCP 共六工具：`get_current_status` / `get_summary` / `get_timeline` / `get_top_apps` / `get_anomalies` / `wait_for`。
- MCP get_summary 的 keys/clicks 从原始 events 现算（dashboard /api/summary 读 agg_minute 缓存）——聚合缓存滞后/修复期间两边可能有小差异，权威口径以 dashboard 为准；active_minutes 已过滤 heartbeat/snapshot，与 dashboard 同源；`compared_to` 是对比基准日（默认 date-1，基准日无数据时该对比字段为空而非 0 增长）。
- MCP get_timeline 的裸日期 from/to 按**本地日界**解析，to 为日期时含当天全天；响应带 `total_segments` 与截断策略（超限丢弃最旧段，`truncation=oldest-dropped`）。

## 安全规则（优先级高于效率）

1. **原始事件永不删除**：retention=0 表示永不清理。`db cleanup` 默认只清 sessions；删 events 需要 `--yes` 且 ≥30 天——用户没有明确说"清理数据"时禁止执行。
2. **绝不直接写 SQLite**：一切读取可以直接开只读连接（`mode=ro`），但写入只经 CLI/API。聚合表（agg_minute/daily_agg）是派生缓存，重算用 `kynoptic-aggrepair --db X --from A --to B`（默认 dry-run，`--apply` 才执行）。
3. **导出含敏感明文**（窗口标题原文含 URL/邮件标题，默认不脱敏）：把导出文件内容发给任何外部服务前必须先问用户。
4. settings.json 损坏会自动留档 `.corrupt.bak`；文件丢失时托盘会告警并落一份默认文件——发现用户配置丢失先查这个文件和历史，不要默默重装。
5. 面板并发上限 64 连接，超限 503；面板 503 先看是否服务被杀（watchdog.log、kynoptic-heartbeat 新鲜度），不要重试轰炸。

## 意图路由

| 用户意图 | 做法 |
|---|---|
| "我今天工作多久了 / 人在电脑前多久" | `kynoptic presence`（或 curl /api/overview 的 presence_minutes） |
| "agent 替我干了多少活" | overview 的 automation_minutes + unattended_fg_minutes（机器值班卡） |
| "昨天 X 点我在干嘛" | curl `/api/timeline?hours=24` 或 `kynoptic report` |
| "这周和上周比" | `/api/trends`（注意 active_minutes 是 raw 口径） |
| "有没有异常" | `/api/anomalies`（APM 突增/深夜活动/马拉松会话） |
| "把我的数据导出来" | `export --days N --out`；确认目标位置含敏感明文 |
| "面板打不开" | tasklist 查 kynoptic-tray → curl 8422 → 看 heartbeat/watchdog.log/data\tray.log → 杀掉后 `kynoptic-tray --minimized` 重启（先杀 watchdog 再动 tray） |
| "让 AI 读取活动数据" | 配 MCP：命令 `kynoptic mcp`（stdio，`--db` 与 `KYNOPTIC_DB` 均可）；新机器记得设其一或把 db 放默认位 |

## 数据迁移（换机）

1. 新机装 Release 安装包（无需工具链；skill 会被安装器自动同步，老包手动跑 `kynoptic skill install`）
2. 旧机停进程后整体拷贝 `data\`（kynoptic.db + wal/shm + settings.json）到新机安装目录的 data\
3. 管理员跑一次 `wevtutil sl Microsoft-Windows-DNS-Client/Operational /e:true`（dns 监控器依赖，不随数据走）
4. 验收：面板四卡有数 + `/api/settings` monitors 40/40 + `kynoptic presence --days 3` 与面板一致

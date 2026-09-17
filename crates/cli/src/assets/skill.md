---
name: kynoptic
description: Use when the user or an agent needs to query, export, or analyze local activity data from Kynoptic (Windows local-first activity tracker) — presence/automation/foreground metrics, timeline, insights, anomalies, raw event export, or MCP wiring. Read-mostly; the only writes are dashboard settings and explicit user-approved cleanup.
---

# Kynoptic CLI（本机活动追踪）

通过 CLI / HTTP API / MCP 读取用户的本地活动追踪数据（Rust 单机，SQLite append-only）。
Kynoptic 的核心模型：**电脑活动 ≠ 人的活动**。三个权威指标：
- **人在场 presence** = 过滤 AI agent 注入输入（LLKHF_INJECTED）后的真实输入分钟 + 无输入宽限桥接（默认 2 分钟，可配 0-15）
- **自动化活动 automation** = 被判定为注入的输入分钟数
- **前台应用时长 fg_dwell** = 窗口驻留推算（不封顶，含人不在场时段）
- 派生卡：**机器值班 unattended** = 前台 − 人在场 − 自动化

## 定位二进制与数据（按序尝试）

1. `D:\Kynoptic\kynoptic.exe`（默认安装位）
2. `kynoptic` 在 PATH（installer 会加）
3. 源码仓库 `G:\kynoptic\target\release\kynoptic.exe`（G 盘可能未挂载）

数据约定：db 与 settings.json 在**安装目录的 data\ 下**（如 `D:\Kynoptic\data\kynoptic.db`）。
CLI 解析顺序：`KYNOPTIC_DB` 环境变量 > `--db <PATH>` 全局参数 > exe 同级 data\。
CLI 默认路径推导时会打印 `using db: <path>` 到 stderr——**注意核对**，防静默读到错误库。
仪表盘：http://127.0.0.1:8422/（只读 HTTP，浏览器可直接验收）。

## 核心命令

```bash
kynoptic presence --days 7        # 三指标：每日 presence/automation/foreground + mixed（权威口径）
kynoptic now                      # 即时状态：APM/CPU/前台应用/idle
kynoptic stats                    # 输入统计（raw 口径）
kynoptic report [date]            # 单日报告
kynoptic analyze --days 7         # 多日分析
kynoptic export --days 7 --out FILE [--format csv|jsonl] [--redact]
kynoptic db stats                 # 行数/库大小
kynoptic mcp                      # 启动 MCP 服务器（stdio）
```

HTTP API（GET 全部只读）：
`/api/overview`（三指标+机器值班+硬件）、`/api/timeline?hours=24`（全小时补零三色桶）、
`/api/insights`（6 张叙事卡）、`/api/report`、`/api/heatmap`、`/api/anomalies`（含 message_en）、
`/api/settings`、`/api/status`、`/api/summary`、`/api/input`。
写设置仅 `POST /api/settings`：需头 `X-Kynoptic: 1` + `Origin: http://127.0.0.1:8422`（CSRF 三重校验的一部分），body 只传要改的字段。

## 口径铁律（引用数字前必读）

- **人在场的唯一权威实现**在 `kynoptic-core::queries::presence::classify_minutes`，dashboard overview 与 CLI `presence` 共用。两者数字必须一致；不一致=bug，直接报告不要解释。
- `stats`/`heatmap`/`summary` 的 active/输入分钟是 **raw 口径**（含注入、不桥接），与 presence 语义不同，**不要混用或互相换算**。
- timeline 的 `human_min` 是桥接后值（另有 `human_min_unbridged`）。
- MCP get_summary 的 active_minutes 已过滤 heartbeat/snapshot，与 dashboard summary 同源。

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
| "面板打不开" | tasklist 查 kynoptic-tray → curl 8422 → 看 heartbeat/watchdog.log → 杀掉后 `kynoptic-tray --minimized` 重启（先杀 watchdog 再动 tray） |
| "让 AI 读取活动数据" | 配 MCP：命令 `kynoptic mcp`（stdio）；新机器记得 `KYNOPTIC_DB` 或把 db 放默认位 |

## 数据迁移（换机）

1. 新机装 Release 安装包（无需工具链）
2. 旧机停进程后整体拷贝 `data\`（kynoptic.db + wal/shm + settings.json）到新机同结构位置
3. 管理员跑一次 `wevtutil sl Microsoft-Windows-DNS-Client/Operational /e:true`（dns 监控器依赖，不随数据走）
4. 验收：面板四卡有数 + `/api/settings` monitors 40/40 + `kynoptic presence --days 3` 与面板一致

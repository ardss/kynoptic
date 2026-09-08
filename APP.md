# Kynoptic App (v0.1)

Kynoptic 的本机感知层（采集器 + CLI + MCP 工具面）Rust workspace，位于仓库 `crates/` 下。网页（`index.html` 等）与本目录无关。

## Workspace 布局

| Crate | 说明 |
|-------|------|
| `crates/core` (`kynoptic-core`) | 采集核心：14 个 v0.1 监控器（纯 windows-sys、零 PowerShell 子进程）、事件通道/写入线程、SQLite 存储层（编号 SQL 迁移）、查询/分析 |
| `crates/cli` (`kynoptic-cli`) | 命令行工具：统计/导出/报告/分析/数据库维护/实时状态/事件查询/MCP 启动（bin 名 `kynoptic` 与别名 `kynoptic-ctl`） |
| `crates/mcp` (`kynoptic-mcp`) | MCP server（stdio JSON-RPC 2.0）：`get_current_status` / `get_summary` / `get_timeline` / `get_anomalies` / `wait_for` |

v0.1 监控器（14 个）：`system` `window` `keyboard_hook` `mouse_hook` `idle` `session` `battery` `network` `device` `process` `audio` `brightness` `wifi` `power_plan`（见 `crates/core/src/monitors/`）。

## 构建

```bash
cargo build --release   # 产物在 target/release/
cargo test              # 全部单元/集成测试
cargo clippy --workspace --all-targets   # 零警告（workspace 级 deny warnings）
```

Windows 专用（依赖 windows-sys）；需要 MSVC 工具链。

## 数据库

SQLite，迁移为 `crates/core/src/db/migrations/` 下的编号 SQL 文件（事务执行，幂等）：
`0001_init.sql`（采集基础表）、`0002_bucket_model.sql`（开放 bucket 模型：schema_meta / buckets / event_types / agg_minute / agg_daily / current_state）。

数据库路径解析：环境变量 `KYNOPTIC_DB` > exe 同级 `data/kynoptic.db` > cwd 候选。

## CLI（v0.1 实际可用面）

两个 bin 同源：`kynoptic`（网站 MCP 配置示例的 command）与 `kynoptic-ctl`（别名）。

| 子命令 | 状态 | 说明 |
|--------|------|------|
| `stats` / `export` / `report` / `db` / `analyze` / `ghost` / `autostart` / `migrate` | ✅ v0.1 | 同上一版 |
| `now [--json]` | ✅ v0.1 | 当前机器状态紧凑视图（cpu/mem/前台应用/idle/APM/电量），与 MCP `get_current_status` 同数据面 |
| `query --from T --to T --bucket B --limit N --json` | ✅ v0.1（营销口径的子集） | 时间范围事件查询。`--from/--to` 接受 `today`/`yesterday`/`YYYY-MM-DD`/RFC3339；`--bucket` 接受 bucket id（`activity/keys`、`activity/mouse`、`app/window`、`system/*`、`network/*`、`session/*`、`device/*`）或裸 event_type。网站的 `--metric gpu` / `--join window` 依赖 GPU/窗口聚合层，**后置到 v0.2**（`current_state`/`agg_*` 表已建，采集器未写入） |
| `mcp` | ✅ v0.1 | 启动 MCP server（stdio，阻塞到 stdin 关闭） |

### Claude Desktop / 任意 MCP 客户端接入

```json
{
  "mcpServers": {
    "kynoptic": { "command": "kynoptic", "args": ["mcp"] }
  }
}
```

协议：换行分隔 JSON-RPC 2.0；实现 `initialize` / `tools/list` / `tools/call` /
`ping` 与通知吸收；五工具定义见《mcp-tool-spec-v1》。数据面偏差见 `CODE_NOTES.md` §5。

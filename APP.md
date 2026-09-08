# Kynoptic App (v0.1)

Kynoptic 的本机感知层（采集器 + CLI + MCP 工具面）Rust workspace，位于仓库 `crates/` 下。网页（`index.html` 等）与本目录无关。

## Workspace 布局

| Crate | 说明 |
|-------|------|
| `crates/core` (`kynoptic-core`) | 采集核心：14 个 v0.1 监控器（纯 windows-sys、零 PowerShell 子进程）、事件通道/写入线程、SQLite 存储层（编号 SQL 迁移）、查询/分析 |
| `crates/cli` (`kynoptic-cli`) | 命令行工具：统计/导出/报告/分析/数据库维护（bin 名 `kynoptic-ctl`） |
| `crates/mcp` (`kynoptic-mcp`) | MCP 工具面 stub：`get_current_status` / `get_summary` / `get_timeline` / `get_anomalies` / `wait_for` 注册与参数校验（传输层后置） |

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

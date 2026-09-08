# CODE_NOTES — v0.1 carve-out 决策记录

源：G:\DigitalPulse（digitalpulse-core 40 监控器）→ kynoptic（14 监控器）。依据：
`R8-v01采集裁剪.md`、`schema-ddl-draft-v1.md`、`mcp-tool-spec-v1.md`。

## 1. 监控器裁剪与 PowerShell 归零

保留 14 个 v0.1 监控器。其中两个上游有部分 PowerShell（WMI）依赖，为满足"零子进程"硬性要求做了替换：

- **process.rs**：CPU% 原为 WMI `Win32_PerfFormattedData_PerfProc_Process` 批量查询。
  改为原生 `GetProcessTimes` 差分（两次采样间 kernel+user 时间增量 / 墙钟增量），
  保留上游 EMA（α=0.3）平滑口径。`ps.rs` 已删除。
- **device.rs**：磁盘 I/O 速率原为 WMI `PerfDisk_PhysicalDisk`，无轻量原生等价 API
  （PDH/IOCTL 复杂度高）。v0.1 暂返回 `None`（`DeviceSnapshot.disk_io` 为 Option 且
  skip_serializing_if，自动省略）；`parse_disk_io` 纯函数与测试保留，待 v0.2 原生方案。

## 2. Schema 迁移 vs DDL 草案的偏差

《schema-ddl-draft-v1》中的统一事件表 `events(id, bucket_id, type_id, ts, duration_ms, data)`
与上游采集器的既有 `events(timestamp, event_type, event_action, event_data, ...)` 布局冲突。
v0.1 采集器/查询层/CLI 全部依赖既有布局，整体重写超出 carve-out 范围，故：

- `0001_init.sql`：继承上游采集基础表（events / sessions / metadata / daily_agg）。
- `0002_bucket_model.sql`：按草案落地 bucket 模型的注册与聚合表
  （schema_meta / buckets / event_types / agg_minute / agg_daily / current_state），
  使"新增监控器零 DDL 扩展"具备基础；统一事件表的切换后置到 v0.2。
- 迁移版本号仍记录在既有 `metadata.schema_version`（CURRENT_SCHEMA_VERSION=2），
  草案的 `schema_meta` 表已创建但暂不承担版本记录（避免双事实源）。

## 3. pet 剥离

按 R8 三节执行：删 `db/pet_state.rs`、`db/pet_memory.rs`、`queries/pet.rs`；
去 `collector.rs` 的 `ensure_pet_state()`、`db/mod.rs` 两个 mod 声明、
`queries/mod.rs` 的 `pub use pet::*`；`SCHEMA`/迁移不再建 pet 表，
`0002` 幂等 DROP 遗留 pet 表；constants.rs 删除进化阶段/里程碑常量及其测试。

## 4. 测试

- 迁移系统自带两个测试（全新库幂等、v1→v2 升级清理 pet 表）。
- battery.rs 补了纯逻辑测试（status_text 映射、ac_online 判定）。
- 其余无测试监控器（window/idle/session/audio/brightness/wifi/power_plan/两个 hook）
  主体是 WinAPI 直调，无可隔离的纯逻辑，未强行造测试（避免硬件依赖型测试）。
- clippy：`workspace.lints.rust.warnings = "deny"`；唯一逐项 allow 是
  process.rs 的 `missing_const_for_thread_local`（HashMap::new 非 const fn，无法用 const 块）。

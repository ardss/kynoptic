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

## 5. MCP 工具面 vs 《mcp-tool-spec-v1》的偏差（v0.1 实现记录）

MCP（crates/mcp）与 CLI 的 `now`/`query` 已是可用实现（stdio JSON-RPC 2.0，
兼容 Claude Desktop：`command: kynoptic, args: ["mcp"]`）。以下为与 spec/草案的
现实偏差，均为数据面受采集器现状所限，接口形状按 spec 保留：

- **数据源**：工具统一读 legacy `events` 表（§2 的偏差所致），`current_state` /
  `agg_minute` / `agg_daily` 已建表但采集器尚未写入。get_timeline 因此从
  window/switch 事件现算段落而非读聚合层；get_summary 与 ctl stats 同口径现算。
- **get_current_status**：spec 的"热路径 0 SQL（ArcSwap 快照）"v0.1 不成立——
  退化为每次查库：优先 `current_state` 表，缺失字段从最新事件推导
  （cpu/mem ← system/heartbeat、battery ← battery_status、foreground_app ←
  window/switch 列值截断 60 字符、idle ← 最新事件距今、apm_5min ← 近 5 分钟计数）。
  net_up/down_kbps、max_temp_c 暂恒 null（无对应写入）。采集器补写 current_state
  后无需改工具接口即可变热。
- **get_summary 对比口径**：`yesterday_same_period` = 昨日本地日起点 → 当前时刻
  −24h（与"同期"一致）；`focus_segments` 用分钟活动现算（≥5min 连续段，
  与 analyzer 同阈值），昨日同期对比仅对可区间化的计数指标给出。
- **get_timeline**：granularity=hour 把同一本地小时内连续同应用段合并；
  窗口标题不出全文（app 名优先，截断 60 字符），联动 spec 的 `--no-text` 降敏。
- **wait_for**：spec 要求"语义层规则引擎订阅，不轮询数据库"——v0.1 无规则引擎，
  实现为 2s 间隔查库轮询（timeout_sec 默认 300、上限 1800、下限 1 保留）。
  信号覆盖：late_night / low_battery / memory_pressure / marathon_session /
  network_down 可判定；thermal_hot / disk_almost_full 因 v0.1 无温度/磁盘监控器
  （R8 裁剪）恒不触发（永远 timeout 而非报错）。
- **Resource（`kynoptic://events/{bucket}`）与 subscriptions/listen**：按 spec 属
  可选项，v0.1 未实现（tools/list 只含五工具）。
- **敏感 bucket consent_token**：v0.1 采集器不写 clipboard 等敏感桶，工具面无需
  consent 校验；隐私降敏（标题截断）已在 timeline/foreground_app 内置。

## 6. CLI bin 双名

crates/cli 同时产出 `kynoptic` 与 `kynoptic-ctl` 两个 bin（同一 main.rs）。
网站 MCP 配置示例的 `command: "kynoptic"` 直接可用；历史文档中的
`kynoptic-ctl` 亦保留。cargo 会提示"file present in multiple build targets"
（同名源码双 bin 的正常提示，非告警级错误）。

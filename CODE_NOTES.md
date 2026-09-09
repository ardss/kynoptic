# CODE_NOTES — v0.1 carve-out 决策记录

源：G:\DigitalPulse（digitalpulse-core 40 监控器）→ kynoptic（14 监控器）。依据：
`R8-v01采集裁剪.md`、`schema-ddl-draft-v1.md`、`mcp-tool-spec-v1.md`。

## 1. 监控器裁剪与 PowerShell 归零

保留 14 个 v0.1 监控器。其中两个上游有部分 PowerShell（WMI）依赖，为满足"零子进程"硬性要求做了替换：

- **process.rs**：CPU% 原为 WMI `Win32_PerfFormattedData_PerfProc_Process` 批量查询。
  改为原生 `GetProcessTimes` 差分（两次采样间 kernel+user 时间增量 / 墙钟增量），
  保留上游 EMA（α=0.3）平滑口径。`ps.rs` 已删除。
- **device.rs**：磁盘 I/O 速率原为 WMI `PerfDisk_PhysicalDisk`，无轻量原生等价 API
  （PDH/IOCTL 复杂度高）。v0.1 曾暂返回 `None`。恢复后（见 §9）上游完整实现
  （`collect_disk_io` + WMI 脚本）已回归，但因 PS 子进程约束由
  `DeviceMonitor.enable_disk_io`（默认 `false`）门控——默认行为不变（disk_io 省略），
  显式开启后才采集，待原生方案落地后转默认。

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
  network_down 可判定。thermal_hot / disk_almost_full 原标注"恒不触发"——恢复
  监控器后已接通：`thermal_hot` 读 `system/thermal_snapshot.max_temp_celsius ≥ 80`
  （thermal 监控器默认关闭，启用后数据面才存在）；`disk_almost_full` 读
  `device/device_snapshot.disks[].used_percent ≥ 90`（device 默认启用，立即可用）。
  两者在数据缺失时仍是不触发（timeout 而非报错），语义不变。
- **Resource（`kynoptic://events/{bucket}`）与 subscriptions/listen**：按 spec 属
  可选项，v0.1 未实现（tools/list 只含五工具）。
- **敏感 bucket consent_token**：v0.1 采集器不写 clipboard 等敏感桶，工具面无需
  consent 校验；隐私降敏（标题截断）已在 timeline/foreground_app 内置。

## 6. CLI bin 双名

crates/cli 同时产出 `kynoptic` 与 `kynoptic-ctl` 两个 bin（同一 main.rs）。
网站 MCP 配置示例的 `command: "kynoptic"` 直接可用；历史文档中的
`kynoptic-ctl` 亦保留。cargo 会提示"file present in multiple build targets"
（同名源码双 bin 的正常提示，非告警级错误）。

## 7. perf2：opt-in 分钟粒度输入聚合 + 可配置 flush 间隔（2026-09-09）

**范围修正（用户决策）**：原始数据神圣。默认 `input_granularity = "raw"`——
键盘/鼠标 Hook 逐事件原样落库，与 v0.1 行为完全一致（"膨胀就膨胀"）。
分钟折叠仅作为 **opt-in** 磁盘优化实现，不做任何保留/清理/rollup（那是后续
产品决策，本轮明确不做）。

- `collector::CollectorSettings`：`input_granularity`（Raw[默认] | Minute）+
  `write_flush_interval_secs`（代码默认 30s，从 constants 提升为配置项）。
  **数据丢失风险 vs 磁盘足迹的 flush 默认值仍在评审中**，当前 30s 只是现状
  保留，非遗依赖结论。
- minute 模式：Hook 回调退化为纯原子计数（perf-hook 实测键盘 3.4ns / 移动
  8.9ns vs raw 全路径 ~1µs），`InputAgg` 线程每秒 drain，按**本地分钟**折叠
  为 `input_agg` 计数型事件（键盘 `{"keys","samples"}`、鼠标
  `{"clicks","scroll_ticks","moves","move_distance_px","samples"}`），
  ≤2 行/分钟；关停时 `flush_partial` 落库未满分钟的部分计数。
- 计数语义保持：APM、活跃分钟、daily_agg、异常检测全部只依赖计数——查询层
  用 `queries::KEYS_ROW_EXPR / CLICKS_ROW_EXPR` 统一兼容两种形态（含
  `json_valid` 防 malformed event_data）。
- 已知取舍：按键明细热力图与鼠标坐标热力图在 minute 模式下无原始样本
  （计数型行不含 per-key/坐标），属 opt-in 换磁盘的显式代价。

## 8. perf2：聚合读缓存（agg_minute / agg_daily）与懒回填（2026-09-09）

- 定位：**派生只读缓存**，`db/agg.rs` 单一实现。原始 events 只增不改不删；
  测试断言聚合维护（增量 + 两次全量重建）前后 events 行数与全行内容指纹
  完全不变（`agg_maintenance_never_touches_raw_events`）。
- bucket 语义（本地分钟桶）：`input_keys` / `input_clicks` / `input_moves`
  （sum=曼哈顿距离px, count=次数）/ `window_switches`；agg_daily 只存
  `app:<name>` 行（count=该应用当日事件数）。
- 维护路径三合一：writer flush 增量 UPSERT（`Database::update_agg`）、
  `Database::open` 懒回填（`backfill_if_needed`：agg 全空 + events 非空时
  全量重建一次；**选懒回填而非 CLI reagg 命令**——存量库零操作自动获益，
  CLI 子命令如后续运维需要再加）、`rebuild_all` 幂等全量重建。
- 查询端：异常路径（late_night / top_burst / active_minutes / minute_stats /
  day_totals / top_apps / app_history_totals）优先读缓存，**缓存缺失（表不
  存在 / 该日无行）回退 events 现算**——正确性不依赖回填成功，只是慢。
  效果：1M 行合成库 get_anomalies(7d) p50 1413→26.7ms、p95 2778→30.4ms
  （详见 BENCHMARKS.md §3）。
- 语义微差（记录在案）：缓存路径的分钟/小时桶按**本地时区**（与"今日"定义
  同源）；events 现算回退路径沿用 UTC substr 切桶。深夜检测在非 UTC 时区的
  回退路径上可能少计（旧有行为），缓存路径为本地口径。

## 9. monitors：恢复上游 26 个裁剪监控器，默认全关（2026-09-09）

**用户决策**："已经写了的代码不应该裁；敏感的默认关就行。"上游 40 个监控器
全部回到 `crates/core/src/monitors/`，v0.1 的 14-monitor 裁剪撤回为
**默认开关分层**：

- 单一事实源：`crates/core/src/registry.rs`（`MONITOR_REGISTRY`，40 项，含
  default_enabled / sensitivity（R8 ①低②中③高）/ dep（native|powershell））。
- 默认启用集合**精确等于 v0.1 的 14 个**（`registry::default_enabled_ids`，
  测试 `default_enabled_is_exactly_the_v01_fourteen` 锁死）——BENCHMARKS.md
  全部空载数字的前提不受影响。
- 恢复的 26 个全部默认关闭：R8 ② 级 12 个（中敏感，含纯原生的 browser/
  clipboard/file_activity/media/screen_capture/usb_device/bluetooth，与
  PS 依赖的 display/external_display/audio_input/audio_output/ime），
  R8 ③ 级 14 个（高敏感或主题外，全部 PS 依赖：location/notification/
  calendar/dns/security/firewall/uac/windows_update/driver/vpn/print/
  stylus/thermal/gpu）。
- **PS 子进程约束不变**：任何默认启用的监控器不 spawn 子进程。PS 依赖型
  （`monitors/ps.rs` 随之恢复）代码原样恢复、默认关闭，文件头统一标注
  "PS 子进程实现（powershell spawn），待原生 API 重写"。
- device.rs 的磁盘 I/O 速率为 PS 依赖但 device 在默认 14 内：以
  `DeviceMonitor::enable_disk_io`（默认 false）门控上游实现（见 §1）。
- 配置模板：`crates/core/config/monitors.json`（严格 JSON，readme + 40 项），
  测试 `config_template_matches_registry` 保证与注册表逐项一致。
- `registry::create_monitors_for(&HashSet<String>)` 是按启用集构建监控器的
  工厂；collector 默认路径走 `default_enabled_ids()`。运行时按需启用某个
  默认关闭的监控器 = 把它的 id 放进启用集（CLI 参数面后置）。
- MCP `wait_for` 的 thermal_hot / disk_almost_full 已接通真实数据面（§5）。

### 恢复清单（26，全部默认关闭）

| 监控器 | 依赖 | 级别 | | 监控器 | 依赖 | 级别 |
|---|---|---|---|---|---|---|
| browser | 原生 | ② | | location | PS | ③ |
| clipboard | 原生 | ② | | notification | PS | ③ |
| file_activity | 原生 | ② | | calendar | PS | ③ |
| media | 原生 | ② | | dns | PS | ③ |
| screen_capture | 原生 | ② | | security | PS | ③ |
| usb_device | 原生 | ② | | firewall | PS | ③ |
| bluetooth | 原生 | ② | | uac | PS | ③ |
| display | PS | ② | | windows_update | PS | ③ |
| external_display | PS | ② | | driver | PS | ③ |
| audio_input | PS | ② | | vpn | PS | ③ |
| audio_output | PS | ② | | print | PS | ③ |
| ime | PS | ② | | stylus | PS | ③ |
| thermal | PS | ③ | | gpu | PS | ③ |

## 10. qa：实机探针（kynoptic-ctl probe）与 probe 期间修复（2026-09-09）

新增 `kynoptic-ctl probe [--monitor ID] [--secs N] [--all]`（core 侧 `probe`
模块）：对单个监控器构造"仅启用它"的配置，在独立临时库上真实采集 N 秒
（1s flush），报告事件数/样本/警告与判定。--all 顺序跑满 40 个并输出矩阵。
探针配套 `examples/probe-stress-input.rs`（RSS/事件速率稳态观测）。
**合成输入压测 DEFERRED**：曾用 SendInput 注入做 60s 压测（实测 45 events/s
稳态、计数线性无丢失趋势、RSS 15.1→17.6MB 平稳、末窗 hook 存活），但注入会
直接干扰真实桌面会话（鼠标抖动/按键），应用户要求已移除全部合成输入代码，
示例改为纯被动观测。hook 健康性验证改为：perf-hook 回调成本基准（既有，
键盘全路径 1.03µs）+ probe 启停干净性 + 真实使用下的被动速率观测。

实机矩阵结论（Win11 26300 桌面机，15s 窗口）：**25 PASS / 15 EXPECTED-LIMITED
/ 0 FAIL**。EXPECTED-LIMITED 均有明确环境原因（无电池、外接屏无亮度接口、
无手写笔/VPN 网卡、Security 日志与 MSAcpi 需管理员、DNS/IME 事件日志为空、
未装 Outlook、change-driven 监控器窗口内无状态变化）。

probe 发现并修复的缺陷：

- **ps.rs**：`powershell -Command` 在 cmdlet 产生被 SilentlyContinue 压制的
  非终止错误时（Get-WinEvent 无记录等）即使 stdout 有效也退出码 1，原实现
  丢弃全部输出 → driver 等事件日志型监控器静默零事件；且管道输出走 OEM
  代码页（GBK），中文被 from_utf8_lossy 打碎。改为"stdout 非空即返回 +
  注入 UTF-8 OutputEncoding"。
- **network.rs**：GetIfTable2 裸指针硬编码偏移读取在 Win11 26300 上恒为 0
  （delta 恒 0 → 静默零事件），netstat -e 解析在中文系统上又因 GBK 失效，
  双路径同时死。改为类型化 MIB_IF_TABLE2（正确切片迭代，回环不计），
  netstat 降为兜底。
- **bluetooth.rs**：GUID_DEVCLASS_BLUETOOTH 类枚举在新系统对 BTHENUM 子设备
  返回空集 → 永远零事件。改按 BTHENUM 枚举器取远端设备（BTHENUM\DEV_ 前缀
  过滤），类 GUID 兜底。
- **audio_output.rs**：AudioEndpoint 的 FriendlyName 在该构建上为空串 →
  解析后设备列表为空 → 零事件。回退到 PnP Name。
- **collector.rs**：全局静态 SHUTDOWN 在同进程多次启停采集器（probe --all）
  时会把上一实例的监控线程"复活"成僵尸，并使关停尾部的 Disconnected 发送
  污染全局丢弃计数。改为每 Collector 一份停机旗标；send_event 只计 Full。
- battery：无电池时显式记一条 info（区分"硬件缺失"与"失效"）。

集成测试 flake 根因：临时库路径仅 pid+seq，Windows pid 快速复用 + 测试
panic/连接未关导致的遗留库在同日重跑时命中旧文件（计数翻倍/UNIQUE 冲突）。
修复：路径加入纳秒级分量（agg_cache/db_integration/queries_integration 三处），
全量 suite 连续 5 次零失败。

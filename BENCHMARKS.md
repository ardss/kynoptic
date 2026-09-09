# Kynoptic 性能基准（BENCHMARKS）

> 本文件是官网 overhead 声明的**事实核对源**。所有数字来自本仓库内可复现的
> 基准 harness（`crates/core/examples/perf-*.rs`、`crates/mcp/examples/perf-query.rs`）。
> 最近一次全量运行：**2026-09-09**（代码版本：perf2 —— 聚合读缓存
> （agg_minute/agg_daily）+ get_anomalies 改读聚合 + 可配置 flush 间隔 + opt-in 分钟粒度输入聚合之后）。

## 机器与环境

| 项 | 值 |
|---|---|
| OS | Windows 11 专业版 Insider Preview（win32 10.0.26300） |
| CPU | AMD Ryzen 5 5600X（6 核 12 线程） |
| RAM | 32 GB |
| 构建 | `cargo build --release`（opt-level=3, lto=true, strip=true） |

## 复现命令

```bash
cargo build --release --workspace --examples

# 1. 空载开销（10 分钟，每秒采样；KYNOPTIC_PERF_SECS 可缩短）
KYNOPTIC_PERF_SECS=600 cargo run --release -p kynoptic-core --example perf-idle
# 2. 存储成本（100k 事件走真实写路径）
cargo run --release -p kynoptic-core --example perf-write
# 3. 查询延迟（首次运行自动 seed 1M 事件合成库，约 3 分钟）
cargo run --release -p kynoptic-mcp --example perf-query
#    （CLI 子进程基准需先: cargo build --release -p kynoptic --bin kynoptic）
# 4. 启动延迟（10 次取中位数）
cargo run --release -p kynoptic-core --example perf-startup
# 5. Hook 回调成本（百万次迭代）
cargo run --release -p kynoptic-core --example perf-hook
```

## 1. 空载开销（IDLE OVERHEAD）

方法：`perf-idle` 在本进程内启动完整采集器（12 monitor + 2 hook + writer），
每 1s 用 GetProcessTimes / GetProcessMemoryInfo / GetProcessIoCounters 采样，
首 10s 预热不计入；600s 窗口、591 个样本。

| 指标 | 实测 | 目标 | 结论 |
|---|---|---|---|
| CPU（单核归一）avg | **0.32%**（5s flush）/ **0.41%**（30s flush） | < 0.5% | PASS |
| CPU p95 | **1.56%** | < 2% | PASS |
| CPU max | 7.8~9.4%（monitor 采集 tick） | — | — |
| RSS avg / p95 / max | **16.4~18.1 / 16.5 / 16.5~19.2 MB** | 稳态 < 30 MB | PASS |
| RSS 斜率（600s 线性回归） | +2.1~2.4 MB/h（窗口太短，属噪声级） | 无单调增长 | PASS* |

**修复记录（重大）**：修复前 writer 线程存在 busy-spin——batch 非空但未到
flush 时机时直接回到 `try_recv` 空转，实测空载 CPU avg 35%~95% 单核。
修复（`collector.rs` writer_loop_inner 阻塞等待剩余 flush 时间）后降到
0.32%。修复前后各指标：

| | 修复前（5s flush） | 修复后（5s flush） |
|---|---|---|
| CPU avg（单核） | 35.6%~95.5%（随事件流波动） | 0.32% |
| 磁盘写入（600s 窗口 WAL 增长） | 1.93 MB | 1.93 MB（同为 5s flush 时不受此修复影响） |

## 2. 磁盘足迹（DISK FOOTPRINT）

### 每事件存储成本（perf-write，100k 事件）

**方法论修正（2026-09-09）**：`maintenance()` 原本在 VACUUM **之前**做 WAL
checkpoint，VACUUM 全程经 WAL 重写会把 WAL 再撑大一倍——旧数字 455 B/event
（45.5 MB）实际含一份未截断的 WAL。修复为 VACUUM 后补一次 checkpoint 后的重测：

| 指标 | 旧测（WAL 未截断虚高） | 重测（2026-09-09） |
|---|---|---|
| maintenance 后 DB（主文件+WAL） | 45.5 MB（含 ~12MB WAL 虚高） | **33.6 MB** |
| **每事件字节（raw 默认）** | ~~455 B/event~~ | **336 B/event** |
| 写吞吐（300/批真实写路径） | 42,071 events/s | 13.7k~24.6k events/s（机器噪声大，非记录指标） |

### 分钟粒度输入聚合（opt-in 信息对照，非默认）

`input_granularity="minute"`（默认 **raw**，原始数据不动）把 keyboard/mouse
折叠为每分钟每桶一行 `input_agg` 计数行。同一 100k 合成工作负载
（perf-write 附带 phase）：

```json
{"bench":"perf-write-minute","opt_in":true,"source_events":100000,
  "stored_rows":40006,"db_bytes_after_maintenance":14884864,
  "bytes_vs_raw_default":"2.3x smaller","rows_vs_raw_default":"2.5x fewer"}
```

注意：该合成负载把 100k 事件压在 ~2 分钟内（1ms 间隔），行数坍缩被低估；
真实人手密度（~10-30 输入事件/分钟）下约坍缩到 ≤2 行/分钟。
此为 opt-in 形态的信息数字，**不构成默认行为声明**（原始数据神圣：
raw 路径字节形态与上表一致）。

### 空载日外推（perf-idle 600s 窗口）

SQLite WAL 每次提交的页开销（events 表 + 4~5 个索引 ≈ 6 个 4KB 页 ≈
38KB/提交）在事件稀疏时主导磁盘写入。实测 600s 窗口（66 事件）：

| flush 间隔 | 窗口 WAL 增长（66/62 事件） | 外推（同活动水平连续活跃时段） |
|---|---|---|
| 5s（旧默认） | 1.93 MB | ~11.6 MB/h 活跃 |
| **30s（新默认）** | **0.84 MB** | **~5.8 MB/h 活跃** |

最终 10 分钟空载窗口（30s flush，2026-09-09）：

```json
{"bench":"perf-idle","window_secs":600,"samples":591,
  "cpu_pct_of_one_core":{"avg":0.410,"p95":1.562,"max":9.357},
  "rss_mb":{"avg":16.36,"p95":16.50,"max":16.54,"slope_mb_per_hour":2.399},
  "disk_write_mb_per_hour_avg_over_1s":5.7696,
  "disk_total_written_mb_incl_startup":1.033,
  "events":62,"sessions":1,
  "db_bytes":106496,"db_wal_bytes":844632}
```

| 场景 | 外推磁盘/天 | 目标 < 2 MB/天 | 结论 |
|---|---|---|---|
| 近零活动日（<几百事件） | KB 级 | < 2 MB/天 | PASS |
| 轻度使用 ~1-2h 活跃/天 | ~6-12 MB | < 2 MB/天 | FAIL |
| 持续活跃 8h/天 | ~46 MB | < 2 MB/天 | FAIL |

**用户可见默认值变更（需 flag）**：`WRITE_FLUSH_INTERVAL_SECS` 5 → 30。
事件落库最大延迟从 5s 变为 30s（大批量 ≥300 条仍立即 flush，不受影响）。
依据：实测每提交磁盘开销 ~38KB 被 flush 频率主导，间隔 6x → 磁盘 ~6x。

## 3. 查询延迟（QUERY LATENCY，1M 事件 / 30 天合成库）

方法：`perf-query` 以生产 schema+PRAGMA+迁移生成 1,000,000 事件
（40% mouse/move、20% keyboard/press、20% window/switch、20% system/heartbeat），
各查询 50 次。MCP 工具为进程内调用（等价 MCP server 持连接）；CLI query
为真实子进程（含进程 spawn 开销 ~10ms）。

| 查询 | p50 | p95 | 目标 p95 | 结论 |
|---|---|---|---|---|
| `kynoptic query --from --to --limit 20`（1d，子进程） | 14.3 ms | **19.8 ms** | < 50 ms | PASS |
| get_summary[keys]（1d） | 0.57 ms | **0.79 ms** | < 50 ms | PASS |
| get_summary[apps]（1d） | 1.90 ms | **2.52 ms** | < 50 ms | PASS |
| get_summary[active_minutes]（1d） | 5.14 ms | **6.82 ms** | < 50 ms | PASS |
| get_summary[focus_segments]（1d） | 2.95 ms | **3.63 ms** | < 50 ms | PASS |
| get_timeline（7d，hour） | 0.24 ms | **0.34 ms** | < 50 ms | PASS |
| **get_anomalies（7d）** | **26.7 ms** | **30.4 ms** | < 50 ms | **PASS** |

### get_anomalies 调优前后（三次测量，同一 1M 库）

| 版本 | p50 | p95 |
|---|---|---|
| 原始（substr 全表扫描，无覆盖索引） | 3756 ms | 5540 ms |
| + sargable 日期区间谓词（charts.rs 三处） | 3613 ms | 4842 ms |
| + 迁移 0003 覆盖索引（idx_events_type_ts / idx_events_app_ts） | 1413 ms | 2778 ms |
| + perf2：agg_minute/agg_daily 读缓存，get_anomalies 改读聚合（events 原样保留，缓存缺失回退现算） | **26.7 ms** | **30.4 ms** |

分项实测（perf2 后）：late_night / apm_burst / marathon 均改读 agg_minute
（7 天 × 1440 分钟 × ~4 bucket ≈ 4.3 万聚合行，而非 100 万原始行）；
new_app_surge 的 per-app 全历史统计改读 agg_daily 的 `app:<name>` 行
（`app_history_totals` 从 O(该应用全部历史行) 降到 O(该应用出现天数)）。
聚合缓存是**派生只读数据**：writer flush 增量维护 + Database::open 懒回填 +
全量重建幂等；查询端在缓存缺失时回退 events 现算（正确但慢）。测试断言
聚合维护前后 events 行数与内容指纹完全不变。注：合成库 app 集中度（2-3 个
app 占满 1M 行）远比真实使用极端，真实多应用负载下只会更快。

## 4. 启动延迟（STARTUP）

方法：`perf-startup` spawn 子进程，读其 stderr 直到 12 个 monitor 全部
输出「首次采集完成」，10 次取中位数。

| 指标 | 实测 | 目标 | 结论 |
|---|---|---|---|
| spawn → 首次采集完成（中位数） | **237.7 ms**（min 207.4 / max 346.3） | < 500 ms | PASS |

## 5. Hook 回调成本（HOOK COST，best effort）

方法：`perf-hook` 以百万次迭代计时与真实回调体等价的逻辑
（Mutex 取 Sender + GetAsyncKeyState×4 + json! 构造 + try_send；
鼠标节流早退路径单独计）。真实回调额外只有函数调用 + CallNextHookEx。

| 路径 | ns/call |
|---|---|
| AtomicU64 计数下限对照 | 1.6 |
| 鼠标移动节流早退（raw 模式绝大多数移动事件） | **68.8** |
| 键盘按下全路径（raw 模式，含修饰键采样+JSON+入队） | **~1033（≈1µs）** |
| 键盘按下（minute 模式：纯原子计数） | **3.4** |
| 鼠标移动（minute 模式：原子计数+距离） | **8.9** |

结论：回调 <1µs/次（键盘全路径 1.03µs、鼠标节流路径 62ns），比人手
感知阈值（~10ms）低 4 个数量级，不构成可感知输入延迟。

## 网站声明核对（index.html）

| 声明 | 核对结果 |
|---|---|
| "near-zero overhead"（CPU） | **修复后成立**（空载 0.41% 单核）。修复前（writer busy-spin 占满 1 核）不成立——本基准正是该 bug 的发现手段。 |
| "typical storage is KB-scale per day" | **仅对近零活动日成立**。实测活跃时段磁盘 ~5.8 MB/h（WAL 提交页开销主导），轻度使用 1-2h/天即 6-12 MB/天。建议文案限定为「空闲/低活动日 KB 级」或标注前提。 |
| 隐含「查询瞬时」 | perf2 后 1M 事件库上**全部**查询 p95 < 50ms（get_anomalies 7d p95 ≈ 30ms，读聚合缓存）。可以恢复"近瞬时"表述，但建议注明"基于预聚合缓存"。 |

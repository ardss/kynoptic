//! 全局常量定义
//!
//! 集中管理所有魔法数字，避免散落在各文件中

// === 事件通道 ===
pub const CHANNEL_CAPACITY: usize = 20_000;

// === 写入线程 ===
pub const WRITE_BATCH_SIZE: usize = 300;
/// 小批 flush 间隔。perf-idle 2026-09 实测每次事务提交因 events 表 + 4~5 个
/// 索引要写 ~6 个 4KB WAL 页（≈38KB/提交），间隔过短时磁盘写入被提交开销
/// 主导：5s → 10 分钟窗口 WAL 增长 1.9MB；30s 在事件新鲜度（最大延迟 30s）
/// 与磁盘足迹之间取平衡（用户可见默认值变更，见 BENCHMARKS.md）。
pub const WRITE_FLUSH_INTERVAL_SECS: u64 = 30;

// === 后台线程间隔 ===
pub const MAINTENANCE_INTERVAL_SECS: u64 = 24 * 3600;
/// daily_agg 缓存刷新间隔。缓存读方（charts/insights）与实时读方（summary 卡片）
/// 并存，若随 24h 维护才刷新，两个面板最长差一整天的数。1h 上限保证交叉一致。
pub const DAILY_AGG_REFRESH_SECS: u64 = 600;
pub const TRAY_UPDATE_INTERVAL_SECS: u64 = 5;
/// SnapshotCache 后台重算间隔。前端 3s 轮询时读此缓存（0 SQL），
/// 2s 重算保证缓存新鲜度足够，同时比每次轮询都 collect（9 SQL）省 ~85% 查询。
pub const SNAPSHOT_CACHE_INTERVAL_SECS: u64 = 2;

// === 数据库 ===
/// 数据保留天数。0 = 永不删除（铁律：原始数据一字节不动；清理必须显式 opt-in）。
pub const DEFAULT_RETENTION_DAYS: i64 = 0;
pub const READER_POOL_SIZE: usize = 8;
/// 数据库文件名（不含目录）。路径解析见 db::resolve_db_path。
pub const DB_FILENAME: &str = "kynoptic.db";
/// 读连接池耗尽时最多等待的周期数（每周期 5s）。
/// 超过则降级为新建临时连接，避免调用方无限阻塞。
pub const READER_POOL_MAX_WAITS: u32 = 3;
pub const CURRENT_SCHEMA_VERSION: i64 = 4;

// === 行为分析阈值（analyzer） ===
/// 至少 N 分钟才算专注段
pub const FOCUS_MIN_MINUTES: i64 = 5;
/// 每分钟窗口切换超过 N 次视为碎片化
pub const FOCUS_MAX_WINDOW_SWITCHES_PER_MIN: i64 = 3;

// === 异常检测阈值（anomaly） ===
/// 23:00 之后视为深夜
pub const LATE_NIGHT_HOUR_START: u32 = 23;
/// 深夜至少 N 次按键才报警
pub const LATE_NIGHT_MIN_KEYS: i64 = 50;
/// 当分钟 APM >= 历史均值的 N 倍视为突增
pub const APM_BURST_MULTIPLIER: f64 = 3.0;
/// APM 突增的绝对值下限
pub const APM_BURST_MIN_KEYS: i64 = 100;
/// 持续活跃 N 分钟算"马拉松会话"
pub const MARATHON_MIN_MINUTES: i64 = 180;
/// 某应用事件数 >= 历史均值的 N 倍视为突增
pub const NEW_APP_SURGE_MULTIPLIER: f64 = 5.0;

// === Insights 告警阈值（src-tauri/commands/system.rs::get_insights） ===
/// 磁盘使用率超过此值触发"危险"告警
pub const DISK_ALERT_USED_PCT: f64 = 90.0;
/// 磁盘使用率超过此值触发"警告"
pub const DISK_WARN_USED_PCT: f64 = 70.0;
/// CPU 温度超过此值（摄氏度）触发"危险"告警
pub const THERMAL_ALERT_C: f64 = 80.0;
/// CPU 温度超过此值（摄氏度）触发"警告"
pub const THERMAL_WARN_C: f64 = 70.0;
/// 设备快照频率（小时）。用于根据历史 rows 数估算采样覆盖的天数。
/// 来自 DeviceMonitor::interval = 2h；写在此处是因为 insights 算法依赖此值反推日均增长。
pub const DEVICE_SNAPSHOT_INTERVAL_HOURS: f64 = 2.0;

// === Pet 信号阈值（src-tauri/semantics/engine.rs::SignalEngine::evaluate） ===
// 注意:与上面 Insights 告警阈值并列但语义不同 — 这里是宠物情绪反应的触发线,
// 高于告警线只是"宠物会反应",并非"系统报警"。
/// CPU 温度（°C）超过此值 → HotEnvironment 信号
pub const PET_HOT_TEMP_C: f64 = 80.0;
/// CPU 温度（°C）低于此值（且 CPU>20%、温度>0）→ ColdEnvironment 信号
pub const PET_COLD_TEMP_C: f64 = 30.0;
/// CPU 温度低于此值时不算"真实温度读数",跳过 ColdEnvironment
pub const PET_TEMP_INVALID_BELOW_C: f64 = 0.0;
/// CPU 使用率（%）超过此值 → BusyWorking 信号
pub const PET_BUSY_CPU_PCT: f64 = 80.0;
/// 内存使用率（%）超过此值 → MemoryPressure 信号
pub const PET_MEM_PRESSURE_PCT: f64 = 90.0;
/// 磁盘使用率（%）超过此值 → DiskFull 信号
pub const PET_DISK_FULL_PCT: f64 = 95.0;
/// 电量低于此值（% 且未充电）→ LowBattery 信号
pub const PET_LOW_BATTERY_PCT: f64 = 20.0;
/// APM 超过此值 → HighIntensity 信号
pub const PET_HIGH_INTENSITY_APM: f64 = 60.0;
/// 空闲秒数超过此值 → UserAway 信号
pub const PET_AWAY_SECONDS: f64 = 600.0;
/// 小时 < 此值 → LateNight（深夜）
pub const PET_LATE_NIGHT_HOUR: u32 = 5;
/// [LATE_NIGHT_HOUR, EARLY_MORNING_HOUR_END) 区间 → EarlyMorning（清晨）
pub const PET_EARLY_MORNING_HOUR_END: u32 = 8;
/// MarathonSession 触发的连续活跃分钟下限（与 anomaly::MARATHON_MIN_MINUTES 同源）
pub const PET_MARATHON_ACTIVE_MIN: i64 = MARATHON_MIN_MINUTES;
/// MarathonSession 触发后,空闲秒数低于此值才算"还在持续"
pub const PET_MARATHON_NOT_IDLE_BELOW_SECS: f64 = 300.0;
/// 今日总量较昨日增长超过此百分比 → Celebrating 信号
pub const PET_CELEBRATE_GROWTH_PCT: f64 = 20.0;

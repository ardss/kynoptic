//! M2.1 ETW 会话管理器（内核网络供应商 Microsoft-Windows-Kernel-Network）。
//!
//! 定位：**增强模式**网络监控的会话层——StartTrace/EnableTraceEx2/OpenTrace
//! 经 windows-sys 直调（复用 core 既有 windows-sys 依赖面，仅补
//! `Win32_System_Diagnostics_Etw` feature）。不在 [`crate::registry`] 的
//! 40 项基础集合内（采集器全开裁定不受影响），由上层显式启用增强模式时
//! 才实例化。
//!
//! 状态机：`Idle → Running → Stopped → Recover`——
//! - 启动前检测同名残留会话（异常退出恢复路径）并 stop（Recover 经历标记）；
//! - stop/Drop 保证 CloseTrace + StopTrace 资源释放（Drop 在 Running 态兜底）；
//! - 权限不足（非管理员 ETW 限制）→ quality=Unavailable + note_degraded
//!   显式留痕，不静默返回零。
//!
//! 消费专职线程：回调把记录推进通道，消费线程按 [`ETW_SINK_BATCH_MAX`]
//! 批量出站（[`chunk_records`] 纯函数，测试钉住保序与边界）。
//! M2.2：内核网络清单 V0 负载手写解析（[`parse_kernel_network_event`]）+
//! 分钟桶聚合（[`MinuteAggregator`]）产出 M1 [`crate::metrics::MetricPoint`]
//! （source=Etw，质量口径沿用 [`crate::metrics::MetricQuality`]；传输层字节
//! ≠应用载荷≠物理线路，差异由接口计数对账——口径见 [`NetFlowRecord`]）。

use crate::metrics::MetricQuality;
use std::sync::Arc;

/// ETW 实时会话专用名（残留清理只清同名，不碰他人会话）。
pub const ETW_SESSION_NAME: &str = "KynopticNetMon";

/// Microsoft-Windows-Kernel-Network 清单 GUID
/// {DDD0FCCD-3934-4E37-B454-31E8F4B9F803}（组件形存放，跨平台可编译；
/// logman `-p {guid}` 同源，真实路径启用即 smoke 验证）。
pub const KERNEL_NETWORK_PROVIDER_GUID_COMPONENTS: (u32, u16, u16, [u8; 8]) = (
    0xDDD0FCCD,
    0x3934,
    0x4E37,
    [0xB4, 0x54, 0x31, 0xE8, 0xF4, 0xB9, 0xF8, 0x03],
);

/// 消费线程单批出站上限（64 条）。
pub const ETW_SINK_BATCH_MAX: usize = 64;

/// 会话状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtwState {
    /// 未启动。
    Idle,
    /// 会话运行中（含消费线程）。
    Running,
    /// 已显式停止（可再次 start）。
    Stopped,
    /// 启动前清理过同名残留会话（异常退出恢复路径已经历）。
    Recover,
}

/// 后端错误（WIN32 错误码的语义收敛）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EtwError {
    /// ERROR_ACCESS_DENIED：非管理员启动 ETW 会话被拒。
    AccessDenied,
    /// ERROR_WMI_INSTANCE_NOT_FOUND / ERROR_NOT_FOUND。
    NotFound,
    /// 同名会话已存在（应走残留恢复路径）。
    AlreadyExists,
    /// 其它 WIN32 错误码。
    Win32(u32),
    /// 平台不支持（非 Windows 构建占位）。
    Unsupported,
}

/// 批量出站回调（专职消费线程持有，整批投递；M2.2 起承载流记录）。
pub type EventSink = Arc<dyn Fn(Vec<NetFlowRecord>) + Send + Sync>;

/// 可注入后端（真实 ETW / 测试 Mock）。句柄以 u64 通行（真实 TRACEHANDLE
/// 本就是 u64；Mock 用合成值），资源释放计数由 Mock 显式断言。
pub trait EtwBackend {
    /// 同名残留会话是否存在（异常退出恢复路径检测点）。
    fn query_residual(&self) -> bool;
    /// 停止同名会话（残留清理与正常停机共用 ControlTrace-STOP 收口）。
    fn stop_session(&mut self) -> Result<(), EtwError>;
    /// 启动会话，返回会话句柄。
    fn start_session(&mut self) -> Result<u64, EtwError>;
    /// 启用内核网络供应商。
    fn enable_provider(&mut self, session: u64) -> Result<(), EtwError>;
    /// 启动专职消费端，返回消费句柄。
    fn start_consumer(&mut self, session: u64, sink: EventSink) -> Result<u64, EtwError>;
    /// 关闭消费端（CloseTrace；真实实现须让 ProcessTrace 退出并回收线程）。
    fn close_consumer(&mut self, consumer: u64);
}

/// 会话管理器（状态机 + 资源生命周期；后端可注入）。
pub struct NetEtwMonitor<B: EtwBackend> {
    state: EtwState,
    quality: MetricQuality,
    recovered_residual: bool,
    handles: Option<(u64, u64)>, // (session, consumer)
    backend: B,
}

impl<B: EtwBackend> NetEtwMonitor<B> {
    pub fn new(backend: B) -> Self {
        Self {
            state: EtwState::Idle,
            quality: MetricQuality::Unavailable,
            recovered_residual: false,
            handles: None,
            backend,
        }
    }

    pub fn state(&self) -> EtwState {
        self.state
    }

    pub fn quality(&self) -> MetricQuality {
        self.quality
    }

    /// 本次进程内是否清理过同名残留会话（Recover 经历标记）。
    pub fn recovered_residual(&self) -> bool {
        self.recovered_residual
    }

    /// 启动：Idle/Stopped/Recover → Running。同名残留先 stop（Recover 标记）。
    /// AccessDenied：quality=Unavailable + note_degraded 显式留痕，Err 上抛
    /// （不静默返回零——调用方读 quality 即知不可用）。
    pub fn start(&mut self, sink: EventSink) -> Result<(), EtwError> {
        if self.state == EtwState::Running {
            return Ok(()); // 幂等：已运行不重复启动
        }
        self.recovered_residual = false;
        if self.backend.query_residual() {
            self.state = EtwState::Recover;
            self.recovered_residual = true;
            log::info!(
                "ETW 会话 {} 存在残留（上次异常退出），先停再启",
                ETW_SESSION_NAME
            );
            self.backend.stop_session()?;
        }
        let session = match self.backend.start_session() {
            Ok(h) => h,
            Err(EtwError::AccessDenied) => {
                // 显式降级：不静默。非管理员 ETW 限制是预期内的环境结局。
                self.quality = MetricQuality::Unavailable;
                self.state = EtwState::Idle;
                crate::monitors::note_degraded("net_etw: ETW 会话启动被拒（需要管理员权限）");
                return Err(EtwError::AccessDenied);
            }
            Err(e) => return Err(e),
        };
        self.backend.enable_provider(session)?;
        let consumer = self.backend.start_consumer(session, sink)?;
        self.handles = Some((session, consumer));
        self.state = EtwState::Running;
        self.quality = MetricQuality::Confirmed;
        Ok(())
    }

    /// 停止：Running → Stopped（CloseTrace + StopTrace）；非 Running 幂等 no-op。
    pub fn stop(&mut self) -> Result<(), EtwError> {
        let Some((_session, consumer)) = self.handles.take() else {
            return Ok(()); // 幂等：没有活的会话/消费端就无事可做
        };
        // 先关消费端（CloseTrace 使 ProcessTrace 退出）再停会话
        self.backend.close_consumer(consumer);
        let r = self.backend.stop_session();
        self.state = EtwState::Stopped;
        r
    }
}

impl<B: EtwBackend> Drop for NetEtwMonitor<B> {
    /// Drop 兜底：Running 态直接 drop（进程退出路径）也必须 CloseTrace +
    /// StopTrace，不留孤儿会话（下次 start 走 Recover 恢复）。
    fn drop(&mut self) {
        if self.state == EtwState::Running {
            let _ = self.stop();
        }
    }
}

/// 批量分片纯函数（保序；空输入输出空；上限内原样单批）。
pub fn chunk_records(records: Vec<NetFlowRecord>, max: usize) -> Vec<Vec<NetFlowRecord>> {
    if records.is_empty() {
        return Vec::new();
    }
    records
        .chunks(max.max(1))
        .map(<[NetFlowRecord]>::to_vec)
        .collect()
}

/// 丢事件观测（ETW 回调/会话统计 → loss_count/coverage 的数据源）。
/// seen=已交付解析的记录数；lost=会话报告的缓冲丢失事件数
/// （EVENT_TRACE_PROPERTIES.EventsLost，stop 时收口）。
#[derive(Default)]
pub struct LossStats {
    pub seen: std::sync::atomic::AtomicU64,
    pub lost: std::sync::atomic::AtomicU64,
}

impl LossStats {
    pub fn counts(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.seen.load(Relaxed), self.lost.load(Relaxed))
    }
}

// ─── 真实后端（windows-sys 直调） ──────────────────────────────────────────

/// 真实 ETW 后端。
pub struct RealEtw {
    /// 批量出站线程（recv_timeout 窗口）；close 置停机旗标后 join。
    batcher_thread: Option<std::thread::JoinHandle<()>>,
    /// 事件通道发送端（回调经 UserContext 直取；close 时丢弃触发停机）。
    tx: Option<Arc<crossbeam_channel::Sender<NetFlowRecord>>>,
    /// 批量出站线程停机旗标。
    stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 丢事件观测（回调 seen / stop 时 EventsLost 收口）。
    stats: Arc<LossStats>,
}

impl RealEtw {
    pub fn new() -> Self {
        Self {
            batcher_thread: None,
            tx: None,
            stop_flag: None,
            stats: Arc::new(LossStats::default()),
        }
    }
}

impl Default for RealEtw {
    fn default() -> Self {
        Self::new()
    }
}

impl RealEtw {
    /// (seen, lost) 快照——供 MinuteAggregator::drain_metric_points 填
    /// loss_count/coverage。
    pub fn loss_stats(&self) -> (u64, u64) {
        self.stats.counts()
    }
}

#[cfg(windows)]
mod real {
    use super::*;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Diagnostics::Etw::{
        CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
        CONTROLTRACE_HANDLE, ENABLE_TRACE_PARAMETERS, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
        EVENT_RECORD, EVENT_TRACE_CONTROL_QUERY, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW,
        EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, PROCESSTRACE_HANDLE,
        PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, TRACE_LEVEL_INFORMATION,
    };

    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_WMI_INSTANCE_NOT_FOUND: u32 = 4201;
    const ERROR_ALREADY_EXISTS: u32 = 183;
    const WNODE_FLAG_TRACED_GUID: u32 = 0x0002_0000;
    const INVALID_PROCESSTRACE_HANDLE: u64 = u64::MAX;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn to_err(code: u32) -> EtwError {
        match code {
            ERROR_ACCESS_DENIED => EtwError::AccessDenied,
            ERROR_WMI_INSTANCE_NOT_FOUND => EtwError::NotFound,
            ERROR_ALREADY_EXISTS => EtwError::AlreadyExists,
            c => EtwError::Win32(c),
        }
    }

    /// 回调用户上下文：通道 + 丢事件观测（leaked Arc，会话级一次性，
    /// EVENT_RECORD.UserContext 原样回传）。
    struct ConsumerCtx {
        tx: Arc<crossbeam_channel::Sender<NetFlowRecord>>,
        stats: Arc<LossStats>,
    }

    unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
        let ctx_ptr = (*record).UserContext as *const ConsumerCtx;
        if ctx_ptr.is_null() {
            return;
        }
        let ctx = &*ctx_ptr;
        let r = &*record;
        let header = &r.EventHeader;
        // FILETIME(100ns since 1601) → UTC epoch 毫秒
        let ts_ms = header.TimeStamp / 10_000 - 11_644_473_600_000;
        // M2.2：内核网络清单 V0 负载手写解析（字节量口径）；连接类事件与
        // 未知 id 返回 None 不出站（seen 只计交付的数据记录）
        let payload =
            std::slice::from_raw_parts(r.UserData as *const u8, r.UserDataLength as usize);
        if let Some(flow) = parse_kernel_network_event(header.EventDescriptor.Id, payload, ts_ms) {
            ctx.stats
                .seen
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = ctx.tx.send(flow);
        }
    }

    /// 构造会话属性块（EVENT_TRACE_PROPERTIES + 尾随 LoggerName 宽串）。
    unsafe fn make_session_properties(buffer: &mut [u8]) -> *mut EVENT_TRACE_PROPERTIES {
        let props = buffer.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        std::ptr::write_bytes(props, 0, 1);
        let size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        (*props).Wnode.BufferSize = buffer.len() as u32;
        (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*props).LoggerNameOffset = size as u32;
        (*props).LogFileNameOffset = 0; // 实时会话无需日志文件
        let name = wide(ETW_SESSION_NAME);
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buffer.as_mut_ptr().add(size) as *mut u16,
            name.len(),
        );
        props
    }

    fn query_session() -> Result<bool, EtwError> {
        let mut buffer = vec![0u8; std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + 512];
        let props = unsafe { make_session_properties(&mut buffer) };
        let code = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                wide(ETW_SESSION_NAME).as_ptr(),
                props,
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        match code {
            ERROR_SUCCESS => Ok(true),
            ERROR_WMI_INSTANCE_NOT_FOUND => Ok(false),
            c => Err(to_err(c)),
        }
    }

    impl RealEtw {
        fn query_residual_real(&self) -> bool {
            matches!(query_session(), Ok(true))
        }
    }

    impl EtwBackend for RealEtw {
        fn query_residual(&self) -> bool {
            self.query_residual_real()
        }

        fn stop_session(&mut self) -> Result<(), EtwError> {
            let mut buffer = vec![0u8; std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + 512];
            let props = unsafe { make_session_properties(&mut buffer) };
            let code = unsafe {
                ControlTraceW(
                    CONTROLTRACE_HANDLE { Value: 0 },
                    wide(ETW_SESSION_NAME).as_ptr(),
                    props,
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            match code {
                ERROR_SUCCESS | ERROR_WMI_INSTANCE_NOT_FOUND => {
                    // 会话已不存在（对端刚退出）即达成目的。
                    // STOP 后属性块带回终局统计：缓冲丢失事件数收口进
                    // loss_count/coverage 管道（残段会话的丢失同样计入，
                    // 同名会话同管道）。
                    let lost = unsafe { (*props).EventsLost } as u64;
                    if lost > 0 {
                        self.stats
                            .lost
                            .fetch_add(lost, std::sync::atomic::Ordering::Relaxed);
                    }
                    Ok(())
                }
                c => Err(to_err(c)),
            }
        }

        fn start_session(&mut self) -> Result<u64, EtwError> {
            let mut buffer = vec![0u8; std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + 512];
            let props = unsafe { make_session_properties(&mut buffer) };
            let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
            let code = unsafe { StartTraceW(&mut handle, wide(ETW_SESSION_NAME).as_ptr(), props) };
            if code != ERROR_SUCCESS {
                return Err(to_err(code));
            }
            Ok(handle.Value)
        }

        fn enable_provider(&mut self, session: u64) -> Result<(), EtwError> {
            let (d1, d2, d3, d4) = KERNEL_NETWORK_PROVIDER_GUID_COMPONENTS;
            let guid = windows_sys::core::GUID {
                data1: d1,
                data2: d2,
                data3: d3,
                data4: d4,
            };
            let params: ENABLE_TRACE_PARAMETERS = unsafe { std::mem::zeroed() };
            let code = unsafe {
                EnableTraceEx2(
                    CONTROLTRACE_HANDLE { Value: session },
                    &guid,
                    EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                    TRACE_LEVEL_INFORMATION as u8, // 4
                    u64::MAX,                      // 全 keyword
                    0,
                    0,
                    &params,
                )
            };
            if code != ERROR_SUCCESS {
                return Err(to_err(code));
            }
            Ok(())
        }

        fn start_consumer(&mut self, _session: u64, sink: EventSink) -> Result<u64, EtwError> {
            let (tx, rx) = crossbeam_channel::unbounded::<NetFlowRecord>();
            let tx = Arc::new(tx);
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            // 专职批量出站线程：回调推通道，线程按 64 条/100ms 窗口整批投递；
            // 停机旗标置位且通道排空后退出（CloseTrace 后最多 ~100ms 延迟）
            let stop_b = stop.clone();
            let sink_b = sink.clone();
            let batcher = std::thread::spawn(move || loop {
                let mut batch: Vec<NetFlowRecord> = Vec::new();
                match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(r) => batch.push(r),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                }
                while batch.len() < ETW_SINK_BATCH_MAX {
                    match rx.try_recv() {
                        Ok(r) => batch.push(r),
                        Err(_) => break,
                    }
                }
                if !batch.is_empty() {
                    sink_b(batch);
                }
                if stop_b.load(std::sync::atomic::Ordering::Relaxed) {
                    while let Ok(r) = rx.try_recv() {
                        sink_b(vec![r]);
                    }
                    return;
                }
            });
            self.batcher_thread = Some(batcher);
            self.stop_flag = Some(stop);
            self.tx = Some(tx.clone());
            // OpenTrace 挂回调 + ProcessTrace 阻塞在专属线程（不占调用方）。
            // 回调用户上下文 = 通道指针（会话级一次性 leaked，非事件级；
            // EVENT_RECORD.UserContext 原样回传给回调）。
            let ctx = Arc::into_raw(Arc::new(ConsumerCtx {
                tx: tx.clone(),
                stats: self.stats.clone(),
            })) as *mut ConsumerCtx;
            let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
            let name = wide(ETW_SESSION_NAME);
            logfile.LoggerName = name.as_ptr() as *mut u16;
            logfile.Context = ctx as *mut core::ffi::c_void;
            logfile.Anonymous1.ProcessTraceMode = EVENT_TRACE_REAL_TIME_MODE
                | PROCESS_TRACE_MODE_REAL_TIME
                | PROCESS_TRACE_MODE_EVENT_RECORD;
            // union 字段写入是安全操作（读取才 unsafe）
            logfile.Anonymous2.EventRecordCallback = Some(on_event);
            let consumer = unsafe { OpenTraceW(&mut logfile) };
            if consumer.Value == INVALID_PROCESSTRACE_HANDLE {
                return Err(EtwError::Win32(ERROR_ACCESS_DENIED));
            }
            let h = consumer;
            std::thread::spawn(move || unsafe {
                ProcessTrace(&h, 1, std::ptr::null(), std::ptr::null());
            });
            Ok(consumer.Value)
        }

        fn close_consumer(&mut self, consumer: u64) {
            unsafe {
                CloseTrace(PROCESSTRACE_HANDLE { Value: consumer });
            }
            // CloseTrace 使 ProcessTrace 返回；置停机旗标、丢弃发送端，
            // 批量出站线程排空残余后退出（最多 ~100ms 窗口）
            if let Some(f) = &self.stop_flag {
                f.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            self.tx = None;
            if let Some(t) = self.batcher_thread.take() {
                let _ = t.join();
            }
            self.stop_flag = None;
        }
    }
}

#[cfg(not(windows))]
impl EtwBackend for RealEtw {
    fn query_residual(&self) -> bool {
        false
    }
    fn stop_session(&mut self) -> Result<(), EtwError> {
        Err(EtwError::Unsupported)
    }
    fn start_session(&mut self) -> Result<u64, EtwError> {
        Err(EtwError::Unsupported)
    }
    fn enable_provider(&mut self, _session: u64) -> Result<(), EtwError> {
        Err(EtwError::Unsupported)
    }
    fn start_consumer(&mut self, _session: u64, _sink: EventSink) -> Result<u64, EtwError> {
        Err(EtwError::Unsupported)
    }
    fn close_consumer(&mut self, _consumer: u64) {}
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    /// 共享计数器（Drop 后仍可断言，故放 Arc）
    #[derive(Default, Clone)]
    struct Counters(Arc<Mutex<MockCounts>>);

    #[derive(Default, Clone)]
    struct MockCounts {
        sessions_started: u64,
        sessions_stopped: u64,
        residual_stops: u64,
        consumers_opened: u64,
        consumers_closed: u64,
        provider_enabled: bool,
        records: Vec<NetFlowRecord>,
    }

    impl Counters {
        fn get(&self) -> MockCounts {
            self.0.lock().unwrap().clone()
        }
    }

    /// 注入后端：可预置残留会话与启动失败模式，全路径状态机驱动。
    struct MockBackend {
        counters: Counters,
        residual: bool,
        fail_start: Option<EtwError>,
    }

    impl MockBackend {
        fn new(residual: bool) -> (Self, Counters) {
            let counters = Counters::default();
            (
                Self {
                    counters: counters.clone(),
                    residual,
                    fail_start: None,
                },
                counters,
            )
        }
    }

    impl EtwBackend for MockBackend {
        fn query_residual(&self) -> bool {
            self.residual
        }
        fn stop_session(&mut self) -> Result<(), EtwError> {
            let mut c = self.counters.0.lock().unwrap();
            // 残留清理与正常停机同走 stop：首次调用收会话，后续收消费者
            c.sessions_stopped += 1;
            c.residual_stops += 1;
            self.residual = false;
            Ok(())
        }
        fn start_session(&mut self) -> Result<u64, EtwError> {
            let mut c = self.counters.0.lock().unwrap();
            c.sessions_started += 1;
            match &self.fail_start {
                Some(e) => Err(e.clone()),
                None => Ok(1000 + c.sessions_started),
            }
        }
        fn enable_provider(&mut self, _session: u64) -> Result<(), EtwError> {
            self.counters.0.lock().unwrap().provider_enabled = true;
            Ok(())
        }
        fn start_consumer(&mut self, _session: u64, sink: EventSink) -> Result<u64, EtwError> {
            let mut c = self.counters.0.lock().unwrap();
            c.consumers_opened += 1;
            c.records.push(NetFlowRecord {
                timestamp_utc_ms: 1_749_000_000_000,
                pid: 100,
                direction: NetDirection::Out,
                proto: NetProto::Tcp,
                ip_version: 4,
                remote: vec![1, 2, 3, 4],
                remote_port: 443,
                bytes: 10,
            });
            sink(c.records.clone());
            Ok(5000 + c.consumers_opened)
        }
        fn close_consumer(&mut self, _consumer: u64) {
            self.counters.0.lock().unwrap().consumers_closed += 1;
        }
    }

    #[test]
    fn etw_session_name_is_dedicated_and_within_limits() {
        // 会话名专用（不与他人会话撞名，残留清理只敢清自己的）
        assert_eq!(ETW_SESSION_NAME, "KynopticNetMon");
        // ETW 会话名上限 128（EVENT_TRACE_MAX_SESSION_NAME），留余量断言
        assert!(
            ETW_SESSION_NAME.chars().count() < 128,
            "会话名必须短于 ETW 128 上限"
        );
    }

    #[test]
    fn kernel_network_provider_guid_constant() {
        // Microsoft-Windows-Kernel-Network 清单 GUID（logman -p 同源）
        assert_eq!(
            KERNEL_NETWORK_PROVIDER_GUID_COMPONENTS,
            (
                0xDDD0FCCD,
                0x3934,
                0x4E37,
                [0xB4, 0x54, 0x31, 0xE8, 0xF4, 0xB9, 0xF8, 0x03]
            )
        );
    }

    fn noop_sink() -> EventSink {
        Arc::new(|_| {})
    }

    #[test]
    fn state_machine_idle_to_running_via_injected_backend() {
        let (backend, counters) = MockBackend::new(false);
        let mut m = NetEtwMonitor::new(backend);
        assert_eq!(m.state(), EtwState::Idle);
        m.start(noop_sink()).expect("无残留启动必须成功");
        assert_eq!(m.state(), EtwState::Running);
        assert_eq!(m.quality(), MetricQuality::Confirmed, "正常启动即实测确认");
        let c = counters.get();
        assert_eq!(c.sessions_started, 1);
        assert!(c.provider_enabled, "启动必须启用内核网络供应商");
        assert_eq!(c.consumers_opened, 1, "必须有专职消费端");
    }

    #[test]
    fn residual_session_recovery_visits_recover_state() {
        // 异常退出恢复路径：同名残留会话先 stop 再启动
        let (backend, counters) = MockBackend::new(true);
        let mut m = NetEtwMonitor::new(backend);
        m.start(noop_sink()).expect("残留恢复启动必须成功");
        assert_eq!(m.state(), EtwState::Running);
        assert!(
            m.recovered_residual(),
            "启动前清过残留必须置 Recover 经历标记"
        );
        let c = counters.get();
        assert!(c.residual_stops >= 1, "启动前必须先停掉同名残留会话");
        assert_eq!(c.sessions_started, 1);
    }

    #[test]
    fn access_denied_degrades_explicitly_not_silently() {
        // 非管理员 ETW 限制：quality=Unavailable + note_degraded 留痕，
        // 不静默返回零/不产出伪造数据
        let before = crate::monitors::MONITOR_DEGRADED.load(Ordering::Relaxed);
        let (mut backend, _counters) = MockBackend::new(false);
        backend.fail_start = Some(EtwError::AccessDenied);
        let mut m = NetEtwMonitor::new(backend);
        let sink_hits = Arc::new(Mutex::new(0usize));
        let sink_hits2 = sink_hits.clone();
        let sink: EventSink = Arc::new(move |_| {
            *sink_hits2.lock().unwrap() += 1;
        });
        assert!(m.start(sink).is_err(), "权限不足必须显式报错");
        assert_eq!(m.quality(), MetricQuality::Unavailable, "权限不足=不可用");
        assert_ne!(m.state(), EtwState::Running);
        assert_eq!(*sink_hits.lock().unwrap(), 0, "降级不得产出任何数据");
        let after = crate::monitors::MONITOR_DEGRADED.load(Ordering::Relaxed);
        assert!(after > before, "降级必须经 note_degraded 显式留痕");
    }

    #[test]
    fn stop_transitions_to_stopped_and_releases_handles() {
        let (backend, counters) = MockBackend::new(false);
        let mut m = NetEtwMonitor::new(backend);
        m.start(noop_sink()).unwrap();
        m.stop().expect("停机必须成功");
        assert_eq!(m.state(), EtwState::Stopped);
        let c = counters.get();
        assert_eq!(c.sessions_stopped, 1);
        assert_eq!(c.consumers_closed, 1, "消费端句柄必须关闭");
    }

    #[test]
    fn stop_is_idempotent_when_not_running() {
        let (backend, counters) = MockBackend::new(false);
        let mut m = NetEtwMonitor::new(backend);
        m.stop().expect("Idle 上 stop 必须是安全 no-op");
        m.start(noop_sink()).unwrap();
        m.stop().unwrap();
        m.stop().expect("Stopped 上二次 stop 必须幂等");
        let c = counters.get();
        assert_eq!(c.sessions_stopped, 1, "幂等 stop 不得重复释放");
        assert_eq!(c.consumers_closed, 1);
    }

    #[test]
    fn restart_after_stop_can_run_again() {
        let (backend, counters) = MockBackend::new(false);
        let mut m = NetEtwMonitor::new(backend);
        m.start(noop_sink()).unwrap();
        m.stop().unwrap();
        m.start(noop_sink()).expect("停机后重启必须成功");
        assert_eq!(m.state(), EtwState::Running);
        let c = counters.get();
        assert_eq!(c.sessions_started, 2);
        assert_eq!(c.consumers_opened, 2);
    }

    #[test]
    fn drop_while_running_releases_session_and_consumer() {
        // Drop 资源释放断言：Running 态直接 drop（模拟进程退出路径），
        // 会话 StopTrace 与消费端 CloseTrace 都必须发生
        let counters;
        {
            let (backend, c) = MockBackend::new(false);
            counters = c;
            let mut m = NetEtwMonitor::new(backend);
            m.start(noop_sink()).unwrap();
            assert_eq!(m.state(), EtwState::Running);
            // 无 stop，直接 drop
        }
        let c = counters.get();
        assert_eq!(c.sessions_stopped, 1, "Drop 必须停会话（StopTrace）");
        assert_eq!(c.consumers_closed, 1, "Drop 必须关消费端（CloseTrace）");
    }

    #[test]
    fn chunk_records_batches_at_max() {
        let rec = |i: u64| NetFlowRecord {
            timestamp_utc_ms: i as i64,
            pid: i as u32,
            direction: NetDirection::Out,
            proto: NetProto::Tcp,
            ip_version: 4,
            remote: vec![1, 2, 3, 4],
            remote_port: 443,
            bytes: i,
        };
        let records: Vec<NetFlowRecord> = (0..70).map(rec).collect();
        let batches = chunk_records(records, ETW_SINK_BATCH_MAX);
        assert_eq!(batches.len(), 2, "70 条按 64 上限应分 2 批");
        assert_eq!(batches[0].len(), 64);
        assert_eq!(batches[1].len(), 6);
        assert_eq!(batches[0][0].timestamp_utc_ms, 0, "分批必须保序");
        assert_eq!(batches[1][5].timestamp_utc_ms, 69, "分批必须保序");

        // 边界：恰一批 / 空输入
        let one: Vec<NetFlowRecord> = (0..64).map(rec).collect();
        assert_eq!(chunk_records(one, ETW_SINK_BATCH_MAX).len(), 1);
        assert!(chunk_records(Vec::new(), ETW_SINK_BATCH_MAX).is_empty());
    }

    /// 真实环境 smoke（cfg(windows)）：启停一次无泄漏报错。非管理员环境下
    /// ETW 启动必然 AccessDenied——两种结局都合法：Running→Stopped 干净
    /// 启停，或显式降级（Unavailable + 留痕）；唯独不允许「声称 Running 却
    /// 没有句柄」或静默失败。
    #[cfg(windows)]
    #[test]
    fn real_etw_smoke_start_stop_once() {
        let before = crate::monitors::MONITOR_DEGRADED.load(Ordering::Relaxed);
        let hits = Arc::new(Mutex::new(Vec::<NetFlowRecord>::new()));
        let hits2 = hits.clone();
        let sink: EventSink = Arc::new(move |batch| hits2.lock().unwrap().extend(batch));
        let mut m = NetEtwMonitor::new(RealEtw::new());
        let started = m.start(sink);
        match started {
            Ok(()) => {
                assert_eq!(m.state(), EtwState::Running);
                assert_eq!(m.quality(), MetricQuality::Confirmed);
                m.stop().expect("smoke 停机不得失败");
                assert_eq!(m.state(), EtwState::Stopped);
            }
            Err(EtwError::AccessDenied) => {
                // 非管理员：显式降级路径
                assert_eq!(m.quality(), MetricQuality::Unavailable);
                let after = crate::monitors::MONITOR_DEGRADED.load(Ordering::Relaxed);
                assert!(after > before, "非管理员降级必须留痕");
            }
            Err(e) => panic!("smoke 只允许成功或 AccessDenied 两种结局，实际 {e:?}"),
        }
    }
}

/// 网络流方向（事件语义直译：transmitted=Out、received=In）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetDirection {
    In,
    Out,
}

/// 传输层协议（清单事件族：10-18 TCPv4、26-34 TCPv6、42-49 UDPv4、58+ UDPv6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetProto {
    Tcp,
    Udp,
}

/// 单笔数据事件解析产物（M2.2 信封；字节量即清单 `size` 字段）。
#[derive(Debug, Clone, PartialEq)]
pub struct NetFlowRecord {
    /// UTC epoch 毫秒（EVENT_HEADER.TimeStamp，测试注入）。
    pub timestamp_utc_ms: i64,
    pub pid: u32,
    pub direction: NetDirection,
    pub proto: NetProto,
    /// 4 或 6。
    pub ip_version: u8,
    /// 远端地址载荷原样字节（4/16B；Out=daddr、In=saddr，清单 Description 语义）。
    pub remote: Vec<u8>,
    /// 远端端口（win:Port 线上大端，按 BE 解码）。
    pub remote_port: u16,
    /// 传输层字节量（清单 `size`）。
    pub bytes: u64,
}

/// 口径注释（需求原文）：传输层字节 ≠ 应用载荷 ≠ 物理线路——TCP 头/重传/
/// 链路开销都不在此列，与网卡接口计数的差异由接口计数对账（M2.3 出对账口径）。
/// event id → (方向, 协议, 地址宽, 是否带 startime/endtime)。清单原文提取
/// （本机 TDH，Get-WinEvent -ListProvider，2026-11-01）。
const KERNEL_NET_DATA_EVENTS: &[(u16, NetDirection, NetProto, u8, bool)] = &[
    (10, NetDirection::Out, NetProto::Tcp, 4, true), // TCPv4 transmitted
    (11, NetDirection::In, NetProto::Tcp, 4, false), // TCPv4 received
    (26, NetDirection::Out, NetProto::Tcp, 6, true), // TCPv6 transmitted
    (27, NetDirection::In, NetProto::Tcp, 6, false), // TCPv6 received
    (42, NetDirection::Out, NetProto::Udp, 4, false), // UDPv4 transmitted
    (43, NetDirection::In, NetProto::Udp, 4, false), // UDPv4 received
    (58, NetDirection::Out, NetProto::Udp, 6, false), // UDPv6 transmitted
    (59, NetDirection::In, NetProto::Udp, 6, false), // UDPv6 received
];

/// 解析一笔内核网络数据事件（清单 V0 定长布局，字段紧排无对齐：
/// PID u32, size u32, daddr, saddr, dport u16BE, sport u16BE,
/// [startime u32, endtime u32,] seqnum u32, connid u32）。
/// 未知 id / 连接类事件 / 截断载荷 → None（不伪造记录）。
pub fn parse_kernel_network_event(
    event_id: u16,
    payload: &[u8],
    timestamp_utc_ms: i64,
) -> Option<NetFlowRecord> {
    let (direction, proto, addr_len, with_time) = KERNEL_NET_DATA_EVENTS
        .iter()
        .find(|(id, ..)| *id == event_id)
        .map(|(_, d, p, a, t)| (*d, *p, *a, *t))?;
    // addr_len 存的是 IP 版本（4/6），字节宽由版本定（v4=4B，v6=16B）
    let addr_bytes = if addr_len == 6 { 16usize } else { 4usize };
    // 4 + 4 + addr*2 + 2 + 2 + (8 if with_time) + 4 + 4
    let expect = 4 + 4 + addr_bytes * 2 + 2 + 2 + if with_time { 8 } else { 0 } + 8;
    if payload.len() < expect {
        return None;
    }
    let rd32 = |o: usize| u32::from_le_bytes(payload[o..o + 4].try_into().unwrap());
    let rd16be = |o: usize| u16::from_be_bytes(payload[o..o + 2].try_into().unwrap());
    let pid = rd32(0);
    let size = rd32(4);
    let daddr_off = 8;
    let saddr_off = daddr_off + addr_bytes;
    let dport_off = saddr_off + addr_bytes;
    let sport_off = dport_off + 2;
    // 方向语义（清单 Description）：transmitted「from saddr:sport to
    // daddr:dport」→ 远端=daddr；received「from saddr:sport」→ 远端=saddr
    let (remote_off, remote_port) = match direction {
        NetDirection::Out => (daddr_off, rd16be(dport_off)),
        NetDirection::In => (saddr_off, rd16be(sport_off)),
    };
    Some(NetFlowRecord {
        timestamp_utc_ms,
        pid,
        direction,
        proto,
        ip_version: addr_len,
        remote: payload[remote_off..remote_off + addr_bytes].to_vec(),
        remote_port,
        bytes: size as u64,
    })
}

/// 分钟桶聚合器：key = (minute_bucket, pid, direction, proto) → 传输层字节。
/// 事件乱序天然免疫（桶由事件自身时间戳决定，与到达顺序无关）；
/// `drain_metric_points` 出站后清桶（不重复出站）。
#[derive(Default)]
pub struct MinuteAggregator {
    buckets: std::collections::HashMap<(i64, u32, NetDirection, NetProto), u64>,
}

pub const METRIC_MINUTE_MS: i64 = 60_000;

/// 指标 id（`<domain>.<subject>.<measure>` 三段）。
pub const METRIC_NET_PROCESS_BYTES_IN: &str = "net.process.bytes_in";
pub const METRIC_NET_PROCESS_BYTES_OUT: &str = "net.process.bytes_out";

impl MinuteAggregator {
    pub fn add(&mut self, r: &NetFlowRecord) {
        let bucket = r.timestamp_utc_ms.div_euclid(METRIC_MINUTE_MS);
        *self
            .buckets
            .entry((bucket, r.pid, r.direction, r.proto))
            .or_insert(0) += r.bytes;
    }

    /// 桶快照（测试断言用；key 同 add）。
    pub fn snapshot(&self) -> std::collections::BTreeMap<(i64, u32, NetDirection, NetProto), u64> {
        self.buckets.iter().map(|(k, v)| (*k, *v)).collect()
    }

    /// 出站 MetricPoint（每桶一条）。口径：
    /// - timestamp = 桶起点（bucket*60_000）、interval_ms = 60_000；
    /// - source=Etw、unit=Bytes、quality=Confirmed（seen>0 且无丢失）；
    ///   lost>0 时全批降 Incomplete 且 loss_count/coverage 显式填充
    ///   （coverage = seen/(seen+lost)，丢失占比对每桶同权——损失数来自
    ///   ETW 回调/会话统计，桶粒度损失数 ETW 不提供）；
    /// - process_identity 携 pid（exe_start_epoch=0=未知，M2.3 由 process
    ///   监控补齐防 PID 复用误绑）。
    ///
    /// `host_id`/`now_ms` 由调用方注入（禁真实时钟）。
    pub fn drain_metric_points(
        &mut self,
        host_id: &str,
        events_lost: u64,
        events_seen: u64,
        _now_ms: i64,
    ) -> Vec<crate::metrics::MetricPoint> {
        use crate::metrics::{MetricPoint, MetricQuality, MetricSource, MetricUnit};
        let quality = if events_lost > 0 {
            MetricQuality::Incomplete
        } else {
            MetricQuality::Confirmed
        };
        let coverage = if events_lost > 0 {
            let total = (events_seen + events_lost) as f64;
            if total > 0.0 {
                Some(events_seen as f64 / total)
            } else {
                None
            }
        } else {
            None
        };
        let mut keys: Vec<_> = self.buckets.keys().copied().collect();
        keys.sort();
        let mut out = Vec::with_capacity(keys.len());
        for (bucket, pid, direction, proto) in keys {
            let bytes = self
                .buckets
                .remove(&(bucket, pid, direction, proto))
                .unwrap_or(0);
            let metric_id = match direction {
                NetDirection::In => METRIC_NET_PROCESS_BYTES_IN,
                NetDirection::Out => METRIC_NET_PROCESS_BYTES_OUT,
            };
            out.push(MetricPoint {
                metric_id: metric_id.to_string(),
                source: MetricSource::Etw,
                host_id: host_id.to_string(),
                process_identity: crate::metrics::ProcessIdentity::Id {
                    pid,
                    exe_start_epoch: 0, // 未知；M2.3 补齐
                },
                timestamp_utc_ms: bucket * METRIC_MINUTE_MS,
                monotonic_ms: 0, // 桶时间语义下无单调时钟意义，恒 0（注释口径）
                interval_ms: METRIC_MINUTE_MS as u64,
                value: bytes as f64,
                unit: MetricUnit::Bytes,
                scope: crate::metrics::MetricScope::Connection {
                    proto: match proto {
                        NetProto::Tcp => "tcp".to_string(),
                        NetProto::Udp => "udp".to_string(),
                    },
                    remote: String::new(), // 桶 key 不含远端（对端维度 M2.3 再分）
                },
                quality,
                loss_count: if events_lost > 0 {
                    Some(events_lost)
                } else {
                    None
                },
                coverage,
                schema_version: crate::metrics::METRIC_SCHEMA_VERSION,
            });
        }
        out
    }
}

// ─── M2.2：内核网络事件解析与分钟聚合（MetricPoint 产出） ────────────────────
// schema 权威来源：本机 TDH 清单（Get-WinEvent -ListProvider
// 'Microsoft-Windows-Kernel-Network' 模板原文，2026-11-01 提取）——
// 经典 V0 布局，数据事件（字节量口径）：
//   10 TCPv4 送出 / 11 TCPv4 收到：PID u32, size u32, daddr u32, saddr u32,
//      dport u16(BE), sport u16(BE), [startime u32, endtime u32,] seqnum u32,
//      connid u32   （无 time 形 v4=28B / v6=52B；带 time 形 v4=36B / v6=60B）
//   26 TCPv6 送出 / 27 TCPv6 收到：同上但 addr 各 16B
//   42 UDPv4 送出 / 43 UDPv4 收到：同 11 形
//   58 UDPv6 送出 / 59 UDPv6 收到：同 27 形
// 方向按事件语义：transmitted=Out、received=In（清单 Description 原文）。
#[cfg(test)]
mod m22_tests {
    use super::*;
    use crate::metrics::{MetricQuality, MetricUnit, ProcessIdentity};

    /// 合成 IPv4 数据载荷（data 事件，with_time=true 时 36B，否则 32B）
    fn v4_payload(
        pid: u32,
        size: u32,
        daddr: u32,
        saddr: u32,
        dport: u16,
        sport: u16,
        with_time: bool,
    ) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&pid.to_le_bytes());
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&daddr.to_le_bytes());
        v.extend_from_slice(&saddr.to_le_bytes());
        v.extend_from_slice(&dport.to_be_bytes());
        v.extend_from_slice(&sport.to_be_bytes());
        if with_time {
            v.extend_from_slice(&0u32.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes());
        }
        v.extend_from_slice(&0u32.to_le_bytes()); // seqnum
        v.extend_from_slice(&0u32.to_le_bytes()); // connid
        v
    }

    /// 合成 IPv6 数据载荷（data 事件，with_time=true 时 60B，否则 52B）
    fn v6_payload(
        pid: u32,
        size: u32,
        daddr: [u8; 16],
        saddr: [u8; 16],
        dport: u16,
        sport: u16,
        with_time: bool,
    ) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&pid.to_le_bytes());
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&daddr);
        v.extend_from_slice(&saddr);
        v.extend_from_slice(&dport.to_be_bytes());
        v.extend_from_slice(&sport.to_be_bytes());
        if with_time {
            v.extend_from_slice(&0u32.to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes());
        }
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v
    }

    #[test]
    fn m22_parse_tcpv4_send_synthetic_bytes() {
        let payload = v4_payload(
            4242,
            1500,
            0x0102_a8c0,
            0x0100_007f_u32,
            443,
            51000,
            true,
        );
        let r = parse_kernel_network_event(10, &payload, 1_749_000_000_000)
            .expect("ID=10 TCPv4 送出必须可解析");
        assert_eq!(r.pid, 4242);
        assert_eq!(r.bytes, 1500);
        assert!(
            matches!(r.direction, NetDirection::Out),
            "ID=10 transmitted=送出"
        );
        assert!(matches!(r.proto, NetProto::Tcp));
        assert_eq!(r.ip_version, 4);
        assert_eq!(r.remote_port, 443, "win:Port 线上大端必须按 BE 解码");
        assert_eq!(
            r.remote,
            0x0102_a8c0u32.to_le_bytes().to_vec(),
            "daddr=远端（送出方向）"
        );
        assert_eq!(r.timestamp_utc_ms, 1_749_000_000_000);
    }

    #[test]
    fn m22_parse_tcpv4_recv_synthetic_bytes() {
        let payload = v4_payload(7, 2048, 0x0102_a8c0, 0x0b01_a8c0, 51000, 443, false);
        let r = parse_kernel_network_event(11, &payload, 1).expect("ID=11 必须可解析");
        assert_eq!(r.bytes, 2048);
        assert!(
            matches!(r.direction, NetDirection::In),
            "ID=11 received=收到"
        );
        assert_eq!(
            r.remote_port, 443,
            "收到事件远端端口=sport（from saddr:sport）"
        );
        // ID=11 语义「received from saddr:sport」→ 远端 = saddr
        assert_eq!(r.remote, 0x0b01_a8c0u32.to_le_bytes().to_vec());
        assert_eq!(payload.len(), 28, "TCPv4 收到模板定长 28B（4×6+2+2）");
    }

    #[test]
    fn m22_parse_tcpv6_synthetic_bytes() {
        let mut d = [0u8; 16];
        d[15] = 1;
        let mut s = [0u8; 16];
        s[15] = 2;
        let payload = v6_payload(9, 300, d, s, 8080, 60000, true);
        let r = parse_kernel_network_event(26, &payload, 1).expect("ID=26 TCPv6 送出必须可解析");
        assert_eq!(r.ip_version, 6);
        assert_eq!(r.bytes, 300);
        assert!(matches!(r.direction, NetDirection::Out));
        assert_eq!(r.remote, d.to_vec());
        assert_eq!(r.remote_port, 8080);
        // 无 startime 形（52B）
        let r2 = parse_kernel_network_event(27, &v6_payload(9, 400, d, s, 8080, 60000, false), 1)
            .expect("ID=27 TCPv6 收到必须可解析");
        assert!(matches!(r2.direction, NetDirection::In));
        assert_eq!(r2.bytes, 400);
    }

    #[test]
    fn m22_parse_udp_events() {
        let r42 = parse_kernel_network_event(42, &v4_payload(1, 100, 1, 2, 53, 40000, false), 1)
            .expect("ID=42 UDPv4 送出必须可解析");
        assert!(matches!(r42.proto, NetProto::Udp));
        assert!(matches!(r42.direction, NetDirection::Out));
        let r43 = parse_kernel_network_event(43, &v4_payload(1, 200, 1, 2, 40000, 53, false), 1)
            .expect("ID=43 UDPv4 收到必须可解析");
        assert!(matches!(r43.proto, NetProto::Udp));
        assert!(matches!(r43.direction, NetDirection::In));
        assert_eq!(r43.bytes, 200);
        let r58 = parse_kernel_network_event(
            58,
            &v6_payload(1, 300, [0; 16], [1; 16], 53, 40000, false),
            1,
        )
        .expect("ID=58 UDPv6 送出必须可解析");
        assert_eq!(r58.ip_version, 6);
        let r59 = parse_kernel_network_event(
            59,
            &v6_payload(1, 400, [0; 16], [1; 16], 40000, 53, false),
            1,
        )
        .expect("ID=59 UDPv6 收到必须可解析");
        assert_eq!(r59.bytes, 400);
    }

    #[test]
    fn m22_parse_unknown_or_truncated_is_none_not_fabricated() {
        // 未知事件 id（连接类/非数据事件）不产字节记录
        assert!(parse_kernel_network_event(12, &v4_payload(1, 1, 1, 2, 1, 2, false), 1).is_none());
        assert!(parse_kernel_network_event(99, &[0u8; 64], 1).is_none());
        // 截断载荷：不得越界、不得伪造
        let short = v4_payload(1, 100, 1, 2, 1, 2, false);
        assert!(parse_kernel_network_event(10, &short[..20], 1).is_none());
        assert!(parse_kernel_network_event(11, &short[..10], 1).is_none());
    }

    /// 造流：乱序注入跨两个分钟桶的若干记录
    fn synthetic_stream() -> Vec<NetFlowRecord> {
        let mk = |bucket_minute: i64, pid: u32, dir: NetDirection, proto: NetProto, bytes: u64| {
            NetFlowRecord {
                timestamp_utc_ms: bucket_minute * 60_000 + 12_345,
                pid,
                direction: dir,
                proto,
                ip_version: 4,
                remote: vec![1, 2, 3, 4],
                remote_port: 443,
                bytes,
            }
        };
        // 分钟 0：pid7 in tcp 3 笔 100/200/300 + pid8 out udp 50
        // 分钟 1：pid7 in tcp 999
        vec![
            mk(1, 7, NetDirection::In, NetProto::Tcp, 999),
            mk(0, 7, NetDirection::In, NetProto::Tcp, 200), // 乱序到达
            mk(0, 8, NetDirection::Out, NetProto::Udp, 50),
            mk(0, 7, NetDirection::In, NetProto::Tcp, 100),
            mk(0, 7, NetDirection::In, NetProto::Tcp, 300),
        ]
    }

    #[test]
    fn m22_minute_aggregation_is_exact_and_order_immune() {
        let mut agg = MinuteAggregator::default();
        let stream = synthetic_stream();
        for r in &stream {
            agg.add(r);
        }
        // 乱序对照：逆序注入必须得到同一聚合
        let mut agg_rev = MinuteAggregator::default();
        for r in stream.iter().rev() {
            agg_rev.add(r);
        }
        assert_eq!(
            agg.snapshot(),
            agg_rev.snapshot(),
            "乱序容忍：桶聚合与到达顺序无关"
        );

        let snap = agg.snapshot();
        let k0 = &(0i64, 7u32, NetDirection::In, NetProto::Tcp);
        let k0b = &(0i64, 8u32, NetDirection::Out, NetProto::Udp);
        let k1 = &(1i64, 7u32, NetDirection::In, NetProto::Tcp);
        assert_eq!(snap.get(k0), Some(&600u64), "分钟 0 pid7 收到 100+200+300");
        assert_eq!(snap.get(k0b), Some(&50u64));
        assert_eq!(snap.get(k1), Some(&999u64));
        // 无跨分钟漂移：同 key 不同分钟严格分桶
        assert_eq!(snap.len(), 3);
    }

    #[test]
    fn m22_metric_points_from_buckets_shape_and_values() {
        let mut agg = MinuteAggregator::default();
        for r in &synthetic_stream() {
            agg.add(r);
        }
        // 无丢失：Confirmed、loss/coverage 缺省
        let pts = agg.drain_metric_points("HOST-1", 0, 5, 1_749_000_000_000);
        assert_eq!(pts.len(), 3);
        for p in &pts {
            assert!(
                matches!(p.source, crate::metrics::MetricSource::Etw),
                "source=Etw"
            );
            assert_eq!(p.unit, MetricUnit::Bytes);
            assert!(
                matches!(p.quality, MetricQuality::Confirmed),
                "无丢失=实测确认"
            );
            assert_eq!(p.loss_count, None);
            assert_eq!(p.coverage, None);
            assert_eq!(p.interval_ms, 60_000, "口径=分钟桶");
            assert_eq!(p.host_id, "HOST-1");
            assert_eq!(p.schema_version, crate::metrics::METRIC_SCHEMA_VERSION);
            assert!(
                p.metric_id.starts_with("net.process.bytes_"),
                "三段命名: {}",
                p.metric_id
            );
            match &p.process_identity {
                ProcessIdentity::Id { pid, .. } => assert!([7u32, 8u32].contains(pid)),
                other => panic!("进程粒度指标必须带 pid，实际 {other:?}"),
            }
        }
        // 桶起点时间戳 + 值精确
        let p0 = pts
            .iter()
            .find(|p| p.timestamp_utc_ms == 0 && p.metric_id == "net.process.bytes_in")
            .expect("分钟 0 in 桶必须有 MetricPoint");
        assert_eq!(p0.value, 600.0);
        let p1 = pts
            .iter()
            .find(|p| p.timestamp_utc_ms == 60_000)
            .expect("分钟 1 桶必须有 MetricPoint");
        assert_eq!(p1.value, 999.0, "无跨分钟漂移：999 全部留在分钟 1");
    }

    #[test]
    fn m22_loss_scenario_sets_incomplete_and_counts() {
        let mut agg = MinuteAggregator::default();
        for r in &synthetic_stream() {
            agg.add(r);
        }
        // 丢 5 见 5：quality=Incomplete、loss_count=Some(5)、coverage=0.5
        let pts = agg.drain_metric_points("HOST-1", 5, 5, 1_749_000_000_000);
        assert!(!pts.is_empty());
        for p in &pts {
            assert!(
                matches!(p.quality, MetricQuality::Incomplete),
                "有丢失时质量必须降为 Incomplete（估算/缺失不冒充实测）"
            );
            assert_eq!(p.loss_count, Some(5));
            let cov = p.coverage.expect("丢失场景必须给 coverage");
            assert!(
                (cov - 0.5).abs() < 1e-9,
                "coverage=seen/(seen+lost)=0.5，实际 {cov}"
            );
        }
    }

    #[test]
    fn m22_drain_clears_buckets_no_double_count() {
        let mut agg = MinuteAggregator::default();
        for r in &synthetic_stream() {
            agg.add(r);
        }
        let first = agg.drain_metric_points("H", 0, 5, 0);
        let second = agg.drain_metric_points("H", 0, 5, 0);
        assert!(!first.is_empty());
        assert!(second.is_empty(), "drain 后桶必须清空（不重复出站）");
    }
}

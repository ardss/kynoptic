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
//! 信封 [`NetEtwRecord`] 沿用 M1 [`crate::metrics::MetricQuality`] 质量口径；
//! 内核网络事件负载逐字节解码（PID/字节量细分）属 M2.2，本层只保证
//! 会话与批量出站管道正确。

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

/// 批量出站记录信封（M2.2 补负载逐字节解码；本层时间/PID 由事件头直取）。
#[derive(Debug, Clone, PartialEq)]
pub struct NetEtwRecord {
    /// UTC epoch 毫秒（FILETIME 100ns 换算）。
    pub timestamp_utc_ms: i64,
    pub pid: u32,
    /// M2.2 解码；本层恒 0（契约层不伪造字节量——「未采到不是零」）。
    pub bytes_sent: u64,
    pub bytes_recv: u64,
}

/// 批量出站回调（专职消费线程持有，整批投递）。
pub type EventSink = Arc<dyn Fn(Vec<NetEtwRecord>) + Send + Sync>;

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
pub fn chunk_records(records: Vec<NetEtwRecord>, max: usize) -> Vec<Vec<NetEtwRecord>> {
    if records.is_empty() {
        return Vec::new();
    }
    records
        .chunks(max.max(1))
        .map(<[NetEtwRecord]>::to_vec)
        .collect()
}

// ─── 真实后端（windows-sys 直调） ──────────────────────────────────────────

/// 真实 ETW 后端。
pub struct RealEtw {
    /// 批量出站线程（recv_timeout 窗口）；close 置停机旗标后 join。
    batcher_thread: Option<std::thread::JoinHandle<()>>,
    /// 事件通道发送端（回调经 UserContext 直取；close 时丢弃触发停机）。
    tx: Option<Arc<crossbeam_channel::Sender<NetEtwRecord>>>,
    /// 批量出站线程停机旗标。
    stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl RealEtw {
    pub fn new() -> Self {
        Self {
            batcher_thread: None,
            tx: None,
            stop_flag: None,
        }
    }
}

impl Default for RealEtw {
    fn default() -> Self {
        Self::new()
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

    /// 回调用户上下文：start_consumer 经 logfile.Context 传入的通道 Arc 指针
    /// （EVENT_RECORD.UserContext 原样回传；会话级一次性 leaked，非事件级）。
    type RecordTx = Arc<crossbeam_channel::Sender<NetEtwRecord>>;

    unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
        let ctx = (*record).UserContext as *const RecordTx;
        if ctx.is_null() {
            return;
        }
        let tx = &*ctx;
        let r = &*record;
        let header = &r.EventHeader;
        // FILETIME(100ns since 1601) → UTC epoch 毫秒
        let ft = header.TimeStamp;
        let ts_ms = ft / 10_000 - 11_644_473_600_000;
        let pid = header.ProcessId;
        let _ = tx.send(NetEtwRecord {
            timestamp_utc_ms: ts_ms,
            pid,
            bytes_sent: 0, // M2.2 解码内核网络事件负载，本层不伪造字节量
            bytes_recv: 0,
        });
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
                    // 会话已不存在（对端刚退出）即达成目的
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
            let (tx, rx) = crossbeam_channel::unbounded::<NetEtwRecord>();
            let tx = Arc::new(tx);
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            // 专职批量出站线程：回调推通道，线程按 64 条/100ms 窗口整批投递；
            // 停机旗标置位且通道排空后退出（CloseTrace 后最多 ~100ms 延迟）
            let stop_b = stop.clone();
            let sink_b = sink.clone();
            let batcher = std::thread::spawn(move || loop {
                let mut batch: Vec<NetEtwRecord> = Vec::new();
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
            let ctx = Arc::into_raw(tx) as *mut crossbeam_channel::Sender<NetEtwRecord>;
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
        records: Vec<NetEtwRecord>,
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
            c.records.push(NetEtwRecord {
                timestamp_utc_ms: 1_749_000_000_000,
                pid: 100,
                bytes_sent: 10,
                bytes_recv: 20,
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
        let rec = |i: u64| NetEtwRecord {
            timestamp_utc_ms: i as i64,
            pid: i as u32,
            bytes_sent: i,
            bytes_recv: i,
        };
        let records: Vec<NetEtwRecord> = (0..70).map(rec).collect();
        let batches = chunk_records(records, ETW_SINK_BATCH_MAX);
        assert_eq!(batches.len(), 2, "70 条按 64 上限应分 2 批");
        assert_eq!(batches[0].len(), 64);
        assert_eq!(batches[1].len(), 6);
        assert_eq!(batches[0][0].timestamp_utc_ms, 0, "分批必须保序");
        assert_eq!(batches[1][5].timestamp_utc_ms, 69, "分批必须保序");

        // 边界：恰一批 / 空输入
        let one: Vec<NetEtwRecord> = (0..64).map(rec).collect();
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
        let hits = Arc::new(Mutex::new(Vec::<NetEtwRecord>::new()));
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

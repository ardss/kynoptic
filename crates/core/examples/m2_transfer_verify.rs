//! M2.5 受控传输验证（100MiB 直连）——验证脚本，不进生产代码。
//!
//! 用法（需管理员：ETW 会话启动权限）：
//!   m2_transfer_verify.exe --port 18422 --mib 100 --out-json evidence.json
//! 流程：本进程内起临时 HTTP 服务线程（隔离实例，端口独立于生产 8422），
//! 主线程以裸 TCP HTTP GET 下载 100MiB 受控载荷；全程 RealEtw 会话在跑，
//! sink 只累计本进程 pid 的传输层字节（方向 Out=服务端发出、In=客户端收）。
//! 下载完成 + 3s 收尾后停会话，比对 ETW 字节 vs 载荷字节，写 JSON 证据。

use kynoptic_core::monitors::net_etw::{EtwBackend, NetDirection, RealEtw};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// 参数矩阵探针（--probe，仅本验证脚本使用）：对 start/enable 组合逐一
/// 试错，找出本机可用的会话属性组合；只打印，不改任何全局状态（每个
/// 变体即起即停）。
#[cfg(windows)]
fn probe_matrix() {
    use windows_sys::Win32::System::Diagnostics::Etw::{
        ControlTraceW, EnableTrace, EnableTraceEx, EnableTraceEx2, StartTraceW,
        CONTROLTRACE_HANDLE, EVENT_TRACE_CONTROL_QUERY, EVENT_TRACE_CONTROL_STOP,
        EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, EVENT_TRACE_SYSTEM_LOGGER_MODE,
    };
    const TRACED: u32 = 0x0002_0000;
    let (d1, d2, d3, d4) =
        kynoptic_core::monitors::net_etw::KERNEL_NETWORK_PROVIDER_GUID_COMPONENTS;
    let guid = windows_sys::core::GUID {
        data1: d1,
        data2: d2,
        data3: d3,
        data4: d4,
    };
    for sys in [false, true] {
        for api in 0..3u8 {
            for kw in [u64::MAX, 0xFFFF_FFFF] {
                for lvl in [4u8, 5u8] {
                    let flags = if sys {
                        TRACED | EVENT_TRACE_SYSTEM_LOGGER_MODE
                    } else {
                        TRACED
                    };
                    let name = format!("KynopticProbe{:05}", std::process::id());
                    let mut wide: Vec<u16> = name.encode_utf16().collect();
                    wide.push(0);
                    let mut buf =
                        vec![0u8; std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + wide.len() * 2];
                    let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
                    let size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
                    unsafe {
                        (*props).Wnode.BufferSize = buf.len() as u32;
                        (*props).Wnode.Flags = flags;
                        (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
                        (*props).LoggerNameOffset = size as u32;
                        std::ptr::copy_nonoverlapping(
                            wide.as_ptr(),
                            buf.as_mut_ptr().add(size) as *mut u16,
                            wide.len(),
                        );
                        let mut h = CONTROLTRACE_HANDLE { Value: 0 };
                        let r1 = StartTraceW(&mut h, wide.as_ptr(), props);
                        if r1 != 0 {
                            println!("sys={sys} api={api} kw={kw:#x} lvl={lvl} -> start err={r1}");
                            continue;
                        }
                        // 会话确实存在？（QUERY 按名核验）
                        let rq = ControlTraceW(
                            CONTROLTRACE_HANDLE { Value: 0 },
                            wide.as_ptr(),
                            props,
                            EVENT_TRACE_CONTROL_QUERY,
                        );
                        let params: windows_sys::Win32::System::Diagnostics::Etw::ENABLE_TRACE_PARAMETERS =
                            std::mem::zeroed();
                        let r2 = match api {
                            0 => EnableTraceEx2(
                                h, &guid, 1, // EVENT_CONTROL_CODE_ENABLE_PROVIDER
                                lvl, kw, 0, 0, &params,
                            ),
                            1 => EnableTraceEx(
                                &guid,
                                std::ptr::null(),
                                h,
                                1,
                                lvl,
                                kw,
                                0,
                                0,
                                std::ptr::null(),
                            ),
                            _ => EnableTrace(1, kw as u32, lvl as u32, &guid, h),
                        };
                        println!(
                            "sys={sys} api={api} kw={kw:#x} lvl={lvl} -> start=ok query={rq} enable={r2}"
                        );
                        let _ = ControlTraceW(
                            CONTROLTRACE_HANDLE { Value: 0 },
                            wide.as_ptr(),
                            props,
                            EVENT_TRACE_CONTROL_STOP,
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }
            }
        }
    }
}

/// 误差百分比（相对期望值）：|measured - expected| / expected * 100
pub fn error_pct(expected: u64, measured: u64) -> f64 {
    if expected == 0 {
        return f64::INFINITY;
    }
    let d = (measured as f64 - expected as f64).abs();
    d / expected as f64 * 100.0
}

/// 判定：≤5% PASS，>5% FAIL（需归因并标 Estimated）
pub fn verdict(error: f64) -> &'static str {
    if error <= 5.0 {
        "PASS"
    } else {
        "FAIL"
    }
}

fn main() {
    let mut port: u16 = 18422;
    let mut mib: u64 = 100;
    let mut out_json = String::from("m2-evidence.json");
    // 默认旁路专用会话名：绝不碰生产 KynopticNetMon 会话
    let mut session_name = String::from("KynopticNetMonVerify");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--probe" => {
                #[cfg(windows)]
                probe_matrix();
                return;
            }
            "--port" => port = args.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--mib" => mib = args.next().and_then(|v| v.parse().ok()).unwrap_or(mib),
            "--out-json" => out_json = args.next().unwrap_or(out_json),
            "--session-name" => session_name = args.next().unwrap_or(session_name),
            other => eprintln!("未知参数 {other}"),
        }
    }
    let payload_len = mib * 1024 * 1024;
    let my_pid = std::process::id();
    println!("M2.5 受控传输验证: {mib}MiB @ 127.0.0.1:{port}, 本进程 pid={my_pid}");

    // ETW 会话（需管理员）；sink 累计本进程 pid 的 Out/In 字节
    let out_bytes = Arc::new(AtomicU64::new(0));
    let in_bytes = Arc::new(AtomicU64::new(0));
    let event_count = Arc::new(AtomicU64::new(0));
    let mut etw = RealEtw::with_session_name(&session_name);
    let ob = out_bytes.clone();
    let ib = in_bytes.clone();
    let ec = event_count.clone();
    let sink = Arc::new(
        move |batch: Vec<kynoptic_core::monitors::net_etw::NetFlowRecord>| {
            ec.fetch_add(batch.len() as u64, Ordering::Relaxed);
            for r in batch {
                if r.pid != my_pid {
                    continue;
                }
                match r.direction {
                    NetDirection::Out => {
                        ob.fetch_add(r.bytes, Ordering::Relaxed);
                    }
                    NetDirection::In => {
                        ib.fetch_add(r.bytes, Ordering::Relaxed);
                    }
                }
            }
        },
    );
    // 直接走 EtwBackend 会话序列（等效 NetEtwMonitor.start 的注入路径）
    let session = match etw.start_session() {
        Ok(h) => h,
        Err(kynoptic_core::monitors::net_etw::EtwError::AccessDenied) => {
            eprintln!("ETW 启动被拒：需要管理员权限（AccessDenied）");
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("ETW 启动失败: {e:?}");
            std::process::exit(2);
        }
    };
    if let Err(e) = etw.enable_provider(session) {
        eprintln!("供应商启用失败: {e:?}");
        std::process::exit(2);
    }
    let consumer = match etw.start_consumer(session, sink) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("消费端启动失败: {e:?}");
            std::process::exit(2);
        }
    };
    println!("ETW 会话已启动（session={session}, consumer={consumer}）");

    // 临时 HTTP 服务线程：GET /payload → 100MiB 确定性字节（0x5A）
    let payload = Arc::new(vec![0x5au8; payload_len as usize]);
    let payload_srv = payload.clone();
    let server = std::thread::spawn(move || {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind");
        let (stream, _) = listener.accept().expect("accept");
        drop(listener);
        let mut stream = stream;
        // 读请求头（直到 \r\n\r\n）
        let mut buf = [0u8; 4096];
        let mut got = 0usize;
        loop {
            let n = std::io::Read::read(&mut stream, &mut buf[got..]).expect("read req");
            got += n;
            if n == 0 || buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let hdr = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload_srv.len()
        );
        use std::io::Write;
        stream.write_all(hdr.as_bytes()).unwrap();
        stream.write_all(&payload_srv).unwrap();
        stream.flush().unwrap();
    });

    // 下载（本进程 = 被观测进程）
    let t0 = std::time::Instant::now();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let req =
        format!("GET /payload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    use std::io::{Read, Write};
    stream.write_all(req.as_bytes()).unwrap();
    let mut header_done = false;
    let mut header: Vec<u8> = Vec::new();
    let mut body_read: u64 = 0;
    let mut buf = [0u8; 65536];
    loop {
        let n = stream.read(&mut buf).expect("read body");
        if n == 0 {
            break;
        }
        if !header_done {
            header.extend_from_slice(&buf[..n]);
            if let Some(pos) = header.windows(4).position(|w| w == b"\r\n\r\n") {
                header_done = true;
                body_read += (header.len() - pos - 4) as u64;
            }
        } else {
            body_read += n as u64;
        }
    }
    server.join().unwrap();
    let transfer_secs = t0.elapsed().as_secs_f64();
    println!("下载完成: 应用层 body={body_read} 字节, {transfer_secs:.2}s");

    // 收尾窗口：让批量出站线程与内核事件排空
    std::thread::sleep(std::time::Duration::from_secs(3));
    etw.close_consumer(consumer);
    etw.stop_session().expect("ETW 停止");
    let (seen, lost) = etw.loss_stats();
    let measured_out = out_bytes.load(Ordering::Relaxed);
    let measured_in = in_bytes.load(Ordering::Relaxed);
    let events = event_count.load(Ordering::Relaxed);
    println!("ETW: pid={my_pid} out={measured_out} in={measured_in} events={events} seen={seen} lost={lost}");

    let err_out = error_pct(payload_len, measured_out);
    let err_in = error_pct(payload_len, measured_in);
    let result = serde_json::json!({
        "payload_bytes": payload_len,
        "app_body_bytes": body_read,
        "etw_out_bytes": measured_out,
        "etw_in_bytes": measured_in,
        "error_pct_out": err_out,
        "error_pct_in": err_in,
        "verdict_out": verdict(err_out),
        "verdict_in": verdict(err_in),
        "events_seen": seen,
        "events_lost": lost,
        "transfer_secs": transfer_secs,
        "pid": my_pid,
        "port": port,
    });
    std::fs::write(&out_json, serde_json::to_string_pretty(&result).unwrap()).unwrap();
    println!("证据 JSON 已写: {out_json}");
    println!(
        "verdict: out={} ({err_out:.2}%) in={} ({err_in:.2}%)",
        verdict(err_out),
        verdict(err_in)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_pct_math_and_verdict_thresholds() {
        assert_eq!(error_pct(100, 100), 0.0);
        assert!((error_pct(100, 103) - 3.0).abs() < 1e-9);
        assert!((error_pct(100, 94) - 6.0).abs() < 1e-9);
        assert_eq!(verdict(5.0), "PASS", "5% 边界含于通过域");
        assert_eq!(verdict(5.01), "FAIL");
        assert_eq!(verdict(0.0), "PASS");
    }
}

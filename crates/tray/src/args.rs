//! 托盘壳 CLI 参数:--db PATH / --port N / --all
//!
//! 纯逻辑,与 Win32 无关,单测覆盖。

use std::path::PathBuf;

use kynoptic_core::Error;

/// dashboard 默认端口(与 `kynoptic-ctl dashboard` 一致)
pub const DEFAULT_PORT: u16 = 8422;

const USAGE: &str = "usage: kynoptic-tray [--db PATH] [--port N] [--all] [--flag PATH]\n  --db PATH    database path (default: kynoptic_core::db::resolve_db_path)\n  --port N     dashboard TCP port on 127.0.0.1 (default 8422)\n  --all        enable the full monitor set instead of the default 14\n  --flag PATH  override tray-exit.flag path (default: exe dir; also KYNOPTIC_EXIT_FLAG env)\n";

/// 托盘壳启动配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub db: PathBuf,
    pub port: u16,
    /// 透传采集器:启用全集监控器(registry::all_monitor_ids)
    pub all: bool,
    /// "用户主动退出"旗标路径覆盖(测试用;缺省走 exe 同目录,见 paths.rs)
    pub exit_flag: Option<PathBuf>,
}

/// 每段连续候选个数
pub const CANDIDATES_PER_BLOCK: u16 = 11;

/// 端口回退候选序列（P1 实测修复：Hyper-V/WSL 的 excludedportrange 常覆盖
/// 8408-8507，旧的 8422 起逐个 +1 的 11 个候选会整段落进保留区，bind 全部
/// 报 os error 10013，面板必死）。改为三段、段间 +10000 大步长跳出保留区：
/// `8422-8432 → 18422-18432 → 28422-28432`。纯函数，单测覆盖。
/// 段内回环撞车（另一个服务占 8422）比段内 +1 撞上保留区可接受得多。
pub fn candidate_ports(preferred: u16) -> Vec<u16> {
    let mut out = Vec::new();
    for block in 0..3u32 {
        let base = match (preferred as u32).checked_add(block * 10_000) {
            Some(b) if b <= u16::MAX as u32 => b,
            _ => break,
        };
        for i in 0..CANDIDATES_PER_BLOCK {
            match base.checked_add(i as u32) {
                Some(p) if p <= u16::MAX as u32 => out.push(p as u16),
                _ => return out,
            }
        }
    }
    out
}

/// 解析托盘壳参数。db 缺省走 core 统一解析;未知选项报错。
pub fn parse(args: &[String]) -> Result<Args, Error> {
    let mut port = DEFAULT_PORT;
    let mut db: Option<PathBuf> = None;
    let mut all = false;
    let mut exit_flag: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                if raw.is_empty() {
                    return Err(Error::InvalidData("--db 需要路径".into()));
                }
                db = Some(PathBuf::from(raw));
            }
            "--port" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                port = raw
                    .parse::<u16>()
                    .map_err(|_| Error::InvalidData(format!("端口非法: {raw}(0-65535)")))?;
            }
            "--all" => all = true,
            // 退出旗标路径覆盖(与 watchdog 契约测试用;缺省 exe 同目录)
            "--flag" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                if raw.is_empty() {
                    return Err(Error::InvalidData("--flag 需要路径".into()));
                }
                exit_flag = Some(PathBuf::from(raw));
            }
            // 开机自启动/看门狗拉起时带的参数:托盘本来就无可视窗口,
            // 此处接受并忽略(历史上未实现,导致自启动启动即报错退出)。
            "--minimized" => {}
            other => return Err(Error::InvalidData(format!("未知选项: {other}\n\n{USAGE}"))),
        }
        i += 1;
    }
    Ok(Args {
        db: db.unwrap_or_else(kynoptic_core::db::resolve_db_path),
        port,
        all,
        exit_flag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 挑第一个能 bind 的候选（探测即正式语义的测试替身;生产路径在
    /// main.rs 的 dashboard 线程内紧贴 serve 执行同样的探测 bind）。
    fn pick_free_port(preferred: u16) -> Option<u16> {
        candidate_ports(preferred)
            .into_iter()
            .find(|&p| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok())
    }

    fn sv(args: &[&str]) -> Result<Args, Error> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn port_fallback_candidates_cross_big_stride_blocks() {
        // P1 回归：Hyper-V/WSL excludedportrange 常覆盖 8408-8507，旧实现
        // 8422-8432 连续 11 个候选整段落在保留区。新序列必须跨 +10000 大步长。
        let v = candidate_ports(8422);
        assert_eq!(v.len(), 33);
        assert_eq!(
            &v[..11],
            &(8422..=8432).collect::<Vec<u16>>(),
            "第一段 8422-8432"
        );
        assert_eq!(
            &v[11..22],
            &(18422..=18432).collect::<Vec<u16>>(),
            "第二段跨大步长"
        );
        assert_eq!(
            &v[22..33],
            &(28422..=28432).collect::<Vec<u16>>(),
            "第三段再跨大步长"
        );
    }

    #[test]
    fn port_fallback_candidates_generic_shape() {
        let v = candidate_ports(9000);
        assert_eq!(
            v,
            (9000..=9010)
                .chain(19000..=19010)
                .chain(29000..=29010)
                .collect::<Vec<u16>>()
        );
    }

    #[test]
    fn port_fallback_never_wraps_on_overflow() {
        // preferred 贴近 u16::MAX 时不得回绕到 0（回环低端口不允许偷偷占）
        let v = candidate_ports(u16::MAX);
        assert_eq!(v, vec![u16::MAX]);
        // 高位 preferred：第二段 75000 超出 u16 即截断，不回绕低端口
        let v = candidate_ports(65_000);
        assert_eq!(v, (65_000..=65_010).collect::<Vec<u16>>());
    }

    #[test]
    fn pick_free_port_skips_occupied_and_reports_actual() {
        // 用临时 bind(0) 拿一个确定空闲的端口做 preferred,自己占住首位候选,
        // pick 必须跳过它并返回候选集内的其他端口。（不写死 8422 段:测试机
        // 的 excludedportrange 可能把候选段整个保留,bind 直接 10013。）
        let preferred = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let preferred_port = preferred.local_addr().unwrap().port();
        let picked = pick_free_port(preferred_port).unwrap();
        assert_ne!(picked, preferred_port, "被占端口必须跳过");
        assert!(candidate_ports(preferred_port).contains(&picked));
        drop(preferred);
    }

    #[test]
    fn pick_free_port_retries_across_stride_when_block_occupied() {
        // 模拟保留区/占用：把第一段候选全部占住或不可 bind（excluded 范围
        // bind 会报 10013——同样是"不可用"），pick 必须跨 +10000 大步长落到
        // 第二段（旧实现 11 连号全失败返回 None → 面板静默死亡的路径）。
        // 选一个满足 base+10010 <= u16::MAX 的空闲 preferred（跨段断言需要
        // 第二段存在;临时端口 49152-65535 里高段不够跨步）
        let (base, _keeper) = loop {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let p = l.local_addr().unwrap().port();
            if p as u32 + 10_000 + u32::from(CANDIDATES_PER_BLOCK) - 1 <= u32::from(u16::MAX) {
                break (p, l);
            }
        };
        drop(_keeper);
        // 逐个封死第一段:bind 成功就持有（占用）,失败也视为不可用（保留区）
        let mut blockers: Vec<std::net::TcpListener> = Vec::new();
        for p in (base..base + CANDIDATES_PER_BLOCK).rev() {
            if let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", p)) {
                blockers.push(l);
            }
        }
        let picked = pick_free_port(base).expect("第一段全占时必须跨段重试");
        // 不写死第二段首位:真机探针(netsh excludedportrange)显示 Hyper-V/WSL
        // 可能把第二段也整个保留（本机 12410-12509 覆盖 12417-12427），生产
        // 代码正确继续跨到第三段。只断言必须跨过 +10000 大步长落到后续段。
        assert!(
            picked >= base + 10_000,
            "应跨 +10000 大步长落到后续段,实际 {picked}"
        );
        assert!(
            candidate_ports(base).contains(&picked),
            "落点必须在候选序列内,实际 {picked}"
        );
    }

    #[test]
    fn port_zero_random_passes_through_candidates() {
        // port 0（随机空闲端口）路径不走回退，但 candidate_ports(0) 不panic
        assert_eq!(candidate_ports(0)[0], 0);
    }

    #[test]
    fn defaults_use_core_db_resolution_and_port() {
        let a = sv(&[]).unwrap();
        assert_eq!(a.port, DEFAULT_PORT);
        assert_eq!(a.db, kynoptic_core::db::resolve_db_path());
        assert!(!a.all);
    }

    #[test]
    fn explicit_db_port_all() {
        let a = sv(&["--db", "x/y.db", "--port", "9000", "--all"]).unwrap();
        assert_eq!(a.db, PathBuf::from("x/y.db"));
        assert_eq!(a.port, 9000);
        assert!(a.all);
    }

    #[test]
    fn port_zero_allowed() {
        assert_eq!(sv(&["--port", "0"]).unwrap().port, 0);
    }

    #[test]
    fn rejects_bad_port_and_unknown_flag() {
        assert!(sv(&["--port", "99999"]).is_err());
        assert!(sv(&["--port", "abc"]).is_err());
        assert!(sv(&["--gpu"]).is_err());
        assert!(sv(&["--db"]).is_err());
    }

    #[test]
    fn accepts_minimized_flag_written_by_autostart_and_installer() {
        // 回归:autostart 注册表与 watchdog 拉起都带 --minimized,
        // 历史上未实现该参数导致自启动启动即报错退出。
        assert!(sv(&["--minimized"]).is_ok());
        assert!(sv(&["--minimized", "--all"]).is_ok());
    }

    #[test]
    fn flag_path_override() {
        // 回归:--flag 覆盖退出旗标路径(测试注入用);缺省为 None -> exe 同目录
        let a = sv(&[]).unwrap();
        assert_eq!(a.exit_flag, None);
        let a = sv(&["--flag", r"C:\tmp\flag"]).unwrap();
        assert_eq!(a.exit_flag, Some(PathBuf::from(r"C:\tmp\flag")));
        assert!(sv(&["--flag"]).is_err());
        assert!(sv(&["--flag", ""]).is_err());
    }
}

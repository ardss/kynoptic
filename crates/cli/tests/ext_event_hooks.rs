//! F05 进程级集成测试：模拟真实 AI hook JSON 全链路——
//! 真实 hook payload（官方字段形状，脱敏自造）→ stdin 管道 → 子进程
//! `kynoptic ext-event` → 校验/幂等 → events 落库 → activity_intervals
//! 区间派生。时钟红线：进程内真实时钟只影响「接收时刻 = now」这一
//! observed 事实本身（与生产一致），区间断言只做相对关系不锚具体时刻。

use std::io::Write;
use std::process::{Command, Stdio};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ky-ext-event-it-{}-{}",
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// 跑子进程：喂 stdin，返回 (exit code, stdout, stderr)。
fn run_ctl(args: &[&str], stdin: Option<&str>, db: &std::path::Path) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kynoptic-ctl"));
    cmd.args(args)
        .arg("--db")
        .arg(db)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn kynoptic-ctl");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 初始化一个与生产同构的库（SCHEMA + 迁移；等价于采集器首次建库）。
fn init_db(db: &std::path::Path) {
    let c = rusqlite::Connection::open(db).unwrap();
    c.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
    kynoptic_core::db::run_migrations(&c).unwrap();
}

/// 全链路：Claude Code hook payload（PreToolUse → PostToolUse → Stop）经
/// stdin 进子进程 → 落库 observed ai_session 事件 → 二次发生各落一行 +
/// event_id 去重头跨时刻重发稳定去重（复审 2026-11-11 新幂等口径）→
/// rebuild 派生 ai_run 区间。
#[test]
fn ext_event_process_level_full_chain_claude() {
    let dir = temp_dir("claude");
    let db = dir.join("kyn.db");
    init_db(&db);

    // 真实 Claude Code hook payload 形状（session_id/hook_event_name/
    // tool_name；脱敏 session id；无 timestamp——与官方 payload 一致）
    let payload = r#"{"source":"claude_code","events":[
        {"hook_event_name":"PreToolUse","session_id":"it-sess-claude-1","tool_name":"Bash"},
        {"hook_event_name":"PostToolUse","session_id":"it-sess-claude-1","tool_name":"Bash","duration_ms":2500},
        {"hook_event_name":"Stop","session_id":"it-sess-claude-1"}]}"#;

    let (code, _stdout, stderr) = run_ctl(
        &["ext-event", "--source", "claude_code"],
        Some(payload),
        &db,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    // 结果 JSON 在 stderr（stdout 保持空：hook runner 对 stdout 有输出协议）
    assert!(
        _stdout.trim().is_empty(),
        "stdout 必须为空（hook 输出协议），实际: {_stdout}"
    );
    let v: serde_json::Value = serde_json::from_str(stderr.trim()).expect("stderr 应为结果 JSON");
    assert_eq!(v["received"], serde_json::json!(3));
    assert_eq!(v["inserted"], serde_json::json!(3));

    // 幂等（复审 2026-11-11 新口径）：payload 无 timestamp → 哈希输入含
    // 接收毫秒，同 payload 二次上报（必然晚于首发的接收时刻）= 第二次独立
    // observed 发生，各落一行——不再把第二次发生误判成重发吞掉（复审
    // high：纯 payload 哈希无法区分「同一条事件的重发」与「第二次发生的
    // 相同事件」）。重试端去重须显式带 event_id，见下方去重头段。
    let (code, _, stderr) = run_ctl(
        &["ext-event", "--source", "claude_code"],
        Some(payload),
        &db,
    );
    assert_eq!(code, 0, "{stderr}");
    let v: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(v["received"], serde_json::json!(3));
    assert_eq!(
        v["inserted"],
        serde_json::json!(3),
        "第二次发生各落一行: {v}"
    );
    assert_eq!(v["duplicates"], serde_json::json!(0));

    // 去重头：显式 event_id 的 payload 跨接收时刻重发稳定去重
    let eid_payload = r#"{"source":"claude_code","events":[
        {"hook_event_name":"Stop","session_id":"it-sess-claude-1","event_id":"it-stop-1"}]}"#;
    let (code, _, stderr) = run_ctl(
        &["ext-event", "--source", "claude_code"],
        Some(eid_payload),
        &db,
    );
    assert_eq!(code, 0, "{stderr}");
    let v: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(v["inserted"], serde_json::json!(1));
    let (code, _, stderr) = run_ctl(
        &["ext-event", "--source", "claude_code"],
        Some(eid_payload),
        &db,
    );
    assert_eq!(code, 0, "{stderr}");
    let v: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(
        v["inserted"],
        serde_json::json!(0),
        "同 event_id 重发应去重: {v}"
    );
    assert_eq!(v["duplicates"], serde_json::json!(1));

    // 落库形态：observed、source_id=claude_code、ext_event_id 带前缀。
    // 行数 = 首发 3 + 二次发生 3 + 去重头 1（重发去重不落行）= 7
    let conn = rusqlite::Connection::open(&db).unwrap();
    let (n, src, q): (i64, String, String) = conn
        .query_row(
            "SELECT COUNT(*), source_id, quality FROM events WHERE event_type='ai_session'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(n, 7);
    assert_eq!(src, "claude_code");
    assert_eq!(q, "observed");

    // 区间派生：rebuild 已由 ext-event 内部执行；ai_run 区间应存在且
    // quality=observed（hook 真实通道），窗口=本批事件 [最早, 最晚] 接收时刻
    let rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT app_name, quality FROM activity_intervals WHERE interval_kind='ai_run'",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        rows,
        vec![("claude_code".to_string(), "observed".to_string())]
    );

    drop(conn);
    std::fs::remove_dir_all(&dir).ok();
}

/// zcode hook payload（--session 兜底：payload 不带 session_id 时注入）
/// + 校验失败整批拒收（退出码非 0，写 0 行）。
#[test]
fn ext_event_zcode_session_fallback_and_batch_reject() {
    let dir = temp_dir("zcode");
    let db = dir.join("kyn.db");
    init_db(&db);

    // ZCode process hook 形态：session 经 --session 注入（payload 无该字段，
    // 生产配置里是 ${CLAUDE_SESSION_ID} 模板变量展开的结果）
    let payload = r#"{"source":"zcode","events":[
        {"hook_event_name":"SessionStart"},
        {"hook_event_name":"PreToolUse","tool_name":"Bash"}]}"#;
    let (code, _, stderr) = run_ctl(
        &[
            "ext-event",
            "--source",
            "zcode",
            "--session",
            "it-zc-sess-9",
        ],
        Some(payload),
        &db,
    );
    assert_eq!(code, 0, "{stderr}");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let sessions: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT json_extract(event_data,'$.session_id') FROM events WHERE event_type='ai_session'")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(sessions, vec!["it-zc-sess-9"; 2], "--session 兜底注入两行");

    // 坏事件（未知 hook_event_name）→ 整批拒收：非 0 退出 + 写 0 行
    let bad = r#"{"source":"zcode","events":[
        {"hook_event_name":"SubagentStop","session_id":"it-zc-bad"}]}"#;
    let (code, _, stderr) = run_ctl(&["ext-event", "--source", "zcode"], Some(bad), &db);
    assert_ne!(code, 0, "坏批必须失败退出: {stderr}");
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type='ai_session' AND json_extract(event_data,'$.session_id')='it-zc-bad'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "坏批不得写半批");

    // dsh：通道如实拒绝（无 hook 机制）
    let dsh = r#"{"source":"dsh","events":[{"hook_event_name":"Stop","session_id":"x"}]}"#;
    let (code, _, stderr) = run_ctl(&["ext-event", "--source", "dsh"], Some(dsh), &db);
    assert_ne!(code, 0);
    assert!(stderr.contains("dsh 无 hook 机制"), "{stderr}");

    drop(conn);
    std::fs::remove_dir_all(&dir).ok();
}

/// `ai-hooks render`：三种工具（dsh 如实缺省）片段含本机 ext-event 命令；
/// zcode 必须 enabled:true 且不含 Notification（官方不支持）。
#[test]
fn ai_hooks_render_outputs_installable_snippets() {
    let dir = temp_dir("render");
    let db = dir.join("unused.db");
    let (code, stdout, stderr) = run_ctl(&["ai-hooks", "render"], None, &db);
    assert_eq!(code, 0, "{stderr}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("stdout 应为 JSON");

    let cc = &v["claude_code"];
    assert!(cc["install_into"]
        .as_str()
        .unwrap()
        .contains("settings.json"));
    for event in ["PreToolUse", "PostToolUse", "Notification", "Stop"] {
        let hook = &cc["config"]["hooks"][event][0]["hooks"][0];
        assert_eq!(hook["type"], "command", "{event}");
        let cmd = hook["command"].as_str().unwrap();
        assert!(
            cmd.contains("ext-event --source claude_code"),
            "{event}: {cmd}"
        );
        assert!(
            cmd.ends_with("kynoptic-ctl.exe") || cmd.contains("kynoptic"),
            "{cmd}"
        );
    }

    let zc = &v["zcode"];
    assert_eq!(
        zc["config"]["hooks"]["enabled"],
        serde_json::json!(true),
        "配置文件 hooks 默认禁用，片段必须 enabled:true"
    );
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
        "PostToolUseFailure",
        "Stop",
    ] {
        let hook = &zc["config"]["hooks"]["events"][event][0]["hooks"][0];
        assert_eq!(hook["type"], "command", "{event}");
        assert!(hook["command"]
            .as_str()
            .unwrap()
            .contains("ext-event --source zcode"));
    }
    assert!(
        zc["config"]["hooks"]["events"]
            .get("Notification")
            .is_none(),
        "Notification 不在 ZCode 支持的七事件内，不得渲染"
    );
    assert!(v.get("dsh").is_none(), "dsh 无 hook 机制，不渲染");
    // notes 说明 dsh 走扫描
    let notes = v["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("dsh")),
        "notes 应说明 dsh 走文件扫描"
    );

    // --target claude_code 只出该源
    let (code, stdout, _) = run_ctl(
        &["ai-hooks", "render", "--target", "claude_code"],
        None,
        &db,
    );
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(v.get("claude_code").is_some());
    assert!(v.get("zcode").is_none());

    std::fs::remove_dir_all(&dir).ok();
}

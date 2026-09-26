#!/usr/bin/env python3
"""从旧 Python 采集器 db 导入数据到当前 kynoptic db。

用法: python migrate_legacy_db.py --legacy <旧db路径> [--target <当前db路径>]
     [--reject-unparseable]

行为契约:
- 旧库以只读 URI 挂载（ATTACH ...?mode=ro），全程只读、绝不写入；
  另开一个只读连接预检旧库可打开（不存在/非库/被锁都会显式报错退出）。
- 目标库只增不改不删：仅 INSERT，已有行一律保留；若目标「不存在/是空文件/
  缺表结构」（新装首跑场景），按 core 0001 基础 schema 补齐表结构——只补结构，
  不动任何已有数据。目标文件存在但不是数据库时拒绝写入（不覆盖用户数据）。
- metadata 表不导入（schema_version 属于目标库自身事实源，不得被旧库覆盖）。
- events 去重:
  * 原始事件行（非 input_agg 的 press/click/switch 等）按全内容去重:
    (timestamp,event_type,event_action,event_data,app_name,window_title,session_id)。
    目标已有「同时间/类型/动作、不同内容」的行时，旧库行作为新行导入并打印
    ⚠ 计数（旧算法按三元组去重会静默跳过这些行、丢原始数据——已修）。
  * input_agg 聚合行按 (timestamp,event_type,event_action) 三元组去重
    （对应 0005 部分唯一索引不变量：同分钟同类型仅一行）；旧库内部若有重复
    input_agg 行（0005 头注释记载存量库确实存在），旧库侧保 MAX(rowid) 预洗
    ——与 0005 预洗同口径——防止第二行撞目标唯一索引、拖垮整单导入。
- 时间戳导入时归一：带时区（Z/±HH:MM）转 UTC +00:00 形；无时区（naive）按
  本机时区解释后转 UTC；不可解析的值原样保留、计数并 ⚠ 告警
  （--reject-unparseable 时改为拒绝导入、退出码 1，默认不改）。
  对照写侧 normalize_timestamp（crates/core/src/db/events.rs）：带时区值
  两侧口径一致；naive 值写侧 parse_from_rfc3339 解析失败、原样保留，本
  脚本按本机时区改写，属超集行为（只会出现在旧导入遗留的 naive 值上）；
  子秒形式：本脚本裁剪尾零，写侧为固定 3 位毫秒。
- sessions / daily_agg 按主键/唯一键去重合并（daily_agg 用 INSERT OR IGNORE，
  目标已有 key 一律保留）。旧库缺列（total_events/idle_seconds 等）按目标
  schema 的 DEFAULT 补值（数值列 → 0），不显式写 NULL——NULL 行会被下游
  类型化读取（daily_agg::recent 等）整行跳过，该天数据静默缺失。
- 旧库 events.event_action 可空且含 NULL 行：目标侧该列 NOT NULL（0001
  schema）无法落库，导入前显式非零退出（与旧脚本 NOT NULL 约束失败
  exit 1 同类行为），不静默丢弃、不误报成功。
- 可安全重复执行（幂等）：重跑时全量重复行被去重跳过、导入 0 行。
"""

import argparse
import os
import sqlite3
import sys
from datetime import datetime, timezone
from urllib.parse import quote


# core 0001 基础 schema（与 crates/core/src/db/migrations/0001_init.sql 一致；
# 仅在目标缺表结构时补齐，存量目标库一律不动）
BOOTSTRAP_SCHEMA = """
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    event_type TEXT NOT NULL,
    event_action TEXT NOT NULL,
    event_data TEXT,
    app_name TEXT,
    window_title TEXT,
    session_id INTEGER
);
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    start_time TEXT NOT NULL,
    end_time TEXT,
    total_events INTEGER DEFAULT 0,
    idle_seconds REAL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY,
    value TEXT
);
CREATE TABLE IF NOT EXISTS daily_agg (
    date TEXT PRIMARY KEY,
    keys INTEGER DEFAULT 0,
    clicks INTEGER DEFAULT 0,
    active_minutes INTEGER DEFAULT 0,
    apm_avg REAL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
CREATE INDEX IF NOT EXISTS idx_events_app ON events(app_name);
CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);
CREATE INDEX IF NOT EXISTS idx_events_type_action_ts ON events(event_type, event_action, timestamp);
"""

# events 去重比较的内容列（旧库缺列时按导入默认值同口径比较）
EVENTS_CONTENT_COLS = ("event_data", "app_name", "window_title", "session_id")


def sqlite_uri(path, mode):
    """本地路径 → SQLite URI（file:...?mode=ro/rwc）；Windows 反斜杠转正斜杠，
    路径按 URI 规则转义（空格等）。SQL 字面量里的单引号转义由调用方再做。"""
    p = path.replace("\\", "/")
    return "file:" + quote(p, safe="/") + "?mode=" + mode


def normalize_ts_value(ts, local_tz):
    """单值时间戳归一。返回 (新值, 可解析)。
    带时区 → UTC +00:00 形；naive → 按 local_tz 解释后转 UTC（写侧
    parse_from_rfc3339 拒绝解析 naive、原样保留，本函数是超集，只影响
    旧导入遗留的 naive 值）；不可解析 → 原值。
    输出为 +00:00 形：无子秒时不带小数（与 to_rfc3339 一致），有子秒时
    裁剪尾零（.5）；写侧 to_rfc3339 为固定 3 位毫秒（.500）。"""
    if not isinstance(ts, str):
        return ts, False
    s = ts.strip()
    if not s:
        return ts, False
    t = s
    if t.endswith(("Z", "z")):
        t = t[:-1] + "+00:00"
    try:
        dt = datetime.fromisoformat(t)
    except ValueError:
        return ts, False
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=local_tz)
    dt = dt.astimezone(timezone.utc)
    base = dt.strftime("%Y-%m-%dT%H:%M:%S")
    if dt.microsecond:
        frac = ".%06d" % dt.microsecond
        while frac.endswith("0"):
            frac = frac[:-1]
        return base + frac + "+00:00", True
    return base + "+00:00", True


def table_cols(conn, schema, table):
    """取表列 {列名: notnull}；表不存在返回 {}。schema 取 'main'/'legacy_db'。"""
    try:
        rows = conn.execute("PRAGMA %s.table_info(%s)" % (schema, table))
    except sqlite3.Error:
        return {}
    return {r[1]: bool(r[3]) for r in rows.fetchall()}


def table_defaults(conn, schema, table):
    """取表列默认值 {列名: 默认值 SQL 字面量}（无默认 → None）。
    旧库缺列时按目标 schema 的 DEFAULT 补值——旧版实现省略缺列、由默认值
    生效；显式写 NULL 会让下游类型化读取（daily_agg::recent 等）整行跳过。
    （INSERT ... SELECT 的列表达式里 DEFAULT 关键字是语法错误，故取字面量。）"""
    try:
        rows = conn.execute("PRAGMA %s.table_info(%s)" % (schema, table))
    except sqlite3.Error:
        return {}
    return {r[1]: r[4] for r in rows.fetchall()}


def default_expr(notnull):
    """旧库缺列时的导入默认值（仅 events 用：目标 events 内容列均可空、
    DEFAULT 即 NULL，去重谓词也要按该值比较）。sessions/daily_agg 缺列
    按目标 schema 的默认值字面量补（table_defaults），见
    import_sessions / import_daily_agg 注释。"""
    return "''" if notnull else "NULL"


def build_ts_map(target, legacy_conn):
    """旧库 events 的 DISTINCT timestamp → 归一值映射（只收发生变化的值，其余
    走 COALESCE 回落原值）。变更映射进 temp 表（挂目标连接、不落目标库文件）。
    返回 (变更映射, 不可解析值列表)。"""
    local_tz = datetime.now().astimezone().tzinfo
    changed = []
    unparsed = []
    for (v,) in legacy_conn.execute("SELECT DISTINCT timestamp FROM main.events"):
        new, ok = normalize_ts_value(v, local_tz)
        if not ok:
            unparsed.append(v)
        elif new != v:
            changed.append((v, new))
    target.execute("CREATE TEMP TABLE ts_map (old_ts TEXT PRIMARY KEY, new_ts TEXT NOT NULL)")
    target.executemany("INSERT INTO ts_map (old_ts, new_ts) VALUES (?, ?)", changed)
    return changed, unparsed


def events_sql_parts(target, legacy_cols):
    """events 导入 SELECT 侧：列表达式 + 全内容去重谓词片段。
    时间戳列用 COALESCE(tsm.new_ts, L.timestamp)（无归一映射时即原值），
    与去重谓词里的目标侧比较同口径。"""
    target_cols = table_cols(target, "main", "events")
    tcols = [c for c in target_cols if c != "id"]
    shared = [c for c in tcols if c in legacy_cols]
    exprs = []
    for c in tcols:
        if c == "timestamp":
            exprs.append("COALESCE(tsm.new_ts, L.timestamp)")
        elif c in shared:
            exprs.append("L." + c)
        else:
            exprs.append(default_expr(target_cols[c]))
    # 全内容谓词（T = 目标行）：共有列按 NULL 安全等值；旧库缺列按导入默认值比较
    preds = []
    for c in EVENTS_CONTENT_COLS:
        if c not in target_cols:
            continue
        if c in shared:
            preds.append("((L.%s IS NULL AND T.%s IS NULL) OR L.%s = T.%s)" % (c, c, c, c))
        elif target_cols[c]:
            preds.append("T.%s = ''" % c)
        else:
            preds.append("T.%s IS NULL" % c)
    return tcols, exprs, "\n      AND ".join(preds) if preds else ""


TRIPLE = ("T.timestamp = COALESCE(tsm.new_ts, L.timestamp) "
          "AND T.event_type = L.event_type AND T.event_action = L.event_action")
JOIN = "LEFT JOIN temp.ts_map tsm ON tsm.old_ts = L.timestamp"
# input_agg 旧库侧预洗（0005 头注释记载存量库确有重复 input_agg 行）：
# 同（归一后时间, event_type）只保 MAX(rowid) 一行，与 0005 预洗同口径；
# 不做这一步，第二行会撞目标侧部分唯一索引、整单导入回滚（events/sessions/
# daily_agg 全部 0 行、退出码 1）
LEGACY_AGG_PRED = """AND NOT EXISTS (
            SELECT 1 FROM legacy_db.events L2
            LEFT JOIN temp.ts_map tsm2 ON tsm2.old_ts = L2.timestamp
            WHERE L2.event_action = 'input_agg'
              AND L2.event_type = L.event_type
              AND COALESCE(tsm2.new_ts, L2.timestamp) = COALESCE(tsm.new_ts, L.timestamp)
              AND L2.rowid > L.rowid
        )"""
INSERT_TPL = """
        INSERT INTO main.events (%(cols)s)
        SELECT %(exprs)s
        FROM legacy_db.events L
        %(join)s
        WHERE L.event_action %(action_cnd)s
%(legacy_pred)s
          AND NOT EXISTS (
            SELECT 1 FROM main.events T
            WHERE %(triple)s%(content)s
          )
"""


def import_events(target):
    """events 两段导入（原始行全内容去重 / input_agg 三元组去重）。
    返回 (原始导入, input_agg导入, 原始全量重复跳过, 原始同三元组异内容导入数)。"""
    legacy_cols = table_cols(target, "legacy_db", "events")
    if not legacy_cols:
        print("旧库无 events 表，跳过")
        return 0, 0, 0, 0
    tcols, exprs, content_preds = events_sql_parts(target, legacy_cols)
    content = "\n      AND " + content_preds if content_preds else ""
    base = "FROM legacy_db.events L " + JOIN + " WHERE L.event_action <> 'input_agg'"
    # 导入前统计（目标当前态快照）：同三元组行的存在性，区分「全量重复」与「异内容」
    raw_dup = target.execute(
        "SELECT COUNT(*) " + base + "\n         AND EXISTS (\n"
        "           SELECT 1 FROM main.events T\n"
        "           WHERE " + TRIPLE + content + "\n         )").fetchone()[0]
    raw_divergent = target.execute(
        "SELECT COUNT(*) " + base + "\n         AND EXISTS (\n"
        "           SELECT 1 FROM main.events T\n"
        "           WHERE " + TRIPLE + "\n         )\n         AND NOT EXISTS (\n"
        "           SELECT 1 FROM main.events T\n"
        "           WHERE " + TRIPLE + content + "\n         )").fetchone()[0]
    raw_n = target.execute(INSERT_TPL % {
        "cols": ", ".join(tcols), "exprs": ", ".join(exprs), "join": JOIN,
        "action_cnd": "<> 'input_agg'", "legacy_pred": "",
        "triple": TRIPLE, "content": content}).rowcount
    agg_n = target.execute(INSERT_TPL % {
        "cols": ", ".join(tcols), "exprs": ", ".join(exprs), "join": JOIN,
        "action_cnd": "= 'input_agg'", "legacy_pred": LEGACY_AGG_PRED,
        "triple": TRIPLE, "content": ""}).rowcount
    return raw_n, agg_n, raw_dup, raw_divergent


def import_sessions(target):
    legacy_cols = table_cols(target, "legacy_db", "sessions")
    if not legacy_cols:
        return 0
    target_cols = table_cols(target, "main", "sessions")
    defaults = table_defaults(target, "main", "sessions")
    tcols = [c for c in target_cols if c != "id"]
    # 旧库缺列按目标 schema 的 DEFAULT 字面量补值（total_events/idle_seconds →
    # 0）：与旧版「省略缺列、由默认值生效」同语义；显式写 NULL 会让该行被
    # 下游类型化读取（daily_agg::recent 等）整行跳过
    exprs = [
        ("L." + c) if c in legacy_cols else
        (defaults[c] if defaults.get(c) is not None else "NULL")
        for c in tcols
    ]
    cur = target.execute(
        """
        INSERT INTO main.sessions (%s)
        SELECT %s FROM legacy_db.sessions L
        WHERE NOT EXISTS (
            SELECT 1 FROM main.sessions T
            WHERE T.start_time = L.start_time
        )
        """ % (", ".join(tcols), ", ".join(exprs)))
    return cur.rowcount


def import_daily_agg(target):
    legacy_cols = table_cols(target, "legacy_db", "daily_agg")
    if not legacy_cols:
        return 0
    target_cols = table_cols(target, "main", "daily_agg")
    defaults = table_defaults(target, "main", "daily_agg")
    # 旧库缺列（active_minutes/apm_avg 等）按目标 schema 的 DEFAULT 字面量
    # 补值（→ 0），与旧版「省略缺列由 DEFAULT 生效」同语义；NULL 行不会进
    # 零值清理谓词（active_minutes = 0 对 NULL 不成立）且下游整行跳过
    exprs = [
        ("L." + c) if c in legacy_cols else
        (defaults[c] if defaults.get(c) is not None else "NULL")
        for c in target_cols
    ]
    cur = target.execute(
        """
        INSERT OR IGNORE INTO main.daily_agg (%s)
        SELECT %s FROM legacy_db.daily_agg L
        """ % (", ".join(target_cols), ", ".join(exprs)))
    return cur.rowcount


def main():
    ap = argparse.ArgumentParser(description="从旧 Python 采集器 db 导入到 kynoptic db")
    ap.add_argument("--legacy", required=True, help="旧 db 路径（只读挂载）")
    ap.add_argument("--target", default=None, help="当前 db 路径（默认随 kynoptic 配置）")
    ap.add_argument("--reject-unparseable", action="store_true",
                    help="拒绝：旧库 events 存在不可解析时间戳时终止导入（退出码 1）。"
                         "默认行为是原样保留这些值并 ⚠ 告警")
    args = ap.parse_args()

    target_path = args.target
    if not target_path:
        # 与 CLI 默认库一致：%LOCALAPPDATA%/kynoptic/kynoptic.db
        base = os.environ.get("LOCALAPPDATA") or os.path.expanduser("~")
        target_path = os.path.join(base, "kynoptic", "kynoptic.db")
        print(f"未指定 --target，使用默认: {target_path}")

    # 旧库：独立只读连接预检（不存在/非库/被锁 → 显式报错，绝不写旧库）；
    # 之后复用该连接取 DISTINCT timestamp（只读）
    try:
        legacy_probe = sqlite3.connect(sqlite_uri(args.legacy, "ro"), uri=True)
        legacy_probe.execute("SELECT count(*) FROM sqlite_master")
    except (sqlite3.Error, OSError) as e:
        print(f"✗ 无法只读打开旧库: {e}")
        return 1

    # 目标库：URI 打开（mode=rwc：不存在则建空库）。连接必须带 URI 标志，
    # 后面的 ATTACH 只读 URI 才会被按 URI 解释
    try:
        target = sqlite3.connect(sqlite_uri(target_path, "rwc"), uri=True)
    except sqlite3.Error as e:
        print(f"✗ 无法打开目标库: {e}")
        return 1
    try:
        target.execute("SELECT count(*) FROM sqlite_master")
    except sqlite3.DatabaseError as e:
        # 目标文件存在但不是数据库：拒绝在其上建表/写数（不覆盖用户数据）
        target.close()
        legacy_probe.close()
        print(f"✗ 目标不是有效数据库，拒绝写入: {e}")
        return 1

    # 旧库只读挂载（SQLite 的 ATTACH 不支持绑定参数；先 URI 转义、再转 SQL
    # 单引号，?mode=ro 使导入全程对旧库只读，写 legacy_db.* 会直接报错）
    safe_uri = sqlite_uri(args.legacy, "ro").replace("'", "''")
    try:
        target.execute(f"ATTACH DATABASE '{safe_uri}' AS legacy_db")
    except sqlite3.Error as e:
        print(f"✗ 数据错误: 无法只读附加旧库: {e}")
        target.close()
        legacy_probe.close()
        return 1

    legacy_cols = table_cols(target, "legacy_db", "events")
    has_events = bool(legacy_cols)

    # 旧库数据完整性门禁（补建/导入之前）：目标侧 event_action 列 0001
    # schema 为 NOT NULL，NULL-action 行无法落库；两段导入谓词
    # （<> 'input_agg' / = 'input_agg'）对 NULL 行均为 NULL、会被静默丢弃，
    # 且 0 行横幅会误报「目标已含全部行」——在此显式终止（旧脚本 NOT NULL
    # 约束失败 exit 1 同类行为；只读 ATTACH 同时避免旧脚本 rw 挂载在空目标
    # 库时写回源库的事故）
    if has_events and "event_action" in legacy_cols:
        null_action = legacy_probe.execute(
            "SELECT COUNT(*) FROM main.events WHERE event_action IS NULL"
        ).fetchone()[0]
        if null_action:
            print(f"✗ 旧库 events 有 {null_action} 行 event_action 为 NULL：目标库"
                  f"该列 NOT NULL、无法落库，终止导入（请先修复旧库这些行）")
            return 1

    # 时间戳归一映射（补建/导入之前算好；--reject-unparseable 命中则原样退出，
    # 此时尚未动目标库任何内容）
    ts_map_rows, unparsed = ([], [])
    if has_events:
        ts_map_rows, unparsed = build_ts_map(target, legacy_probe)
    if args.reject_unparseable and unparsed:
        print(f"✗ 旧库有 {len(unparsed)} 个不可解析时间戳值（如: {unparsed[:3]}），"
              f"--reject-unparseable 生效，终止导入")
        return 1

    total = 0
    raw_n = agg_n = raw_dup = 0
    try:
        # 目标缺表结构时按 0001 补齐（只增不改：已有表/行一律不动）
        have = {r[0] for r in target.execute(
            "SELECT name FROM main.sqlite_master WHERE type='table'")}
        missing = [t for t in ("events", "sessions", "metadata", "daily_agg")
                   if t not in have]
        if missing:
            target.executescript(BOOTSTRAP_SCHEMA)
            print(f"⚠ 目标库缺少表: {', '.join(missing)}，已按 core 0001 基础 schema 补齐")
            target.commit()

        if not has_events:
            print("旧库无 events 表，跳过")
        else:
            raw_n, agg_n, raw_dup, raw_divergent = import_events(target)
            print(f"events: 导入 {raw_n + agg_n} 行（原始 {raw_n} + input_agg {agg_n}）")
            total += raw_n + agg_n
            if raw_divergent:
                print(f"⚠ 目标已有 {raw_divergent} 行同（时间/类型/动作）但内容不同："
                      f"已作为新行导入，未丢失")
            if raw_dup:
                print(f"  其中 {raw_dup} 行与目标完全相同，跳过（幂等）")
        n = import_sessions(target)
        print(f"sessions: 导入 {n} 行")
        total += n
        n = import_daily_agg(target)
        print(f"daily_agg: 导入 {n} 行")
        total += n
        if ts_map_rows:
            print(f"已把 {len(ts_map_rows)} 个不同时间戳值归一为 UTC +00:00 形")
        if unparsed:
            print(f"⚠ {len(unparsed)} 个不可解析时间戳值原样保留（如: {unparsed[:3]}")
        target.commit()
    except sqlite3.Error as e:
        target.rollback()
        print(f"✗ 数据错误: {e}")
        return 1
    finally:
        target.close()
        legacy_probe.close()

    # 成功横幅按结果区分：no-op（⚠）与真实导入（✓）不可混报，退出码保持 0
    if not has_events:
        print("⚠ 未导入任何事件行（旧库无 events 表）")
    if total == 0:
        reason = "旧库无 events 表" if not has_events else "目标已含全部行"
        print(f"⚠ 未导入任何行（{reason}）")
    else:
        print(f"✓ 迁移完成，共导入 {total} 行")
    return 0


if __name__ == "__main__":
    sys.exit(main())

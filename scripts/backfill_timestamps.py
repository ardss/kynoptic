#!/usr/bin/env python3
"""存量库时间戳一次性 backfill：把 events.timestamp 归一为 UTC +00:00 形。

背景：旧版本导入或外部写入的行可能带混合编码（Z 后缀 / ±HH:MM 偏移 / 无时区
naive 形）。导出/清理窗口按「+00:00 形字符串 >= cutoff」比较，混合编码下
字典序不等于时间序，会让窗口外的行混进导出、窗口内的行被漏掉（同一时刻
落在窗口两侧）。本 backfill 的归一规则与写侧 normalize_timestamp
（crates/core/src/db/events.rs）对照：带时区值两侧口径一致（改写为
UTC +00:00 形）；无时区（naive）值写侧 parse_from_rfc3339 解析失败、
原样保留，本脚本按本机时区解释后改写——超集行为，只影响旧导入遗留的
naive 值；子秒形式：本脚本裁剪尾零，写侧为固定 3 位毫秒：
- 带时区值 → UTC +00:00 形；
- 无时区（naive）值 → 按本机时区解释后转 UTC（与导入侧归一同假设）；
- 不可解析值原样保留、只计数（绝不改坏数据）。

用法: python backfill_timestamps.py [--target <db路径>] [--dry-run]
     （--target 缺省用 %LOCALAPPDATA%/kynoptic/kynoptic.db，与 migrate 脚本默认一致）

契约：不增不删行；只 UPDATE 可解析值的时间戳编码字段（同一时刻的等价改写，
不是数据删除）。可安全重跑（幂等）：第二次运行没有可变化值、报 0 行。
"""

import argparse
import os
import sqlite3
import sys
from datetime import datetime, timezone


def normalize_ts_value(ts, local_tz):
    """单值时间戳归一。返回 (新值, 可解析)。口径与
    scripts/migrate_legacy_db.py 一致；对照写侧 normalize_timestamp
    （events.rs）：带时区值两侧一致（UTC +00:00 形），naive 值写侧解析
    失败原样保留、本函数按 local_tz 改写（超集）；不可解析 → 原值。
    输出为 +00:00 形：无子秒不带小数（与 to_rfc3339 一致），有子秒裁剪
    尾零（.5），写侧为固定 3 位毫秒（.500）。"""
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


def main():
    ap = argparse.ArgumentParser(description="存量库 events.timestamp 一次性归一 backfill")
    ap.add_argument("--target", default=None, help="当前 db 路径（默认随 kynoptic 配置）")
    ap.add_argument("--dry-run", action="store_true", help="只报告需要改写的值与行数，不写库")
    args = ap.parse_args()

    target_path = args.target
    if not target_path:
        base = os.environ.get("LOCALAPPDATA") or os.path.expanduser("~")
        target_path = os.path.join(base, "kynoptic", "kynoptic.db")
        print(f"未指定 --target，使用默认: {target_path}")

    try:
        conn = sqlite3.connect(target_path)
        conn.execute("SELECT count(*) FROM sqlite_master")
    except sqlite3.Error as e:
        print(f"✗ 无法打开目标库: {e}")
        return 1
    try:
        has_events = conn.execute(
            "SELECT 1 FROM main.sqlite_master WHERE type='table' AND name='events'"
        ).fetchone() is not None
        if not has_events:
            print("目标库无 events 表，无需 backfill")
            return 0

        local_tz = datetime.now().astimezone().tzinfo
        changed, unparsed = [], []
        for (v,) in conn.execute("SELECT DISTINCT timestamp FROM events"):
            new, ok = normalize_ts_value(v, local_tz)
            if not ok:
                unparsed.append(v)
            elif new != v:
                changed.append((v, new))

        rows = 0
        if changed:
            by_val = {v: n for v, n in conn.execute(
                "SELECT timestamp, COUNT(*) FROM events GROUP BY timestamp")}
            rows = sum(by_val.get(v, 0) for v, _ in changed)
            for v, new in changed[:5]:
                print(f"  {v} -> {new}")
            if len(changed) > 5:
                print(f"  …另有 {len(changed) - 5} 个不同值")

        if args.dry_run:
            print(f"--dry-run：将改写 {rows} 行（{len(changed)} 个不同时间戳值）"
                  f"，未写库")
            if unparsed:
                print(f"⚠ {len(unparsed)} 个不可解析时间戳值将原样保留（如: {unparsed[:3]}")
            return 0

        if not changed:
            print("没有需要 backfill 的时间戳")
            if unparsed:
                print(f"⚠ {len(unparsed)} 个不可解析时间戳值原样保留（如: {unparsed[:3]}")
            return 0

        # UPDATE 占位符顺序是 (SET 值, WHERE 值)，而 changed 存的是 (旧值, 新值)——
        # 必须逐条换位，否则变成 SET 旧值 WHERE 新值（匹配 0 行，静默不生效）
        cur = conn.executemany(
            "UPDATE events SET timestamp=? WHERE timestamp=?",
            [(new, old) for old, new in changed])
        conn.commit()
        # 行数用改写前的逐值计数（executemany 的 rowcount 语义跨 Python 版本不保证）
        print(f"✓ backfill 完成：{rows} 行（{len(changed)} 个不同值归一为 UTC +00:00 形）")
        if unparsed:
            print(f"⚠ {len(unparsed)} 个不可解析时间戳值原样保留（如: {unparsed[:3]}")
    except sqlite3.Error as e:
        conn.rollback()
        print(f"✗ 数据错误: {e}")
        return 1
    finally:
        conn.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())

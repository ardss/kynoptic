#!/usr/bin/env python3
"""从旧 Python 采集器 db 导入数据到当前 kynoptic db。

用法: python migrate_legacy_db.py --legacy <旧db路径> [--target <当前db路径>]

- 只增不改不删：目标库已有行一律保留，按 (timestamp, event_type, event_action)
  去重后追加导入，可安全重复执行（幂等）。
- metadata 表不导入（schema_version 属于目标库自身事实源，不得被旧库覆盖）。
- daily_agg / sessions 同样按主键/唯一键去重合并。
"""

import argparse
import sqlite3
import sys


def table_columns(conn, table):
    cur = conn.execute(f"PRAGMA table_info({table})")
    return [row[1] for row in cur.fetchall()]


def import_events(legacy, target):
    """导入 events：旧库缺列（如 app_name）时按目标列补 NULL/''。"""
    legacy_cols = table_columns(legacy, "events")
    if not legacy_cols:
        print("旧库无 events 表，跳过")
        return 0
    # 目标库 events 列（不含自增 id，让目标库自行分配）
    target_cols = [c for c in table_columns(target, "events") if c != "id"]
    missing = [c for c in target_cols if c not in legacy_cols]
    shared = [c for c in target_cols if c in legacy_cols]
    col_exprs = [f"L.{c}" for c in shared] + [
        "NULL" if c in ("event_data", "app_name", "window_title", "session_id") else "''"
        for c in missing
    ]
    dedupe = "(timestamp, event_type, event_action)"
    sql = f"""
        INSERT INTO events ({", ".join(target_cols)})
        SELECT {", ".join(col_exprs)} FROM legacy_db.events L
        WHERE NOT EXISTS (
            SELECT 1 FROM events T
            WHERE T.timestamp = L.timestamp
              AND T.event_type = L.event_type
              AND T.event_action = L.event_action
        )
    """
    cur = target.execute(sql)
    return cur.rowcount


def import_sessions(legacy, target):
    if not table_columns(legacy, "sessions"):
        return 0
    cols = [c for c in table_columns(target, "sessions") if c != "id"]
    cur = target.execute(
        f"""
        INSERT INTO sessions ({", ".join(cols)})
        SELECT {", ".join("L." + c for c in cols)} FROM legacy_db.sessions L
        WHERE NOT EXISTS (
            SELECT 1 FROM sessions T
            WHERE T.start_time = L.start_time
        )
        """
    )
    return cur.rowcount


def import_daily_agg(legacy, target):
    if not table_columns(legacy, "daily_agg"):
        return 0
    cols = table_columns(target, "daily_agg")
    cur = target.execute(
        f"""
        INSERT OR IGNORE INTO daily_agg ({", ".join(cols)})
        SELECT {", ".join("L." + c for c in cols)} FROM legacy_db.daily_agg L
        """
    )
    return cur.rowcount


def main():
    ap = argparse.ArgumentParser(description="从旧 Python 采集器 db 导入到 kynoptic db")
    ap.add_argument("--legacy", required=True, help="旧 db 路径")
    ap.add_argument("--target", default=None, help="当前 db 路径（默认随 kynoptic 配置）")
    args = ap.parse_args()

    target_path = args.target
    if not target_path:
        # 与 CLI 默认库一致：%LOCALAPPDATA%/kynoptic/kynoptic.db
        import os

        base = os.environ.get("LOCALAPPDATA") or os.path.expanduser("~")
        target_path = os.path.join(base, "kynoptic", "kynoptic.db")
        print(f"未指定 --target，使用默认: {target_path}")

    legacy = sqlite3.connect(f"file:{args.legacy}?mode=ro", uri=True)
    target = sqlite3.connect(target_path)
    try:
        # SQLite 的 ATTACH 不支持绑定参数，这里手动转义单引号
        safe = args.legacy.replace("'", "''")
        target.execute(f"ATTACH DATABASE '{safe}' AS legacy_db")
    except sqlite3.Error as e:
        print(f"✗ 数据错误: 无法附加旧库: {e}")
        return 1

    total = 0
    try:
        n = import_events(legacy, target)
        print(f"events: 导入 {n} 行")
        total += n
        n = import_sessions(legacy, target)
        print(f"sessions: 导入 {n} 行")
        total += n
        n = import_daily_agg(legacy, target)
        print(f"daily_agg: 导入 {n} 行")
        total += n
        target.commit()
    except sqlite3.Error as e:
        target.rollback()
        print(f"✗ 数据错误: {e}")
        return 1
    finally:
        legacy.close()
        target.close()
    print(f"✓ 迁移完成，共导入 {total} 行")
    return 0


if __name__ == "__main__":
    sys.exit(main())

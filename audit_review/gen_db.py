import sqlite3, os, sys
from datetime import datetime, timedelta, timezone

MIG = r"K:\kynoptic\crates\core\src\db\migrations"
OUT = r"K:\kynoptic\audit_review"

LH = timezone(timedelta(hours=8))  # local = UTC+8 (same as this machine)

def utc_rfc(dt_utc):
    return dt_utc.strftime("%Y-%m-%dT%H:%M:%S+00:00")

def to_utc(dt_local):
    return dt_local.replace(tzinfo=LH).astimezone(timezone.utc)

def fmt_local_day(d):
    return d.strftime("%Y-%m-%d")

def make_db(name):
    path = os.path.join(OUT, name)
    if os.path.exists(path):
        os.remove(path)
    conn = sqlite3.connect(path)
    for f in sorted(os.listdir(MIG)):
        if f.endswith(".sql"):
            sql = open(os.path.join(MIG, f), encoding="utf-8").read()
            try:
                conn.executescript(sql)
            except Exception as e:
                print(f"[{name}] migration {f}: {e}")
    conn.commit()
    return conn, path

def ins_events(conn, rows):
    conn.executemany(
        "INSERT INTO events(timestamp,event_type,event_action,event_data,app_name,window_title) VALUES(?,?,?,?,?,?)",
        rows)
    conn.commit()

def ins_minute(conn, date, hour, minute, bucket, val):
    conn.execute("INSERT OR REPLACE INTO agg_minute(date,hour,minute,bucket_id,sum_value,count_value) VALUES(?,?,?,?,?,1)",
                 (date, hour, minute, bucket, val))

def ins_daily_agg(conn, date, keys, clicks, minutes, apm):
    conn.execute("INSERT OR REPLACE INTO daily_agg(date,keys,clicks,active_minutes,apm_avg) VALUES(?,?,?,?,?)",
                 (date, keys, clicks, minutes, apm))

def ins_agg_daily(conn, date, bucket, count):
    conn.execute("INSERT OR REPLACE INTO agg_daily(date,bucket_id,count_value) VALUES(?,?,?)",
                 (date, bucket, count))

today = datetime(2026, 9, 17)  # local (machine date)

# ---------------------------------------------------------------- DB1: apm_burst + late_night
conn, p1 = make_db("db1_apm.db")
# prior 7 days baseline: apm_avg = 60 each day (has_minute not needed; daily_agg only)
for i in range(1, 8):
    d = fmt_local_day(today - timedelta(days=i))
    ins_daily_agg(conn, d, 60000, 5000, 300, 60.0)
# today via agg_minute path (has_minute_for_date true)
# (a) 2x burst: 120 keys in one minute at 10:00 local, min_count 100 satisfied, ratio 2 < 3 -> no alert
ins_minute(conn, fmt_local_day(today), 10, 0, "input_keys", 120)
# (b) 13x burst: 800 keys at 11:00 local -> ratio 13.3 -> alert
ins_minute(conn, fmt_local_day(today), 11, 0, "input_keys", 800)
# (c) late_night local: 60 keys at 23:30 local today -> hour 23 >= 23, sum 60 >= 50 -> warn
ins_minute(conn, fmt_local_day(today), 23, 30, "input_keys", 60)
# (d) UTC 16:00 = local 00:00 attribution: 40 keys at local 00:10 today -> should NOT count as "23点后"
ins_minute(conn, fmt_local_day(today), 0, 10, "input_keys", 40)
conn.commit(); conn.close()

# ---------------------------------------------------------------- DB1b: late_night fallback path (no agg_minute)
conn, p1b = make_db("db1b_latenight_fallback.db")
rows = []
# local today 07:30 (= UTC 2026-09-16T23:30) 60 keyboard presses -> fallback hour check 23>=23 -> FALSE POSITIVE expected
t = to_utc(datetime(2026, 9, 17, 7, 30))
for i in range(60):
    rows.append((utc_rfc(t + timedelta(seconds=i)), "keyboard", "press", None, "code.exe", "editor"))
# local yesterday 23:30 (= UTC 15:30) 60 presses -> true late night, fallback hour 15 < 23 -> MISS expected
t2 = to_utc(datetime(2026, 9, 16, 23, 30))
for i in range(60):
    rows.append((utc_rfc(t2 + timedelta(seconds=i)), "keyboard", "press", None, "code.exe", "editor"))
ins_events(conn, rows); conn.close()

# ---------------------------------------------------------------- DB2: marathon / bridge
conn, p2 = make_db("db2_marathon.db")
d_today = fmt_local_day(today)
# today: active minutes 09:00-09:59 (60), hole 10:00-10:01 (2 min), 10:02-12:59 (178) -> streak max 178 (<180, no marathon); bridged 15 would give 240
for m in range(0, 60):
    ins_minute(conn, d_today, 9, m, "input_keys", 5)
for m in range(2, 60):
    ins_minute(conn, d_today, 10, m, "input_keys", 5)
for m in range(0, 60):
    ins_minute(conn, d_today, 11, m, "input_keys", 5)
for m in range(0, 60):
    ins_minute(conn, d_today, 12, m, "input_keys", 5)
conn.commit(); conn.close()

# DB2b: 8h "active" only heartbeats, no input -> expect no marathon
conn, p2b = make_db("db2b_idle_heartbeats.db")
rows = []
t = to_utc(datetime(2026, 9, 17, 1, 0))
for i in range(0, 8 * 60, 5):
    tt = t + timedelta(minutes=i)
    rows.append((utc_rfc(tt), "system", "heartbeat", '{"memory":{"used_percent":40},"idle_seconds":290}', None, None))
ins_events(conn, rows); conn.close()

# DB2c: clean 200-min streak -> expect marathon
conn, p2c = make_db("db2c_marathon_yes.db")
for m in range(0, 200):
    ins_minute(conn, d_today, 8, m, "input_keys", 5)
conn.commit(); conn.close()

# ---------------------------------------------------------------- DB3: insights
conn, p3 = make_db("db3_insights.db")
rows = []
for i in range(6, -1, -1):  # last 7 days including today
    day = today - timedelta(days=i)
    base = to_utc(day.replace(hour=9, minute=0))
    for k in range(60):  # 60 acts between 09:00-10:00 -> golden hours 09-11 region
        tt = base + timedelta(seconds=k * 10)
        rows.append((utc_rfc(tt), "keyboard", "press", None, "appA.exe", "A"))
    # window switches for dwell + fragmented hour: 25 switches at 14:00-14:59 each day
    base2 = to_utc(day.replace(hour=14, minute=0))
    for k in range(25):
        tt = base2 + timedelta(minutes=k * 2)
        rows.append((utc_rfc(tt), "window", "switch", None, "appB.exe" if k % 2 else "appA.exe", "w"))
# rhythm edge cases: a day with a single event (D-3 at 12:00), plus an exactly-midnight event today (00:00 local)
day = today - timedelta(days=3)
rows.append((utc_rfc(to_utc(day.replace(hour=12, minute=0))), "keyboard", "press", None, "appA.exe", "A"))
rows.append((utc_rfc(to_utc(today.replace(hour=0, minute=0))), "keyboard", "press", None, "appA.exe", "A"))
# 02:00 local events for insights late-night (h<6) card: 40 events today 02:00
base = to_utc(today.replace(hour=2, minute=0))
for k in range(40):
    rows.append((utc_rfc(base + timedelta(seconds=k)), "keyboard", "press", None, "appC.exe", "C"))
ins_events(conn, rows); conn.close()

# DB3b: sparse single-day user (new install): only 30 acts today
conn, p3b = make_db("db3b_newuser.db")
rows = []
base = to_utc(today.replace(hour=9, minute=0))
for k in range(30):
    rows.append((utc_rfc(base + timedelta(minutes=k)), "keyboard", "press", None, "newapp.exe", "N"))
ins_events(conn, rows); conn.close()

# ---------------------------------------------------------------- DB4: trends week compare
conn, p4 = make_db("db4_trends.db")
# last week D-14..D-8: rows exist but all zeros (7 rows)
for i in range(8, 15):
    ins_daily_agg(conn, fmt_local_day(today - timedelta(days=i)), 0, 0, 0, 0.0)
# this week D-7..D-1: 60 active minutes each
for i in range(1, 8):
    ins_daily_agg(conn, fmt_local_day(today - timedelta(days=i)), 3000, 300, 60, 10.0)
conn.commit(); conn.close()

# ---------------------------------------------------------------- DB5: new_app_surge
conn, p5 = make_db("db5_surge.db")
# history: appOld 20 events/day for 10 days (agg_daily), today 200 -> 10x -> warn
for i in range(1, 11):
    ins_agg_daily(conn, fmt_local_day(today - timedelta(days=i)), "app:appOld.exe", 20)
ins_agg_daily(conn, d_today, "app:appOld.exe", 200)
# appNew: 100 events today, no history -> first seen
ins_agg_daily(conn, d_today, "app:appNew.exe", 100)
conn.commit(); conn.close()

# DB5b: no agg_daily at all -> events fallback for new_app_surge
conn, p5b = make_db("db5b_surge_events.db")
rows = []
t = to_utc(today.replace(hour=10, minute=0))
for k in range(120):
    rows.append((utc_rfc(t + timedelta(seconds=k)), "keyboard", "press", None, "brandnew.exe", "BN"))
ins_events(conn, rows); conn.close()

# ---------------------------------------------------------------- DB6: report focus block >4h
conn, p6 = make_db("db6_focus.db")
rows = []
t0 = to_utc(today.replace(hour=8, minute=0))
rows.append((utc_rfc(t0), "window", "switch", None, "ide.exe", "IDE"))
rows.append((utc_rfc(t0 + timedelta(hours=5)), "window", "switch", None, "browser.exe", "Web"))
rows.append((utc_rfc(t0 + timedelta(hours=5, minutes=30)), "window", "switch", None, "ide.exe", "IDE2"))
ins_events(conn, rows); conn.close()

print("DONE")
for p in [p1, p1b, p2, p2b, p2c, p3, p3b, p4, p5, p5b, p6]:
    print(p)

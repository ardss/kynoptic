import json, subprocess, time, os, sys

EXE = r"K:\kynoptic\target\release\kynoptic.exe"
env = dict(os.environ)
env["KYNOPTIC_DB"] = r"C:\Users\13397\AppData\Local\Programs\Kynoptic\data\kynoptic.db"

p = subprocess.Popen([EXE, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.PIPE, env=env, text=True, encoding="utf-8")

_id = 0
def send(method, params=None, note=""):
    global _id
    _id += 1
    msg = {"jsonrpc": "2.0", "id": _id, "method": method}
    if params is not None:
        msg["params"] = params
    t0 = time.time()
    p.stdin.write(json.dumps(msg) + "\n")
    p.stdin.flush()
    line = p.stdout.readline()
    dt = (time.time() - t0) * 1000
    try:
        resp = json.loads(line)
    except Exception:
        print(f"[{method}] PARSE FAIL raw={line[:300]!r} ({dt:.0f}ms) {note}")
        return None, dt
    return resp, dt

def shape(v, depth=0):
    if isinstance(v, dict):
        return {k: shape(x, depth+1) for k, x in v.items()} if depth < 3 else "..."
    if isinstance(v, list):
        return [shape(v[0], depth+1), f"...len={len(v)}"] if v else []
    return type(v).__name__

# 1. initialize handshake
r, dt = send("initialize", {"protocolVersion": "2024-11-05",
    "capabilities": {}, "clientInfo": {"name": "e2e-probe", "version": "1.0"}})
print(f"== initialize ({dt:.0f}ms)"); print(json.dumps(r, ensure_ascii=False, indent=1)[:900])

# notification (no id) — initialized
p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n"); p.stdin.flush()

r, dt = send("tools/list")
print(f"\n== tools/list ({dt:.0f}ms)")
for t in r.get("result", {}).get("tools", []):
    print(" -", t["name"], "| schema:", json.dumps(t.get("inputSchema", {}), ensure_ascii=False)[:200])

def call(name, args, note=""):
    r, dt = send("tools/call", {"name": name, "arguments": args}, note)
    txt = json.dumps(r, ensure_ascii=False)
    print(f"\n== tools/call {name} {args} ({dt:.0f}ms) isError={r.get('result',{}).get('isError') if r and 'result' in r else 'ERR'}")
    print("shape:", json.dumps(shape(r), ensure_ascii=False)[:500])
    if "result" in r:
        content = r["result"].get("content", [])
        print("text[0:300]:", (content[0].get("text","")[:300] if content else "(empty)"))
    else:
        print("resp:", txt[:300])
    return r

call("get_current_status", {})
call("get_summary", {"days": 1})
call("get_timeline", {"date": None} if False else {})
call("get_top_apps", {"days": 7})
call("get_anomalies", {})
call("wait_for", {"event_type": "this_will_never_come", "timeout_seconds": 2})
# illegal args
call("get_summary", {"days": "notanumber"})
call("no_such_tool", {})
call("get_summary", {"days": -5})

p.stdin.close()
rc = p.wait(timeout=10)
err = p.stderr.read()
print(f"\n== server exit code: {rc}")
print("stderr tail:", err[-500:])

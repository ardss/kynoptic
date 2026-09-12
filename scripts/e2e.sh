#!/usr/bin/env bash
# ══════════════════════════════════════════════════════════════════
# kynoptic 标准化端到端测试（真浏览器 + 后端铁律）
# 用法: bash scripts/e2e.sh [dashboard_url]
#   默认 http://127.0.0.1:8422（本机托盘）
# 前置: agent-browser CLI 已安装；dashboard 正在运行
# 输出: 逐项 PASS/FAIL，任何 FAIL 则退出码 1
# ══════════════════════════════════════════════════════════════════
set -u
BASE="${1:-http://127.0.0.1:8422}"
SESSION="e2e-$$"
PASS=0; FAIL=0; FAILED_ITEMS=()

ok()   { PASS=$((PASS+1)); echo "  PASS  $1"; }
bad()  { FAIL=$((FAIL+1)); FAILED_ITEMS+=("$1"); echo "  FAIL  $1"; }
check(){ if [ "$2" = "1" ]; then ok "$1"; else bad "$1 (got: $3)"; fi; }

# eval helper: 在页面上下文执行 JS 并返回 JSON/文本
ev() { agent-browser --session "$SESSION" eval "$1" 2>/dev/null | tail -1; }

echo "══ kynoptic E2E @ $BASE ══"

# ── A. 后端接口 ──────────────────────────────────────────
echo "── A. 后端接口"
A1=$(curl -s --noproxy '*' -o /dev/null -w '%{http_code}' --max-time 8 "$BASE/")
check "A1 首页 200" "$([ "$A1" = "200" ] && echo 1 || echo 0)" "$A1"

OV=$(curl -s --noproxy '*' --max-time 8 "$BASE/api/overview")
A2=$(echo "$OV" | python -c "
import json,sys
try:
    o=json.load(sys.stdin)
    pm=o.get('presence_minutes'); am=o.get('automation_minutes'); fd=o.get('fg_dwell_min')
    print(1 if all(isinstance(x,int) and x>=0 for x in [pm,am,fd]) else 0)
except Exception: print(0)")
check "A2 /api/overview 三指标为非负整数" "$A2" "$OV"

A3=$(curl -s --noproxy '*' --max-time 8 "$BASE/api/summary" | python -c "
import json,sys
try:
    d=json.load(sys.stdin)
    print(1 if all(isinstance(d.get(k),int) and d.get(k)>=0 for k in ['active_minutes','keys','clicks']) else 0)
except Exception: print(0)")
check "A3 /api/summary keys/clicks/active 非负整数" "$A3"

A4=$(curl -s --noproxy '*' --max-time 8 "$BASE/api/insights" | python -c "
import json,sys
d=json.load(sys.stdin)
print(1 if isinstance(d.get('insights'),list) else 0)")
check "A4 /api/insights 返回列表（可为空）" "$A4"

# ── B. 真浏览器 UI ────────────────────────────────────────
echo "── B. 真浏览器 UI"
agent-browser close >/dev/null 2>&1
sleep 2
agent-browser --session "$SESSION" open "$BASE/" >/dev/null 2>&1
sleep 4
B1=$(ev "JSON.stringify({errs: window.__e2e_errors||[], presence: (document.getElementById('c-presence')||{textContent:''}).textContent})")
B1_OK=$(echo "$B1" | python -c "
import json,sys
try:
    d=json.loads(json.loads(sys.stdin.read()))
    print(1 if d['presence'] not in ('','–','loading…') else 0)
except Exception: print(0)")
check "B1 总览-人在场数字已渲染" "$B1_OK" "$B1"

B2=$(ev "JSON.stringify({a:(document.getElementById('c-automation')||{textContent:''}).textContent, f:(document.getElementById('c-fgdwell')||{textContent:''}).textContent, t:(document.getElementById('c-first')||{textContent:''}).textContent})")
B2_OK=$(echo "$B2" | python -c "
import json,sys
try:
    d=json.loads(json.loads(sys.stdin.read()))
    print(1 if all(v not in ('','–') for v in d.values()) else 0)
except Exception: print(0)")
check "B2 总览-自动化/前台/首次在场已渲染" "$B2_OK" "$B2"

B3=$(ev "document.querySelectorAll('#timeline span[title]').length")
B3_OK=$(echo "$B3" | python -c "import sys; print(1 if int(sys.stdin.read().strip() or 0) >= 5 else 0)")
check "B3 总览-24h 时间线条目 >= 5" "$B3_OK" "$B3"

# 中英切换
ev "applyLang('en')"
B4=$(ev "(document.querySelector('#tab-insights .l.en')||{textContent:''}).textContent")
B4_OK=$(echo "$B4" | python -c "import json,sys; print(1 if json.loads(sys.stdin.read()) == 'Insights' else 0)")
check "B4 语言切换 EN 生效" "$B4_OK" "$B4"
ev "applyLang('zh')"

# 报告页
ev "showView('report')"
sleep 2
B5=$(ev "JSON.stringify({goal:(document.getElementById('r-goal')||{textContent:''}).textContent, hm: document.querySelectorAll('#heatmap rect').length, trend: document.querySelectorAll('#report-trend rect').length})")
B5_OK=$(echo "$B5" | python -c "
import json,sys
try:
    d=json.loads(json.loads(sys.stdin.read()))
    print(1 if d['goal'] and d['hm']>0 and d['trend']>0 else 0)
except Exception: print(0)")
check "B5 报告-目标/热力图/趋势渲染" "$B5_OK" "$B5"

# 应用页
ev "showView('apps')"; sleep 2
B6=$(ev "document.querySelectorAll('#top-apps .app-row').length")
B6_OK=$(echo "$B6" | python -c "import sys; print(1 if int(sys.stdin.read().strip() or 0) >= 3 else 0)")
check "B6 应用-7天 Top 榜 >= 3 行" "$B6_OK" "$B6"

# 输入页 + 悬停一致性（左键 = clicks_left）
ev "showView('input')"; sleep 2
B7=$(ev "
(() => {
  const d = window.__e2e_input || null;
  const titles = [];
  document.querySelectorAll('#view-input svg rect title').forEach(t => titles.push(t.textContent));
  const left = titles.find(t => t.startsWith('Left'));
  const total = (document.getElementById('i-clicks')||{textContent:''}).textContent;
  return JSON.stringify({left, total});
})()")
B7_VAL=$(ev "JSON.stringify({left: window.__e2e_left || null})")
# 左键悬停值与 API 一致性：直接比对 API
API_LEFT=$(curl -s --noproxy '*' --max-time 8 "$BASE/api/input?days=7" | python -c "
import json,sys
print(json.load(sys.stdin).get('clicks_left',-1))")
B7_OK=$(echo "$B7" | python -c "
import json,sys
try:
    d=json.loads(json.loads(sys.stdin.read()))
    print(1 if ('Left' in d.get('left','') and d['left'].split(': ')[1] != '0') or d.get('left','').endswith(': 0') else 0)
except Exception: print(0)")
check "B7 输入-鼠标图左键悬停有值" "$B7_OK" "$B7"
if [ "${API_LEFT:-0}" -gt 0 ]; then
    LEFT_TITLE=$(ev "
(() => {
  const titles = [];
  document.querySelectorAll('#view-input svg rect title').forEach(t => titles.push(t.textContent));
  return titles.find(t => t.startsWith('Left')) || '';
})()")
    case "$LEFT_TITLE" in
        *": 0") bad "B8 左键悬停值与 API 一致（API=$API_LEFT 但 UI=0）";;
        *": "*) ok "B8 左键悬停值与 API 一致（$LEFT_TITLE）";;
        *)      bad "B8 左键悬停值无法读取（$LEFT_TITLE）";;
    esac
fi

# 洞察页
ev "showView('insights')"; sleep 2
B9=$(ev "JSON.stringify({cards: document.querySelectorAll('#insights-list .card').length, note: (document.getElementById('insights-list')||{textContent:''}).textContent.includes('统计规律')})")
B9_OK=$(echo "$B9" | python -c "
import json,sys
try:
    d=json.loads(json.loads(sys.stdin.read()))
    print(1 if d['cards'] >= 1 or d['note'] else 0)
except Exception: print(0)")
check "B9 洞察-卡片或空状态提示" "$B9_OK" "$B9"

# 设置页（硬件面板 + 桥接阈值）
ev "showView('settings')"; sleep 5
B10=$(ev "JSON.stringify({model:(document.getElementById('h-model')||{textContent:'–'}).textContent, bridge:(document.getElementById('set-bridge')||{value:'-1'}).value})")
B10_OK=$(echo "$B10" | python -c "
import json,sys
try:
    d=json.loads(json.loads(sys.stdin.read()))
    print(1 if d['model'] not in ('–','') and 0 <= int(d['bridge']) <= 15 else 0)
except Exception: print(0)")
check "B10 设置-硬件面板+桥接阈值" "$B10_OK" "$B10"

# ── C. 铁律与命令行（临时副本库，绝不碰真实数据）─────────
echo "── C. 铁律（临时副本库）"
REAL_DB="D:/Kynoptic/data/kynoptic.db"
TMPD="$(mktemp -d)"
TMPDB="$TMPD/e2e-copy.db"
BIN="/d/Kynoptic/kynoptic.exe"
if [ -f "$REAL_DB" ] && [ -f "$BIN" ]; then
    cp "$REAL_DB" "$TMPDB"
    count_events() {
        local wpath; wpath="$(cygpath -w "$1")"
        python - "$wpath" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print(c.execute("SELECT COUNT(*) FROM events").fetchone()[0])
PY
    }
    BEFORE=$(count_events "$TMPDB")
    # 1) cleanup 0 不得删除任何原始事件（铁律）
    R1=$(KYNOPTIC_DB="$(cygpath -w "$TMPDB")" "$BIN" db cleanup 0 2>&1 | tail -1)
    AFTER1=$(count_events "$TMPDB")
    check "C1 cleanup 0 不删原始事件" "$([ "$BEFORE" = "$AFTER1" ] && [ -n "$BEFORE" ] && echo 1 || echo 0)" "$BEFORE->$AFTER1"
    # 2) cleanup --yes 且 days<30 必须被拒绝
    R2=$(KYNOPTIC_DB="$(cygpath -w "$TMPDB")" "$BIN" db cleanup 7 --yes 2>&1 | tail -1)
    check "C2 cleanup 7 --yes 被拒绝（<30 天保护）" "$(echo "$R2" | grep -q "拒绝" && echo 1 || echo 0)" "$R2"
    # 3) cleanup abc 必须报错而非静默
    R3=$(KYNOPTIC_DB="$(cygpath -w "$TMPDB")" "$BIN" db cleanup abc 2>&1 | tail -1)
    check "C3 cleanup abc 报错而非静默" "$(echo "$R3" | grep -q "无效天数" && echo 1 || echo 0)" "$R3"
    rm -rf "$TMPD"
else
    echo "  SKIP  C 段（未找到真实库或 CLI）"
fi

# ── 汇总 ────────────────────────────────────────────────
echo "══ 结果: $PASS PASS / $FAIL FAIL ══"
if [ "$FAIL" -gt 0 ]; then
    printf '  失败项: %s\n' "${FAILED_ITEMS[@]}"
    exit 1
fi
exit 0

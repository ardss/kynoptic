// 仪表盘内嵌 JS 语法检查（CI 用）：逐 <script> 块过 node 语法器。
// 背景：面板曾有重复 const 声明导致整页死掉多轮没人发现——cargo test 只
// 断言 HTML 含字符串，不执行 JS。用法：
//   node scripts/check-js.mjs [html路径，默认 crates/dash/src/dashboard.html]
import { readFileSync } from "node:fs";
import vm from "node:vm";

const htmlPath = process.argv[2] ?? "crates/dash/src/dashboard.html";
const html = readFileSync(htmlPath, "utf8");
const re = /<script\b[^>]*>([\s\S]*?)<\/script>/g;
const blocks = [...html.matchAll(re)];
if (blocks.length === 0) {
  console.error(`no <script> blocks found in ${htmlPath}`);
  process.exit(1);
}
for (const [i, m] of blocks.entries()) {
  try {
    new vm.Script(m[1], { filename: `${htmlPath}#script${i}` });
  } catch (e) {
    console.error(`script block ${i} failed to parse: ${e.message}`);
    process.exit(1);
  }
}
console.log(`dashboard JS OK (${blocks.length} script block(s))`);

// ── 逻辑断言扩展（条件执行）：被断言的渲染函数随 UI 版本演进可能增删，
// 函数集齐全才执行对应断言组，缺失则跳过（语法门禁恒执行，不受影响）。
function extractFn(name) {
  const m = html.match(new RegExp(`function ${name}\\([\\s\\S]*?\\n}`));
  return m ? m[0] : null;
}
const assertOrFail = (cond, msg) => {
  if (!cond) {
    console.error(`assert FAILED: ${msg}`);
    process.exit(1);
  }
};

// 网络归因徽标渲染断言（netQualityBadge / netSectionRow）
if (extractFn("netQualityBadge") && extractFn("netSectionRow") && extractFn("esc")) {
  const sandbox = {};
  vm.createContext(sandbox);
  new vm.Script(
    extractFn("esc") + "\n" + extractFn("netQualityBadge") + "\n" + extractFn("netSectionRow"),
    { filename: `${htmlPath}#netbadge` },
  ).runInContext(sandbox);
  const b = sandbox.netQualityBadge;
  assertOrFail(b("confirmed").cls === "q-confirmed", "confirmed 徽标必须绿档");
  assertOrFail(b("estimated").cls === "q-estimated", "estimated 徽标必须琥珀档");
  assertOrFail(b("incomplete").cls === "q-incomplete", "incomplete 徽标必须次档");
  assertOrFail(b("unavailable").cls === "q-unavailable", "unavailable 徽标必须灰档");
  assertOrFail(b(null).cls === "q-unavailable" && b(undefined).cls === "q-unavailable" &&
    b("weird").cls === "q-unavailable", "未知 quality 一律灰显（不可信值不猜档）");
  const row = sandbox.netSectionRow(
    { key: "plan", label_en: "plan usage", label_zh: "套餐用量", bytes: null,
      quality: "unavailable", note: "本机无可信套餐计量面" },
    "zh");
  assertOrFail(row.includes("q-unavailable") && row.includes("q-gray"), "unavailable 行必须灰显");
  assertOrFail(row.includes("套餐用量") && row.includes("不可用"), "unavailable 行必须带徽标文案");
  assertOrFail(row.includes("本机无可信套餐计量面"), "unavailable 行必须带原因文案");
  assertOrFail(!row.includes("null"), "bytes=null 不得泄漏为字符串 null");
  const rowOk = sandbox.netSectionRow(
    { key: "proxy_outer", label_en: "proxy outer", label_zh: "代理外层", bytes: 3000,
      quality: "confirmed", note: null }, "en");
  assertOrFail(rowOk.includes("q-confirmed") && !rowOk.includes("q-gray"), "confirmed 行不灰显");
  assertOrFail(rowOk.includes("3,000 B"), "字节值须本地化格式化");
  console.log("net badge logic OK (badge + section row assertions)");
} else {
  console.log("net badge logic: functions absent in this UI version, skipped");
}

// 诊断视图渲染断言（系统总览卡 / 资源排行行 / 诊断时间线行）
if (extractFn("sysCard") && extractFn("resourceTopRow") && extractFn("diagTimelineRow") &&
    extractFn("netQualityBadge") && extractFn("esc")) {
  const sandbox43 = {};
  vm.createContext(sandbox43);
  new vm.Script(
    extractFn("esc") + "\n" + extractFn("netQualityBadge") + "\n" +
    extractFn("sysCard") + "\n" + extractFn("resourceTopRow") + "\n" + extractFn("diagTimelineRow"),
    { filename: `${htmlPath}#diagviews` },
  ).runInContext(sandbox43);
  const card = sandbox43.sysCard(
    { label_en: "cpu", label_zh: "处理器", quality: "unavailable", value: null,
      note: "无持久化数据源；未采不伪造" }, "zh");
  assertOrFail(card.includes("q-unavailable") && card.includes("q-gray"), "unavailable 卡必须灰显");
  assertOrFail(card.includes("未采不伪造"), "unavailable 卡必须带原因文案");
  const cardOk = sandbox43.sysCard(
    { label_en: "network", label_zh: "网络", quality: "confirmed", value: 4200, unit: "bytes", note: null }, "en");
  assertOrFail(cardOk.includes("q-confirmed") && cardOk.includes("4,200 B"), "confirmed 卡带数值");
  const rrow = sandbox43.resourceTopRow(
    { pid: 9, name: null, total: 5000, quality: "confirmed" }, "zh");
  assertOrFail(rrow.includes("pid 9") && rrow.includes("5,000 B"), "排行行须显示 pid 与字节（无名不伪造）");
  const rrowNamed = sandbox43.resourceTopRow(
    { pid: 7, name: "alpha.exe", total: 3100, quality: "incomplete" }, "en");
  assertOrFail(rrowNamed.includes("alpha.exe") && rrowNamed.includes("q-incomplete"), "有名行显示进程名与徽标");
  const trow = sandbox43.diagTimelineRow(
    { timestamp: "2026-11-02T10:00:00+00:00", type: "window", action: "switch", app: "editor.exe" },
    "zh");
  assertOrFail(trow.includes("2026-11-02T10:00:00+00:00"), "时间线行必须渲染时间");
  assertOrFail(trow.includes("switch") && trow.includes("editor.exe"), "时间线行必须渲染动作与应用");
  const trowDest = sandbox43.diagTimelineRow(
    { timestamp: "2026-11-02T10:00:00+00:00", type: "system", action: "heartbeat", app: null,
      dest: { quality: "unavailable", reason: "ETW 分钟聚合不含远端地址，未采不伪造" } }, "zh");
  assertOrFail(trowDest.includes("未采不伪造"), "时间线行的目的地址不可用须渲染原因");
  assertOrFail(!sandbox43.diagTimelineRow(
    { timestamp: "t", type: "s", action: "a", app: null, dest: null }, "zh").includes("undefined"),
    "缺字段不得泄漏 undefined");
  console.log("diag view logic OK (sys card + resource row + timeline row assertions)");
} else {
  console.log("diag view logic: functions absent in this UI version, skipped");
}

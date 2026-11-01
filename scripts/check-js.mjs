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

// ── M3.4 扩展：网络归因徽标渲染逻辑断言（无 DOM，纯函数 vm 求值） ──
// 从 HTML 提取 netQualityBadge / netSectionRow（连同其依赖 esc），在 vm
// 沙箱求值后跑固定断言向量；此前 check-js 只做语法检查，不执行逻辑。
function extractFn(name) {
      const m = html.match(new RegExp(`function ${name}\\([\\s\\S]*?\\n}`));
  if (!m) {
    console.error(`M3.4 assert: function ${name} not found in ${htmlPath}`);
    process.exit(1);
  }
  return m[0];
}
const sandbox = {};
vm.createContext(sandbox);
new vm.Script(
  extractFn("esc") + "\n" + extractFn("netQualityBadge") + "\n" + extractFn("netSectionRow"),
  { filename: `${htmlPath}#m34` },
).runInContext(sandbox);
const assert = (cond, msg) => {
  if (!cond) {
    console.error(`M3.4 assert FAILED: ${msg}`);
    process.exit(1);
  }
};
const b = sandbox.netQualityBadge;
assert(b("confirmed").cls === "q-confirmed", "confirmed 徽标必须绿档");
assert(b("estimated").cls === "q-estimated", "estimated 徽标必须琥珀档");
assert(b("incomplete").cls === "q-incomplete", "incomplete 徽标必须次档");
assert(b("unavailable").cls === "q-unavailable", "unavailable 徽标必须灰档");
assert(b(null).cls === "q-unavailable" && b(undefined).cls === "q-unavailable" &&
  b("weird").cls === "q-unavailable", "未知 quality 一律灰显（不可信值不猜档）");
const row = sandbox.netSectionRow(
  { key: "plan", label_en: "plan usage", label_zh: "套餐用量", bytes: null,
    quality: "unavailable", note: "本机无可信套餐计量面" },
  "zh");
assert(row.includes("q-unavailable") && row.includes("q-gray"), "unavailable 行必须灰显");
assert(row.includes("套餐用量") && row.includes("不可用"), "unavailable 行必须带徽标文案");
assert(row.includes("本机无可信套餐计量面"), "unavailable 行必须带原因文案");
assert(!row.includes("null"), "bytes=null 不得泄漏为字符串 null");
const rowOk = sandbox.netSectionRow(
  { key: "proxy_outer", label_en: "proxy outer", label_zh: "代理外层", bytes: 3000,
    quality: "confirmed", note: null }, "en");
assert(rowOk.includes("q-confirmed") && !rowOk.includes("q-gray"), "confirmed 行不灰显");
assert(rowOk.includes("3,000 B"), "字节值须本地化格式化");
console.log("M3.4 badge logic OK (badge + section row assertions)");

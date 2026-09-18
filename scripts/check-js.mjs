// 仪表盘内嵌 JS 语法检查（CI 用）：逐 <script> 块过 node 语法器。
// 背景：面板曾有重复 const 声明导致整页死掉多轮没人发现——cargo test 只
// 断言 HTML 含字符串，不执行 JS。用法：
//   node scripts/check-js.mjs [html路径，默认 crates/dash/src/dashboard.html]
import { readFileSync } from "node:fs";
import vm from "node:vm";

const htmlPath = process.argv[2] ?? "crates/dash/src/dashboard.html";
const html = readFileSync(htmlPath, "utf8");
const re = /<script>([\s\S]*?)<\/script>/g;
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

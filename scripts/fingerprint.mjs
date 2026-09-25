// 安装目录指纹参考实现：与 crates/core/src/naming.rs 的 install_fingerprint
// 同算法（小写化、去结尾分隔符后，对 UTF-16 码元做 h=5381 起
// h=(h*33+码元) mod 2^32 的 djb 变体，输出 8 位大写十六进制）。
// 用途：CI 发布门禁真机一致性探针（.github/workflows/release.yml）断言
// 「安装器写入的计划任务名 / Run 值名 == 本脚本按安装目录算出的名」。
// 用法：node scripts/fingerprint.mjs <安装目录>
const dir = process.argv[2];
if (!dir) {
  console.error("usage: node scripts/fingerprint.mjs <dir>");
  process.exit(2);
}
let h = 5381n;
const trimmed = dir.toLowerCase().replace(/[\\/]+$/, "");
for (let i = 0; i < trimmed.length; i++) {
  h = (h * 33n + BigInt(trimmed.charCodeAt(i))) & 0xffffffffn;
}
console.log(h.toString(16).toUpperCase().padStart(8, "0"));

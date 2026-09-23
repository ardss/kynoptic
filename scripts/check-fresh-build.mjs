#!/usr/bin/env node
// ci 修复：陈旧产物防线。「源码已修」不等于「运行态已修」——本轮曾实际
// 发生旧 exe（早于源码修改）打在旧构建上、新加的令牌门探测全部 200。
// 用法：node scripts/check-fresh-build.mjs <binDir> <exe名...>
// 规则：每个待交付 exe 的 mtime 必须不早于源码树（crates/ + 根
// Cargo.toml/Cargo.lock）中最新的文件。cargo 的重建触发条件正是"源码比
// 产物新"，因此本检查失败 ⇒ 先重跑 cargo build 再交付。发版 CI 在全新
// checkout 上构建，必然通过；本脚本主要拦本地/升级打包的陈旧产物。
import { readdir, stat } from "node:fs/promises";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const [binDir, ...exes] = process.argv.slice(2);
if (!binDir || exes.length === 0) {
  console.error("用法: node scripts/check-fresh-build.mjs <binDir> <exe名...>");
  process.exit(2);
}

// 源码树最新 mtime（只扫 crates/，根 manifest；不碰 target/）
async function newestSourceMtime() {
  let newest = 0;
  async function walk(dir) {
    for (const e of await readdir(dir, { withFileTypes: true })) {
      const p = join(dir, e.name);
      if (e.isDirectory()) await walk(p);
      else newest = Math.max(newest, (await stat(p)).mtimeMs);
    }
  }
  await walk(join(root, "crates"));
  for (const f of ["Cargo.toml", "Cargo.lock"]) {
    newest = Math.max(newest, (await stat(join(root, f))).mtimeMs);
  }
  return newest;
}

const newestSrc = await newestSourceMtime();
let failed = false;
for (const name of exes) {
  const p = join(resolve(binDir), name);
  let mtime;
  try {
    mtime = (await stat(p)).mtimeMs;
  } catch {
    console.error(`[stale-build] 缺失产物: ${p}`);
    failed = true;
    continue;
  }
  if (mtime < newestSrc) {
    console.error(
      `[stale-build] 陈旧产物: ${name} (mtime ${new Date(mtime).toISOString()}) 早于最新源码 ` +
      `(${new Date(newestSrc).toISOString()}) —— 先 cargo build 再交付`,
    );
    failed = true;
  } else {
    console.log(`[stale-build] ok: ${name}`);
  }
}
process.exit(failed ? 1 : 0);

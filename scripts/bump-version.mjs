#!/usr/bin/env node
// ci 修复：一键版本号提升脚本。此前版本号散落六处硬编码（Cargo.toml、
// index.html×2、README.md、README.zh-CN.md、APP.md），且 kynoptic.iss 曾
// 持有第二个版本源（缺省值），导致过 0.2.0/0.2.2 漂移事故。现 iss 只认
// /DAppVersion 外部传入，本脚本一次改全源并同步 Cargo.lock。
// 用法：node scripts/bump-version.mjs <新版本号>   例：0.4.0
import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const next = process.argv[2];
if (!/^\d+\.\d+\.\d+(-[\w.]+)?$/.test(next || "")) {
  console.error("用法: node scripts/bump-version.mjs <x.y.z[-pre]>");
  process.exit(1);
}

function read(rel) {
  return readFileSync(join(root, rel), "utf8");
}
function write(rel, content) {
  writeFileSync(join(root, rel), content);
  console.log(`[bump-version] ${rel} -> ${next}`);
}

function replaceOnce(rel, from, to) {
  const src = read(rel);
  if (!src.includes(from)) {
    console.error(`[bump-version] ${rel} 中未找到待替换片段: ${from}`);
    process.exit(1);
  }
  write(rel, src.replace(from, to));
}

function currentCargoVersion() {
  return read("Cargo.toml").match(/^version = "([^"]+)"/m)[1];
}

// 旧版本号在改动 Cargo.toml 前先定格，后续各文件都以它定位待替换片段
const old = currentCargoVersion();

// 1. Cargo.toml（workspace 版本号，全仓唯一真实版本源）
replaceOnce("Cargo.toml", `version = "${old}"`, `version = "${next}"`);

// 2. Cargo.lock 中 kynoptic 自身条目（依赖包不动）
{
  const src = read("Cargo.lock");
  const re = /name = "kynoptic"\r?\nversion = "([^"]+)"/;
  const m = src.match(re);
  if (m) replaceOnce("Cargo.lock", m[0], m[0].replace(m[1], next));
  else console.error("[bump-version] Cargo.lock 未找到 kynoptic 条目，跳过（cargo check 会补）");
}

// 3. 官网首页状态文案（中英各两处）
replaceOnce("index.html", `v${old} released`, `v${next} released`);
replaceOnce("index.html", `v${old} 已发布</span>`, `v${next} 已发布</span>`);
replaceOnce("index.html", `v${old} is out`, `v${next} is out`);
replaceOnce("index.html", `v${old} 已发布，直接下载`, `v${next} 已发布，直接下载`);

// 4. 双语 README 状态行
replaceOnce("README.md", `Status: v${old}`, `Status: v${next}`);
replaceOnce("README.zh-CN.md", `状态：v${old}`, `状态：v${next}`);

// 5. APP.md 标题版本号
replaceOnce("APP.md", `# Kynoptic App (v${old})`, `# Kynoptic App (v${next})`);

console.log(`[bump-version] 完成。记得更新 CHANGELOG.md 并用 /DAppVersion=${next} 打包。`);

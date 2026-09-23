#!/usr/bin/env node
// ci 修复：dist 版本一致性防线。曾实际发生 Cargo.toml 已到 0.3.0、
// dist/ 里仍是 0.2.x 安装包与 exe 的情况——新用户按发布页拿到旧面板，
// 缺少新版全部修复。用法：node scripts/check-dist-version.mjs <distDir>
// 规则：
//   1. dist/kynoptic-ctl.exe --version 输出必须等于 Cargo.toml 的版本；
//   2. dist/Kynoptic-Setup-<版本>.exe 安装包必须存在；
//   3. dist 内不得残留低于当前版本的旧安装包。
// 发版 CI 在 Stage assets 之后调用；本地刷新 dist 后也应自检。
import { execFileSync } from "node:child_process";
import { readdirSync, existsSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const dist = resolve(process.argv[2] || join(root, "dist"));

function cargoVersion() {
  const meta = JSON.parse(
    execFileSync("cargo", ["metadata", "--no-deps", "--format-version", "1"], {
      cwd: root, encoding: "utf8",
    }),
  );
  return meta.packages.find((p) => p.name === "kynoptic").version;
}

function ctlVersion() {
  const out = execFileSync(join(dist, "kynoptic-ctl.exe"), ["--version"], {
    encoding: "utf8",
  }).trim();
  // 期望形如「kynoptic 0.3.0」
  return out.split(/\s+/).pop();
}

function cmpVer(a, b) {
  const pa = a.split(".").map(Number);
  const pb = b.split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) - (pb[i] || 0);
  }
  return 0;
}

let failed = false;
const ver = cargoVersion();
console.log(`[dist-version] Cargo.toml 版本: ${ver}`);

const ctlExe = join(dist, "kynoptic-ctl.exe");
if (!existsSync(ctlExe)) {
  console.error(`[dist-version] 缺失 ${ctlExe}`);
  failed = true;
} else {
  const dv = ctlVersion();
  if (dv !== ver) {
    console.error(
      `[dist-version] dist/kynoptic-ctl.exe 版本 ${dv} ≠ Cargo.toml 版本 ${ver} —— 先刷新 dist 产物再发布`,
    );
    failed = true;
  } else {
    console.log(`[dist-version] ok: dist/kynoptic-ctl.exe == ${ver}`);
  }
}

const files = existsSync(dist) ? readdirSync(dist) : [];
const setup = files.find((f) => f === `Kynoptic-Setup-${ver}.exe`);
if (!setup) {
  console.error(`[dist-version] 缺失安装包 Kynoptic-Setup-${ver}.exe`);
  failed = true;
} else {
  console.log(`[dist-version] ok: ${setup}`);
}

const stale = files.filter(
  (f) => /^Kynoptic-Setup-[\d.]+\.exe$/.test(f) && cmpVer(f.match(/[\d.]+/)[0], ver) < 0,
);
if (stale.length > 0) {
  console.error(`[dist-version] 残留旧安装包: ${stale.join(", ")} —— 请清理后重新打包`);
  failed = true;
}

process.exit(failed ? 1 : 0);

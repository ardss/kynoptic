#!/usr/bin/env node
// ci 修复（Wave46）：文档版本行一致性断言。版本一致性工具 bump-version.mjs
// 曾只护七处中的六处，漏 llms.txt——0.3.1 发版时其状态行漂移在 v0.3.0，
// 事后人肉补正（CHANGELOG.md:269-272 有同型事故记录）。本脚本只锚定
// 头部/状态行模式，不做全文件扫描：APP.md:103/111 的「截至 v0.3.1」
// 历史正文属有意不动，全文件扫描会误报。
// 断言：llms.txt / index.html / README / README.zh-CN / APP.md 的
// 状态行与 CHANGELOG.md 顶部版本段均须与 Cargo.toml 当前版本一致
// （头部/状态行各只含一个版本 token，一致即旧版不残留）。
// 调用方：scripts/release-gate.ps1 G4 门（发布日本地）与
// .github/workflows/release.yml 构建前步骤；不一致 exit 1 终止。
import { readFileSync, existsSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

function cargoVersion() {
  // 与 bump-version.mjs 同源：根 Cargo.toml 的 workspace 版本（全仓唯一真实版本源）
  return readFileSync(join(root, "Cargo.toml"), "utf8").match(
    /^version = "([^"]+)"/m,
  )[1];
}

const ver = cargoVersion();
console.log(`[doc-version] Cargo.toml 版本: ${ver}`);

let failed = false;

// 头部/状态行锚定断言：指定文件的头部/状态行必须含 needle，缺文件即失败
function requireContains(rel, needle, label) {
  const p = join(root, rel);
  if (!existsSync(p)) {
    console.error(`[doc-version] 缺失 ${rel}`);
    failed = true;
    return;
  }
  const src = readFileSync(p, "utf8");
  if (!src.includes(needle)) {
    console.error(
      `[doc-version] ${label}：${rel} 未含 '${needle}'（头部/状态行仍残留旧版或未随版本更新）`,
    );
    failed = true;
  } else {
    console.log(`[doc-version] ok: ${label} ${rel}`);
  }
}

// llms.txt 状态行
requireContains("llms.txt", `Status: v${ver}`, "状态行");
// index.html 状态文案四处（与 bump-version.mjs 第 3 步镜像）
requireContains("index.html", `v${ver} released`, "英文状态文案");
requireContains("index.html", `v${ver} 已发布</span>`, "中文状态文案");
requireContains("index.html", `v${ver} is out`, "英文下载标题");
requireContains("index.html", `v${ver} 已发布，直接下载`, "中文下载标题");
// 双语 README 状态行
requireContains("README.md", `Status: v${ver}`, "状态行");
requireContains("README.zh-CN.md", `状态：v${ver}`, "状态行");
// APP.md 标题版本号
requireContains("APP.md", `# Kynoptic App (v${ver})`, "标题行");
// CHANGELOG.md 顶部版本段：首个带日期的 '## [x.y.z] - ' 段必须即当前版本
// （锚定顶部段，历史段不动；顶段停旧版 = 发版时忘记立新版段）
{
  const p = join(root, "CHANGELOG.md");
  if (!existsSync(p)) {
    console.error("[doc-version] 缺失 CHANGELOG.md");
    failed = true;
  } else {
    const src = readFileSync(p, "utf8");
    const heads = [...src.matchAll(/^## \[([^\]]+)\] - /gm)].map((m) => m[1]);
    if (heads.length === 0 || heads[0] !== ver) {
      console.error(
        `[doc-version] CHANGELOG.md 顶部版本段须为 '## [${ver}] - …'（实际 '${
          heads[0] ?? "(无)"
        }'），顶段未随版本更新`,
      );
      failed = true;
    } else {
      console.log(`[doc-version] ok: CHANGELOG.md 顶部版本段 ${heads[0]}`);
    }
  }
}

process.exit(failed ? 1 : 0);

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
// 断言清单第 10 项（W47）：SECURITY.md 支持表须含当前 minor 线——此前版本
// 工具从不覆盖该文件，0.3.1 升到 1.0.0 时「0.3.x | Yes / pre-1.0」会静默
// 失真且门禁放行（盲区已实测证伪）。同样只锚定支持表首行，不做全文件扫描。
// 定版操作序列（此前只靠门禁失败信息现场教学，无文档记载，纯改名字段会
// 被下方断言拒绝）：CHANGELOG.md 定版须同时做三件事——
// 1) 顶部段 '## [Unreleased]' 改名 '## [x.y.z] - 日期'；
// 2) 顶部重开一个空 '## [Unreleased]' 段；
// 3) 底部链接区新增 '[x.y.z]: .../compare/v<上版>...vx.y.z'，并把
//    '[Unreleased]' 基线从旧版前移到 'compare/vx.y.z...HEAD'（漏改悬空）。
// skill 副本防漂移（ci-low-3）：本机 ~/.zcode|.claude|.cursor 的
// skills/kynoptic/SKILL.md 与仓库内置 crates/cli/src/assets/skill.md 逐字节
// 一致（三者是 main.rs SKILL_CLIENT_DIRS 的同步目标）；本机无副本时跳过。
import { readFileSync, existsSync } from "node:fs";
import { homedir } from "node:os";
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
    // 底部链接定义区：[Unreleased] compare 基线必须等于当前版本
    // （定版时若漏改 v0.3.1...HEAD → 悬空链接，此前十项断言全过门禁放行，
    // 见 fixReport ci-low-1 实测记录。只锚定该行，不做全文件扫描。）
    const unrel = `[Unreleased]: https://github.com/ardss/kynoptic/compare/v${ver}...HEAD`;
    if (!src.includes(unrel)) {
      console.error(
        `[doc-version] CHANGELOG.md 底部 [Unreleased] compare 基线须为 '${unrel}'（漏改将悬空）`,
      );
      failed = true;
    } else {
      console.log(`[doc-version] ok: CHANGELOG.md [Unreleased] 基线 v${ver}`);
    }
  }
}

// SECURITY.md 支持线：「## Supported versions」段的支持表须含当前 minor 线
// 且标记 Yes（minor 线 = 取 Cargo.toml 版本前两段，0.3.1 → 0.3.x）
{
  const p = join(root, "SECURITY.md");
  if (!existsSync(p)) {
    console.error("[doc-version] 缺失 SECURITY.md");
    failed = true;
  } else {
    const src = readFileSync(p, "utf8");
    const minor = ver.split(".").slice(0, 2).join(".");
    // CRLF 宽容：Windows 工作树行尾是 \r\n。段边界只认下一个标题行或文末——
    // 勿用 m 标志下的 $ 做备选（会在首个换行处提前截断捕获段）
    const sec = src.match(
      /## Supported versions\r?\n([\s\S]*?)(?=\r?\n##[ \t]|\s*$)/,
    );
    const rowRe = new RegExp(
      "^\\|\\s*" + minor.replace(/\./g, "\\.") + "\\.x\\s*\\|\\s*Yes\\s*\\|",
      "m",
    );
    if (!sec || !rowRe.test(sec[1])) {
      console.error(
        `[doc-version] SECURITY.md 支持表须含 '| ${minor}.x | Yes |'（当前版本 ${ver}），支持线未随版本更新`,
      );
      failed = true;
    } else {
      console.log(`[doc-version] ok: SECURITY.md 支持线 ${minor}.x`);
    }
  }
}

// 本机 skill 副本防漂移：存在才比（CI runner 上通常没有，静默跳过）
{
  const asset = join(root, "crates", "cli", "src", "assets", "skill.md");
  if (existsSync(asset)) {
    const want = readFileSync(asset);
    for (const dir of [".zcode", ".claude", ".cursor"]) {
      const copy = join(homedir(), dir, "skills", "kynoptic", "SKILL.md");
      if (!existsSync(copy)) continue;
      if (readFileSync(copy).equals(want)) {
        console.log(`[doc-version] ok: skill 副本一致 ${dir}/skills/kynoptic/SKILL.md`);
      } else {
        console.error(
          `[doc-version] 本机 skill 副本漂移：${copy} 与 crates/cli/src/assets/skill.md 不一致，重跑 kynoptic skill install 同步`,
        );
        failed = true;
      }
    }
  }
}

process.exit(failed ? 1 : 0);

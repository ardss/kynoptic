# release-gate.ps1 — 发布提交前置门禁（CONTRIBUTING.md 第 3 节第 3 步前执行）
#
# 背景（Wave45 缺陷固化，详见 scripts/E2E-README.md 同域记录）：
#   G1 仓库根曾出现字面文件 `nul`（Windows 保留设备名，POSIX shell 误重定向
#     `> nul` 且环境变量未展开而生成）。git 经 WinAPI 读该名命中 0 字节 NUL
#     设备而非磁盘文件，任何 `git add`（含 -An 干跑）直接 fatal：
#     "error: short read while indexing nul / fatal: adding files failed"。
#     本门用 `git add -An` 干跑（不写索引）做可索引性探测——这是受控复现
#     证实过的检测器（无 nul rc=0 / 有 nul rc=128 / 删后 rc=0 三轮）。
#     K: 为远程卷，UNC 前缀的 Win32 删除不适用；清除手段为 MSYS/Git Bash
#     `rm /k/kynoptic/nul`（cmd 内置 del/ren 不支持保留设备名）。
#   G2 并发代理遗留的"用完即删"临时文件（如 crates/core 下 examples/tests
#     临时文件）会 (a) 被 `git add -A` 扫入发布提交造成污染，(b) 使
#     `cargo fmt --all -- --check` 失败。本门要求发布前无未跟踪残留。
#   G3 与 ci.yml 第 22 行同形的 workspace 级 fmt 门（未跟踪源码文件同样卡它）。
#
# 用法：
#   pwsh scripts/release-gate.ps1            # 只查（发布日前跑）
#   pwsh scripts/release-gate.ps1 -Stage      # 查全部门通过后，显式 add 发布文件
#   显式 add 铁律：永远不要用 `git add -A`（会把未跟踪残留扫进发布提交）。
#   -Stage 后按 `release x.y.z: <summary>` 提交（Conventional Commits）。

param(
  [switch]$Stage,
  [string]$RepoRoot = ''
)

$ErrorActionPreference = 'Stop'
if (-not $RepoRoot) {
  # 默认取脚本所在仓库根（<repo>/scripts/ 的上层目录），不依赖调用方 cwd
  Push-Location $PSScriptRoot
  $RepoRoot = & git rev-parse --show-toplevel
  Pop-Location
  if ($LASTEXITCODE -ne 0) { Write-Error "定位仓库根失败（git rev-parse --show-toplevel）"; exit 1 }
}
Set-Location $RepoRoot
Write-Host "== release-gate: $RepoRoot"

# G1 可索引性门：`git add -An` 干跑（-n 不写索引）。
# 字面 nul 文件在此报 fatal（rc=128）；任何其它索引故障也在此拦截。
git add -An > $null 2>&1
if ($LASTEXITCODE -ne 0) {
  Write-Host "[FAIL] G1 索引门：git add -An rc=$LASTEXITCODE（干跑即 fatal）"
  Write-Host "  典型成因：仓库根存在字面 nul 设备名文件（POSIX shell 误重定向 > nul 生成）。"
  Write-Host "  清除：MSYS/Git Bash 执行 rm /k/kynoptic/nul（cmd del/ren 不支持保留设备名；"
  Write-Host "        K: 远程卷上 UNC 前缀 Win32 删除不适用）。"
  Write-Host "  防复发：禁止在仓库根以 POSIX shell 重定向到 nul（变量未展开即建字面文件）。"
  exit 1
}
Write-Host "[PASS] G1 索引门：git add -An rc=0"

# G2 无未跟踪残留门：`??` 行即代理临时文件/误产物，必须先用完即删。
# 注意：-like 中 ? 是通配符，须用 [?] 转义成字面量
$status = & git status --porcelain
if ($LASTEXITCODE -ne 0) { Write-Error "git status 失败"; exit 1 }
$untracked = @($status | Where-Object { $_ -like '[?][?] *' })
if ($untracked.Count -gt 0) {
  Write-Host "[FAIL] G2 残留门：存在 $($untracked.Count) 个未跟踪文件（禁止进发布提交）："
  $untracked | ForEach-Object { Write-Host "  $_" }
  Write-Host "  处置：代理临时文件用完即删；确认无需保留后逐个删除再重跑本门。"
  exit 1
}
Write-Host "[PASS] G2 残留门：无未跟踪文件"

# G3 fmt 门：与 ci.yml 第 21-22 行同形（未跟踪的 examples/tests 临时文件也卡它）
if (Test-Path (Join-Path $RepoRoot 'Cargo.toml')) {
  $null = & cargo fmt --all -- --check
  if ($LASTEXITCODE -ne 0) {
    Write-Host "[FAIL] G3 fmt 门：cargo fmt --all -- --check rc=$LASTEXITCODE"
    exit 1
  }
  Write-Host "[PASS] G3 fmt 门：cargo fmt --all -- --check rc=0"
} else {
  Write-Host "[SKIP] G3 fmt 门：$RepoRoot 无 Cargo.toml（沙箱/纯 git 仓库）"
}

# 显式 add 清单：发布提交只含版本文件（CONTRIBUTING.md 第 3 节第 1-2 步），
# 逐一显式 add，绝不 `git add -A`。
$releaseFiles = @('Cargo.toml', 'Cargo.lock', 'CHANGELOG.md')
if (-not $Stage) {
  Write-Host "== 门禁全部通过。发布日提交序列（显式 add，禁 git add -A）："
  Write-Host "   git add $($releaseFiles -join ' ')"
  Write-Host "   git commit -m 'release x.y.z: 摘要'（Conventional Commits，见 CONTRIBUTING 第 2 节）"
  Write-Host "   （先完成第 3 节第 3 步的 cargo test / clippy 全绿）"
  exit 0
}

# -Stage：门禁全过后显式 add 发布文件，再复核工作区无旁路内容
git add $releaseFiles
if ($LASTEXITCODE -ne 0) { Write-Error "git add $($releaseFiles -join ' ') 失败"; exit 1 }
$after = & git status --porcelain
$leftover = @($after | Where-Object { $_ -notlike 'M  *' -and $_ -notlike 'A  *' })
if ($leftover.Count -gt 0) {
  Write-Host "[FAIL] 复核：暂存后仍有未纳入的改动（发布提交必须单一逻辑变更）："
  $leftover | ForEach-Object { Write-Host "  $_" }
  Write-Host "  处置：无关改动先另行提交/撤出，再重跑 -Stage。"
  exit 1
}
# --quiet：有暂存差异 rc=1 / 无差异 rc=0（括号表达式只取输出，必须显式查 $LASTEXITCODE）
git diff --cached --quiet
if ($LASTEXITCODE -eq 0) {
  Write-Host "[FAIL] 复核：发布文件均无变更（先做第 3 节第 1-2 步的版本与 CHANGELOG 更新）。"
  exit 1
}
Write-Host "[PASS] 复核：仅 $($releaseFiles -join ' / ') 已暂存，可直接 commit + tag"
exit 0

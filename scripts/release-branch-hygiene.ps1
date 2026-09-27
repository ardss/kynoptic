# release-branch-hygiene.ps1 — 本地陈旧分支卫生门控（发布窗口执行）
#
# 判定铁律（Wave45 缺陷 3 固化）：分支内容是否"已在 main"必须用 tree 字节级
# 对比判定——分支 tip 的 tree 与 main 上某个提交的 tree 完全相同，则其全部
# 内容字节级已在 main（squash 合入的提交正是以同 tree 落在 main 上），删除
# 无内容损失（仅丢 squash 前逐提交粒度，正常代价）。
#
# 反例（禁止使用）：`git branch --merged main`。squash 合并后分支 tip 不是
# main 的祖先，--merged 只会列出 main（实测如此），照它复核会误判
# "未合并、不可删"。本脚本不依赖任何祖先关系判定。
#
# 默认只验证（可重复跑）；-Delete 在"全部待删分支均通过验证"的前提下执行
# git branch -D（发布窗口前清偿陈旧分支）。

param(
  [string[]]$Branch = @('chore/site-sync-v0.2.2', 'fix/wave42-deep-review', 'fix/wave43-deep-review'),
  [switch]$Delete
)

$ErrorActionPreference = 'Stop'
$Root = & git -C $PSScriptRoot rev-parse --show-toplevel
if ($LASTEXITCODE -ne 0) { Write-Error '定位仓库根失败'; exit 1 }
Set-Location $Root
Write-Host "== branch-hygiene: $Root"

# main 上全部提交的 tree 集合（一次 rev-list 取回，本地比对）
$rawPairs = & git rev-list main --format='%H %T'
if ($LASTEXITCODE -ne 0) { Write-Error 'git rev-list main 失败'; exit 1 }
$pairs = @($rawPairs | Where-Object { $_ -match '^[0-9a-f]{40} [0-9a-f]{40}$' })
$mainByTree = @{}
foreach ($p in $pairs) {
  $parts = $p -split ' '
  if (-not $mainByTree.ContainsKey($parts[1])) { $mainByTree[$parts[1]] = $parts[0] }
}

$allVerified = $true
foreach ($b in $Branch) {
  & git rev-parse --verify --quiet "refs/heads/$b" > $null
  if ($LASTEXITCODE -ne 0) {
    Write-Host "[SKIP] $b 不存在（可能已删）"
    continue
  }
  $tipTree = & git rev-parse "${b}^{tree}"
  if ($LASTEXITCODE -ne 0) { Write-Error "取 $b 的 tip tree 失败"; exit 1 }
  if ($mainByTree.ContainsKey($tipTree)) {
    $mainC = $mainByTree[$tipTree]
    Write-Host "[OK]   $b tip-tree $tipTree ≡ main 提交 $mainC（字节级同一内容，删除无损）"
  } else {
    $allVerified = $false
    Write-Host "[FAIL] $b tip-tree $tipTree 在 main 上无字节级同 tree 提交，禁止删除"
  }
}

if (-not $allVerified) {
  Write-Host "== 存在未通过验证的分支，未删除任何分支。"
  exit 1
}

if (-not $Delete) {
  Write-Host "== 验证通过（tree 字节级对比）。发布窗口执行删除："
  Write-Host "   pwsh scripts/release-branch-hygiene.ps1 -Delete"
  exit 0
}

foreach ($b in $Branch) {
  & git rev-parse --verify --quiet "refs/heads/$b" > $null
  if ($LASTEXITCODE -eq 0) {
    git branch -D $b
    if ($LASTEXITCODE -ne 0) { Write-Error "git branch -D $b 失败"; exit 1 }
  }
}
Write-Host "== 陈旧分支已删除（main 受推送保护，本地删分支不影响远端）。"
exit 0

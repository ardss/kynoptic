# register-browser-host.ps1 — 注册 Chrome Native Messaging host（PRD F02）。
#
# 作用：构建 kynoptic-browser-host，生成填写好的 host 清单（host exe 绝对
# 路径 + 扩展 ID），并写 HKCU 注册表键
#   HKCU\Software\Google\Chrome\NativeMessagingHosts\kynoptic_browser_host
# 默认值 = 清单文件绝对路径。HKCU 写入不需要管理员权限。
#
# 用法（在仓库根目录执行）：
#   powershell -ExecutionPolicy Bypass -File scripts\register-browser-host.ps1 -ExtensionId <32位扩展ID>
# 扩展 ID 获取：chrome://extensions 开发者模式加载 browser-extension/ 后，
# 扩展卡片上显示的 ID（可在 scripts/register-browser-host.ps1 之后用
# chrome://extensions 的 ID 替换重跑一次）。
#
# 参数：
#   -ExtensionId   必填，32 位 Chrome 扩展 ID（可传多个：-ExtensionId a,b）
#   -Db            可选，kynoptic 数据库路径（host 运行时 --db 透传）
#   -HostExe       可选，host 可执行文件路径（默认 target/release/kynoptic-browser-host.exe）

param(
    [Parameter(Mandatory = $true)][string]$ExtensionId,
    [string]$Db = "",
    [string]$HostExe = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

# 1. 构建 host 与 ctl
cargo build --release --manifest-path (Join-Path $root "Cargo.toml") -p kynoptic-browser-host -p kynoptic
if ($LASTEXITCODE -ne 0) { throw "[!] cargo build 失败" }

# 2. host 可执行文件（默认 target/release 下）
if (-not $HostExe) {
    $HostExe = Join-Path $root "target\release\kynoptic-browser-host.exe"
}
if (-not (Test-Path $HostExe)) { throw "[!] host 可执行文件不存在: $HostExe" }

# 3. 经 kynoptic host-register 写清单 + HKCU 注册表
$extArgs = $ExtensionId.Split(",") | ForEach-Object { "--ext"; $_.Trim() } | Where-Object { $_ }
$manifestPath = Join-Path $env:APPDATA "kynoptic\kynoptic_browser_host.json"
$ctlArgs = @("host-register", "--host-exe", $HostExe, "--manifest", $manifestPath) + $extArgs

# kynoptic host-register 本身也支持 --db（全局选项），透传给注册流程无意义，
# 但 -Db 会被写入清单旁的说明文件供排障参考
$ctl = Join-Path $root "target\release\kynoptic.exe"
& $ctl @ctlArgs
if ($LASTEXITCODE -ne 0) { throw "[!] kynoptic host-register 失败" }

if ($Db) {
    $notePath = Join-Path (Split-Path -Parent $manifestPath) "browser_host_db.txt"
    Set-Content -Path $notePath -Value "--db $Db" -Encoding UTF8
    Write-Host "[i] db 路径备注已写入: $notePath"
    Write-Host "[i] host 运行时的 --db 由注册表/清单不承载，直接调用 host 时自行传："
    Write-Host "    $HostExe --db $Db"
}

Write-Host "完成。重新加载扩展或重启 Chrome 使注册生效。"
Write-Host "真机验收（需装扩展后人工执行）："
Write-Host "  1. chrome://extensions 打开扩展卡片 service worker 的 DevTools；"
Write-Host "  2. Console 执行 chrome.runtime.sendNativeMessage(\"kynoptic_browser_host\", {event_type:\".tab_change\", event_id:\"manual-test-1\", ts_ms:Date.now(), kind:\"activated\", tab_id:1, window_id:1, url:\"https://example.com/\", title:\"Sample\", active:true}, r => console.log(r));"
Write-Host "  3. 期望收到 {ok:true,...}；重复发送同 event_id 收到 deduped:true。"

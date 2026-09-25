; Kynoptic 安装器脚本（Inno Setup 6）
; CI: iscc kynoptic.iss /DAppVersion=<版本>（必须外部传入，见下）
; 版本漂移审查：忘传 /DAppVersion 时曾打包出 0.2.0 安装包而二进制是 0.2.2
; 的自相矛盾产物。本文件不再持有版本号缺省值，只认 /DAppVersion。

#ifndef AppVersion
#error 请以 /DAppVersion=<版本> 显式传入版本号（如 scripts/bump-version.mjs 写入 Cargo.toml 的值）
#endif

; 测试隔离后缀（/DTestSuffix=sbox1）：带后缀编译出的安装包使用独立的
; AppId（卸载注册完全隔离，异目录演练卸载不再互删生产注册信息）、独立
; 任务名与 Run 值名（叠加安装目录指纹，见 [Code] InstallFingerprint）、
; 独立 skill 目录与独立输出文件名，可与真实安装并存做沙箱演练。仅测试
; 构建使用；生产构建不带此参数，命名与指纹规则和运行时（core::naming）
; 完全一致；TestSuffix 构建运行时读 KYNOPTIC_MUTEX_SUFFIX（与 singleton
; 同一开关）拼同形后缀名，沙箱闭环两侧一致（回归复审修复）。
#ifdef TestSuffix
#define OutTag "-" + TestSuffix
#else
#define OutTag ""
#endif

; 生产 AppId 永久不可改动：它是升级/卸载识别的惟一键，改动后旧版本无法被
; 新安装包覆盖升级。TestSuffix 构建派生独立 AppId（Inno 的 AppId 接受任意
; 字符串，非 GUID 也可；指向独立卸载注册，不触碰生产的添加/删除程序条目）。
#ifdef TestSuffix
#define MyAppId "Kynoptic-Test-" + TestSuffix + "-E761967B-0337-4F1A-B522-1C45B57AFF1D"
#else
#define MyAppId "{{E761967B-0337-4F1A-B522-1C45B57AFF1D}"
#endif

#define AppName "Kynoptic"
#define AppPublisher "ardss"
#define AppExeName "kynoptic-tray.exe"

[Setup]
; 升级不记忆上次的任务勾选（审查 P1：旧版本默认未勾 autostart，记忆会让
; 修复后的默认勾选在升级场景永远不生效）。
UsePreviousTasks=no
; 永久不可改动（生产构建）：AppId 是升级/卸载识别的惟一键，改动后旧版本
; 无法被新安装包覆盖升级，会装出第二份程序并留下无法卸载的旧记录。
; TestSuffix 构建使用派生 AppId（见文件头），与生产卸载注册完全隔离。
AppId={#MyAppId}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppCopyright=Copyright (c) 2026 ardss
; 安装包 exe 与向导的图标（资源管理器里一眼认出是 Kynoptic）
SetupIconFile=assets\kynoptic.ico
WizardSmallImageFile=assets\wizard-small.png
UninstallDisplayName={#AppName} {#AppVersion}
UninstallDisplayIcon={app}\{#AppExeName}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
OutputDir=dist
OutputBaseFilename=Kynoptic-Setup-{#AppVersion}{#OutTag}
Compression=lzma2/max
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
PrivilegesRequired=lowest
; force:托盘是无可视窗口的后台进程,收不到 WM_CLOSE,普通等待会永远卡在
; "正在关闭应用";直接终止旧进程(SQLite WAL 模式,任意时刻终止都安全)。
CloseApplications=force
; RM 强制关闭应用后默认会弹"是否重新启动应用"确认框；/SUPPRESSMSGBOXES 下该
; 框被折叠成默认值 Abort，导致静默升级 exit=5 回滚。显式关闭重启动提示。
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Files]
Source: "dist\kynoptic-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\kynoptic.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\kynoptic-watchdog.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\kynoptic-aggrepair.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\kynoptic-ctl.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\README.md"; DestDir: "{app}"; Flags: ignoreversion
; SKILL.md 随安装包分发（AI 客户端 skill，见 [Run] 段 skill install）
Source: "crates\cli\src\assets\skill.md"; DestDir: "{app}"; DestName: "SKILL.md"; Flags: ignoreversion
; migrate 子命令依赖的迁移脚本（kynoptic-ctl 按 exe 同级 scripts\ 解析）
Source: "scripts\migrate_legacy_db.py"; DestDir: "{app}\scripts"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\{#AppName} CLI"; Filename: "{app}\kynoptic.exe"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Tasks]
; 桌面图标默认勾选（装机审查：唯一持久的"回到应用"入口，默认不勾会让
; 用户装完找不到应用——托盘图标默认收在 Win11 溢出区）
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
; 默认勾选（审查 P1：静默安装按默认勾选态执行，unchecked 会让静默升级
; 静默丢失自启动）；安装器 always-creates 看门狗任务，未勾选时转 DISABLE。
Name: "autostart"; Description: "{cm:AutoStartTask}"

; Run 自启动值不再走 [Registry] 段：该段在安装事务内、ssPostInstall 之前写
; 入，失败/回滚后不清理，留下"未勾自启动的用户升级失败后自启动被静默打开且
; 指向旧路径"的孤儿态。改由 [Code] 在 ssPostInstall 确认成功后写入（见
; FinalizeAutostart），失败路径天然不残留；卸载时由代码对称删除。

[Run]
; AI 客户端 skill 同步（静默、总是执行；升级覆盖安装也会刷新 SKILL.md）。
; TestSuffix 构建经 cmd 注入 KYNOPTIC_MUTEX_SUFFIX：skill 写入
; skills/kynoptic-<suffix>、托盘互斥体加同款后缀，与生产实例完全并行
;（与 crates/cli skill 目录选择、core/singleton.rs 同一开关，两侧闭环）。
#ifdef TestSuffix
Filename: "{cmd}"; Parameters: "/C set KYNOPTIC_MUTEX_SUFFIX={#TestSuffix}&& ""{app}\kynoptic.exe"" skill install"; Flags: runhidden
Filename: "{cmd}"; Parameters: "/C set KYNOPTIC_MUTEX_SUFFIX={#TestSuffix}&& ""{app}\{#AppExeName}"" --minimized"; Description: "Launch Kynoptic (background tray - right-click the tray icon to open the dashboard / 启动 Kynoptic 后台托盘，右键托盘图标打开面板)"; Flags: nowait postinstall skipifsilent
#else
Filename: "{app}\kynoptic.exe"; Parameters: "skill install"; Flags: runhidden
; 说明性 Description：让用户知道启动的是后台托盘进程而非窗口程序
Filename: "{app}\{#AppExeName}"; Description: "Launch Kynoptic (background tray - right-click the tray icon to open the dashboard / 启动 Kynoptic 后台托盘，右键托盘图标打开面板)"; Flags: nowait postinstall skipifsilent
#endif
; 看门狗计划任务不再由 [Run] 段创建：该段执行一次且退出码被吞，任何瞬时失败
; 都精确产生"安装 exit=0 + 任务保持旧状 + 无痕迹"。改由 [Code] 的
; RebuildWatchdogTask（先删后建+重试+回读校验+日志）在 ssDone（[Run] 之后）
; 统一收敛终态，DISABLE 也不会再被此处覆盖回 Enabled。

[Code]
// ============================ 通用辅助 ============================

var
  // 安装中断补偿（失败/取消恢复路径）：安装是否已走到 ssPostInstall、
  // 升级前用户是否已显式关闭自启动（Run 值缺失 + 任务存在）。
  G_PostInstallDone: Boolean;
  G_AutostartWasDisabled: Boolean;
  // InitializeSetup 走完才允许补偿逻辑碰机器状态：初始化本身失败（如命令行
  // 检测抛异常）时 {app} 未初始化，补偿自己也会炸，且可能误动另一目录/生产
  // 的计划任务（0.3.1 发布门禁实测教训）。
  G_SetupInitialized: Boolean;

const
  RunKeyPath = 'Software\Microsoft\Windows\CurrentVersion\Run';
  // 用户级 PATH 所在键（PrivilegesRequired=lowest，只碰 HKCU）
  EnvKeyPath = 'Environment';
  WM_SETTINGCHANGE = $001A;
  // HWND_BROADCAST 为 Inno 预定义常量，不重复声明
  SMTO_ABORTIFHUNG = $0002;

// 广播用的 Win32 API（真机探针验证过调用形状；lParam 传 'Environment'）
function SendMessageTimeout(hWnd: HWND; Msg: UINT; wParam: Longint;
  lParam: String; fuFlags, uTimeout: DWORD; var lpdwResult: DWORD): DWORD;
  external 'SendMessageTimeoutW@user32.dll stdcall';

// 广播环境变量变更，让资源管理器与新开的终端立即看到新 PATH
//（真机探针验证：SendMessageTimeoutW 广播返回非 0 即成功）
procedure BroadcastEnvChange();
var
  Res: DWORD;
begin
  SendMessageTimeout(HWND_BROADCAST, WM_SETTINGCHANGE, 0, 'Environment',
    SMTO_ABORTIFHUNG, 1000, Res);
end;

// 去掉字符串尾部的分号（不引入 Inno 版本相关的字符串助手）
function TrimTrailingSemicolons(const S: String): String;
begin
  Result := S;
  while (Length(Result) > 0) and (Result[Length(Result)] = ';') do
    SetLength(Result, Length(Result) - 1);
end;

// ============================ 按安装目录指纹命名 ============================
// Wave40 挂账（安装器全局命名空间隔离）：「Kynoptic Watchdog」任务名与
// 「Kynoptic」Run 值名是全局的，任何一次 /DIR= 指向别处的安装（含沙箱试装）
// 都会夺走已装实例的任务指向。改为按安装目录指纹派生专属名字，运行时侧
// （crates/core/src/naming.rs）以同一算法独立实现，两侧必须逐字节一致。

// Integer → 4 位大写十六进制（Inno PascalScript 无 IntToHex，也不支持函数内
// const 段，手写等价实现，与 Rust 侧 format!("{:04X}") 对齐）
function FingerprintHalfToHex(const V: Integer): String;
var
  T, I: Integer;
begin
  Result := '';
  T := V;
  for I := 1 to 4 do begin
    Result := Copy('0123456789ABCDEF', (T mod 16) + 1, 1) + Result;
    T := T div 16;
  end;
end;

// 安装目录指纹：小写化、去结尾分隔符后，对 UTF-16 码元做 h=5381 起
// h=(h*33+码元) mod 2^32 的 djb 变体，输出 8 位大写十六进制。用 UTF-16 码元
// 是为了与 Rust 侧 encode_utf16() 对齐。算法勿改；实现刻意不用 Int64——
// 0.3.1 发布门禁实测：部分 Inno PascalScript 编译产物对 Int64 乘法与
// mod 4294967296 字面量求值错误，把指纹算成 00000000（本机 6.7.3 复测正常，
// 缺陷随 runner 预装版本漂移）。改为 h 拆高低 16 位（HiW*65536+LoW），全程
// Integer 运算且中间值 < 2^22，任何版本下数值行为一致（真机 ISCC 探针
// 双向量与 Rust/脚本参考实现比对一致，2026-09-25）。
function InstallFingerprint(): String;
var
  AppDir: String;
  HiW, LoW, ProdLo, NewLo, NewHi, I: Integer;
begin
  AppDir := Lowercase(ExpandConstant('{app}'));
  while (Length(AppDir) > 0) and
        ((AppDir[Length(AppDir)] = '\') or (AppDir[Length(AppDir)] = '/')) do
    SetLength(AppDir, Length(AppDir) - 1);
  HiW := 0;
  LoW := 5381;
  for I := 1 to Length(AppDir) do begin
    // h*33 = HiW*33*65536 + LoW*33；LoW*33+码元 < 2^22，进位并入高半字
    ProdLo := LoW * 33 + Ord(AppDir[I]);
    NewLo := ProdLo mod 65536;
    NewHi := (HiW * 33 + (ProdLo div 65536)) mod 65536;
    HiW := NewHi;
    LoW := NewLo;
  end;
  Result := FingerprintHalfToHex(HiW) + FingerprintHalfToHex(LoW);
end;

// 本次安装的看门狗计划任务名：Kynoptic Watchdog [ -后缀] <指纹>
function WatchdogTaskName(): String;
begin
  Result := 'Kynoptic Watchdog';
#ifdef TestSuffix
  Result := Result + '-{#TestSuffix}';
#endif
  Result := Result + ' ' + InstallFingerprint();
end;

// 本次安装的 Run 自启动值名：Kynoptic [ -后缀] -<指纹>
function RunValueName(): String;
begin
  Result := 'Kynoptic';
#ifdef TestSuffix
  Result := Result + '-{#TestSuffix}';
#endif
  Result := Result + '-' + InstallFingerprint();
end;

// 旧版无指纹全局名（升级窗口 DISABLE 与升级清扫用；TestSuffix 构建绝不碰
// 生产名字，只认自己旧后缀形态的无指纹名，与 CleanupLegacyAutostart 同规则）
function LegacyWatchdogTaskName(): String;
begin
#ifndef TestSuffix
  Result := 'Kynoptic Watchdog';
#else
  Result := 'Kynoptic Watchdog-{#TestSuffix}';
#endif
end;

function LegacyRunValueName(): String;
begin
#ifndef TestSuffix
  Result := 'Kynoptic';
#else
  Result := 'Kynoptic-{#TestSuffix}';
#endif
end;

// skill 安装目录名（与 crates/cli skill_dir_name() 同规则：TestSuffix 经
// [Run] 注入的 KYNOPTIC_MUTEX_SUFFIX 写 skills/kynoptic-<suffix>）
function SkillDirName(): String;
begin
  Result := 'kynoptic';
#ifdef TestSuffix
  Result := Result + '-{#TestSuffix}';
#endif
end;

// 旧版无指纹全局名的升级清扫在 RunHidden 定义之后（CleanupAutostartEntries），
// 避免前向引用。

// 按分号切分 PATH 为条目数组（手写切分，兼容全部 Inno 6.x；跳过空条目）
function SplitPathEntries(const PathVal: String): TArrayOfString;
var
  Count, Start, I: Integer;
begin
  SetArrayLength(Result, 0);
  Count := 0;
  Start := 1;
  for I := 1 to Length(PathVal) + 1 do begin
    if (I > Length(PathVal)) or (PathVal[I] = ';') then begin
      if I > Start then begin
        SetArrayLength(Result, Count + 1);
        Result[Count] := Copy(PathVal, Start, I - Start);
        Count := Count + 1;
      end;
      Start := I + 1;
    end;
  end;
end;

// 用户 PATH 是否已含 Dir（分号分隔、大小写不敏感的整项比较）
function UserPathHasEntry(const PathVal, Dir: String): Boolean;
var
  Parts: TArrayOfString;
  I: Integer;
begin
  Result := False;
  Parts := SplitPathEntries(PathVal);
  for I := 0 to GetArrayLength(Parts) - 1 do
    if Uppercase(Trim(Parts[I])) = Uppercase(Dir) then begin
      Result := True;
      Exit;
    end;
end;

// 安装目录写入用户 PATH（官方文档/README 的 MCP 配置都假设 `kynoptic` 是裸
// 命令，而安装器此前从不写 PATH，安装版用户照做必然"找不到命令"）。
// 已存在时不重复追加；写回保留原值的展开类型（含 %VAR% 用 REG_EXPAND_SZ，
// 真机探针验证：按 DoNotExpand 读写往返不破坏含 %VAR% 条目）。
procedure AddInstallDirToUserPath();
var
  Dir, Cur, NewVal: String;
  HadValue, HasPercent: Boolean;
begin
  Dir := ExpandConstant('{app}');
  HadValue := RegQueryStringValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path', Cur);
  if not HadValue then
    Cur := '';
  if UserPathHasEntry(Cur, Dir) then
    Exit;
  if Cur = '' then
    NewVal := Dir
  else
    NewVal := TrimTrailingSemicolons(Cur) + ';' + Dir;
  HasPercent := Pos('%', NewVal) > 0;
  if HadValue and HasPercent then
    RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path', NewVal)
  else
    RegWriteStringValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path', NewVal);
  BroadcastEnvChange();
  Log('install dir appended to user PATH');
end;

// 卸载对称清理：从用户 PATH 移除安装目录项；移除后为空则删除整个值。
// 每一项只在与 Dir 全等时剔除，不动用户自配的其它条目。
procedure RemoveInstallDirFromUserPath();
var
  Dir, Cur: String;
  Parts: TArrayOfString;
  Keep: TArrayOfString;
  I, N: Integer;
begin
  Dir := ExpandConstant('{app}');
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path', Cur) then
    Exit;
  Parts := SplitPathEntries(Cur);
  SetArrayLength(Keep, 0);
  for I := 0 to GetArrayLength(Parts) - 1 do
    if Uppercase(Trim(Parts[I])) <> Uppercase(Dir) then begin
      N := GetArrayLength(Keep);
      SetArrayLength(Keep, N + 1);
      Keep[N] := Parts[I];
    end;
  if GetArrayLength(Keep) = 0 then begin
    RegDeleteValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path');
  end else begin
    Cur := '';
    for I := 0 to GetArrayLength(Keep) - 1 do begin
      if I > 0 then
        Cur := Cur + ';';
      Cur := Cur + Keep[I];
    end;
    if Pos('%', Cur) > 0 then
      RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path', Cur)
    else
      RegWriteStringValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path', Cur);
  end;
  BroadcastEnvChange();
  Log('install dir removed from user PATH');
end;

// 清理指向已消失的用户 Temp 目录的孤儿 PATH 项（实机审查 high：演练装完
// 直接删目录而不走卸载器会永久残留孤儿项，指向可写的 %TEMP% 构成命令解析
// 投毒面）。只剔「位于用户 Temp 之下且目录已不存在」的项——不做全盘剔不
// 存在项，避免误伤 U 盘/网络路径这类暂时不可达的用户自配条目。
procedure RemoveOrphanTempPathEntries();
var
  TempDir, TempDirU, Cur, Entry, EntryU: String;
  Parts, Keep: TArrayOfString;
  I, N: Integer;
begin
  TempDir := GetEnv('TEMP');
  if TempDir = '' then
    Exit;
  TempDir := Trim(TempDir);
  while (Length(TempDir) > 0) and (TempDir[Length(TempDir)] = '\') do
    SetLength(TempDir, Length(TempDir) - 1);
  TempDirU := Uppercase(TempDir);
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path', Cur) then
    Exit;
  Parts := SplitPathEntries(Cur);
  SetArrayLength(Keep, 0);
  for I := 0 to GetArrayLength(Parts) - 1 do begin
    Entry := Trim(Parts[I]);
    EntryU := Uppercase(Entry);
    if (Entry <> '') and (Pos(TempDirU, EntryU) = 1) and (not DirExists(Entry)) then
      Log('removing orphan temp PATH entry: ' + Entry)
    else begin
      N := GetArrayLength(Keep);
      SetArrayLength(Keep, N + 1);
      Keep[N] := Parts[I];
    end;
  end;
  if GetArrayLength(Keep) = GetArrayLength(Parts) then
    Exit; // 无变化不回写
  Cur := '';
  for I := 0 to GetArrayLength(Keep) - 1 do begin
    if I > 0 then
      Cur := Cur + ';';
    Cur := Cur + Keep[I];
  end;
  if Pos('%', Cur) > 0 then
    RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path', Cur)
  else
    RegWriteStringValue(HKEY_CURRENT_USER, EnvKeyPath, 'Path', Cur);
  BroadcastEnvChange();
end;

// skill 目录归属指纹（与 crates/cli 的 SKILL_SOURCE_MARKER 同约定）：
// 0=指向本次安装目录（可删）；1=指向别处（另一份安装的，不动）；
// 2=无指纹（旧版安装/历史残留，维持旧版行为删除）。
function SkillMarkerState(const Dir: String): Integer;
var
  Lines: TArrayOfString;
  S: String;
begin
  Result := 2;
  // LoadStringFromFile 读的是 AnsiString（与 String 类型不匹配），改用
  // LoadStringsFromFile 取内容行（sidecar 只有一行路径）
  if not LoadStringsFromFile(Dir + '\.kynoptic-source', Lines) then
    Exit;
  if GetArrayLength(Lines) = 0 then
    Exit;
  S := Trim(Lines[GetArrayLength(Lines) - 1]);
  if (S <> '') and (Uppercase(S) = Uppercase(ExpandConstant('{app}'))) then
    Result := 0
  else
    Result := 1;
end;

// 静默执行外部命令（SW_HIDE 不弹窗），返回是否成功且退出码为 0
function RunHidden(const Exe, Params: String): Boolean;
var
  CmdResult: Integer;
begin
  Result := Exec(Exe, Params, '', SW_HIDE, ewWaitUntilTerminated, CmdResult);
  if Result then
    Result := (CmdResult = 0);
end;

// ===================== 自启动清扫（按指向归属） =====================
// 0.3.1 审查修复（卸载清扫双向缺口，同根因合并）：旧的「两个精确名无条件
// 删」①对同后缀异目录的另一份沙箱安装无归属校验，任意一方卸载都会误删
// 对方的 legacy 任务与 Run 值；②对畸名/更旧指纹名（如「… 00000000」）无
// 枚举兜底，永久残留并反复触发调度报错。统一改为：读任务 TR / Run 值数据
// 里的 exe 路径，仅当指向本次 {app}（升级残留）或指向已不存在的目录（孤儿）
// 时才删，指向本机其它仍在的安装目录一律保留；并按前缀枚举兜底，覆盖当前
// 算法产不出的名字。每个删除/保留决定都留日志。
//
// 清扫前缀即 Legacy* 名：生产构建覆盖全部「Kynoptic Watchdog」/「Kynoptic」
// 开头名字（归属校验兜住不误删活安装）；TestSuffix 构建只认自己后缀形态，
// 绝不触碰生产名字（与旧 CleanupLegacyAutostart 同规则）。

// 去掉字符串结尾的路径分隔符
function TrimTrailingSeparators(const S: String): String;
begin
  Result := S;
  while (Length(Result) > 0) and
        ((Result[Length(Result)] = '\') or (Result[Length(Result)] = '/')) do
    SetLength(Result, Length(Result) - 1);
end;

// exe 路径是否位于 Dir 之下（大小写不敏感，不含 Dir 本身）
function PathUnderDir(const ExePath, Dir: String): Boolean;
var
  D, P: String;
begin
  D := Uppercase(TrimTrailingSeparators(Dir));
  P := Uppercase(Trim(ExePath));
  Result := (Length(P) > Length(D)) and (Copy(P, 1, Length(D)) = D) and
    (P[Length(D) + 1] = '\');
end;

// 从命令行/值数据里提取 exe 路径：优先取引号（单/双）内的内容，无引号取
// 首个空格前的段；解析不出返回空串（调用方安全侧不删）
function ExtractExePath(const CmdLine: String): String;
var
  Q: Char;
  Start, I: Integer;
begin
  Result := '';
  Q := #0;
  Start := 0;
  for I := 1 to Length(CmdLine) do begin
    if (Q = #0) and ((CmdLine[I] = '"') or (CmdLine[I] = Chr(39))) then begin
      Q := CmdLine[I];
      Start := I + 1;
    end else if (Q <> #0) and (CmdLine[I] = Q) then begin
      Result := Copy(CmdLine, Start, I - Start);
      Exit;
    end else if (Q = #0) and (CmdLine[I] = ' ') then begin
      Result := Copy(CmdLine, 1, I - 1);
      Exit;
    end;
  end;
  if Q = #0 then
    Result := Trim(CmdLine);
end;

// 读指定任务「要运行的任务」字段的原始命令行（LIST /V 输出，中英系统都认；
// 勿用 CSV 的该列——同状态列的版本相关解析问题，统一走 LIST 字段）
function GetTaskActionCmd(const TaskName: String): String;
var
  TmpFile: String;
  CmdResult: Integer;
  Lines: TArrayOfString;
  I, P: Integer;
  S: String;
begin
  Result := '';
  TmpFile := ExpandConstant('{tmp}') + '\kyn-task-tr.txt';
  Exec('cmd.exe', '/C schtasks /Query /TN "' + TaskName + '" /FO LIST /V > "' + TmpFile + '" 2>&1',
    '', SW_HIDE, ewWaitUntilTerminated, CmdResult);
  if LoadStringsFromFile(TmpFile, Lines) then
    for I := 0 to GetArrayLength(Lines) - 1 do begin
      S := TrimRight(Lines[I]);
      if (Pos('Task To Run', S) > 0) or (Pos('要运行的任务', S) > 0) then begin
        P := Pos(':', S);
        if P > 0 then
          Result := Trim(Copy(S, P + 1, MaxInt));
      end;
    end;
  DeleteFile(TmpFile);
end;

// 枚举名字以 Prefix 开头的用户任务（schtasks /Query /FO CSV /NH 全列表，
// 取每行首个引号字段为任务名；查不到任何任务时返回空数组）
function ListTaskNamesByPrefix(const Prefix: String): TArrayOfString;
var
  TmpFile: String;
  CmdResult: Integer;
  Lines: TArrayOfString;
  I, Q1, Q2, N: Integer;
  S, Name: String;
begin
  SetArrayLength(Result, 0);
  TmpFile := ExpandConstant('{tmp}') + '\kyn-task-list.txt';
  Exec('cmd.exe', '/C schtasks /Query /FO CSV /NH > "' + TmpFile + '" 2>&1',
    '', SW_HIDE, ewWaitUntilTerminated, CmdResult);
  if not LoadStringsFromFile(TmpFile, Lines) then begin
    DeleteFile(TmpFile);
    Exit;
  end;
  N := 0;
  for I := 0 to GetArrayLength(Lines) - 1 do begin
    S := Lines[I];
    Q1 := Pos('"', S);
    if Q1 = 0 then
      Continue;
    Q2 := Pos('"', Copy(S, Q1 + 1, MaxInt));
    if Q2 = 0 then
      Continue;
    Name := Copy(S, Q1 + 1, Q2 - 1);
    // 任务名可能带根文件夹前导「\」，剥掉再比前缀
    while (Length(Name) > 0) and (Name[1] = '\') do
      Name := Copy(Name, 2, MaxInt);
    if (Name <> '') and
       (Uppercase(Copy(Name, 1, Length(Prefix))) = Uppercase(Prefix)) then begin
      SetArrayLength(Result, N + 1);
      Result[N] := Name;
      N := N + 1;
    end;
  end;
  DeleteFile(TmpFile);
end;

// 指向是否可清：指向本次 {app}（本目录升级残留）或指向已不存在的目录
//（孤儿，如手工删除目录没走卸载器的演练残留）；解析不出指向的安全侧不删
function ShouldSweepDeleteTarget(const ExePath: String): Boolean;
begin
  Result := False;
  if Trim(ExePath) = '' then
    Exit;
  if PathUnderDir(ExePath, ExpandConstant('{app}')) then begin
    Result := True;
    Exit;
  end;
  Result := not DirExists(ExtractFileDir(Trim(ExePath)));
end;

// 自启动统一清扫（调用点：ssDone 新名终态收敛完之后与卸载收尾）。本名
//（本次安装刚收敛/已精确删除的终态）跳过，其余按指向归属删留。
procedure CleanupAutostartEntries();
var
  Names: TArrayOfString;
  I: Integer;
  Name, Data, Exe: String;
begin
  // 计划任务侧
  Names := ListTaskNamesByPrefix(LegacyWatchdogTaskName());
  for I := 0 to GetArrayLength(Names) - 1 do begin
    Name := Names[I];
    if Uppercase(Name) = Uppercase(WatchdogTaskName()) then
      Continue;
    Exe := ExtractExePath(GetTaskActionCmd(Name));
    if ShouldSweepDeleteTarget(Exe) then begin
      Log('sweep: delete task (this install or orphan), TR=' + Exe + ', name=' + Name);
      RunHidden('schtasks', '/Delete /F /TN "' + Name + '"');
    end else
      Log('sweep: keep task (another live install or unreadable TR), name=' + Name);
  end;
  // Run 值侧
  if RegGetValueNames(HKEY_CURRENT_USER, RunKeyPath, Names) then
    for I := 0 to GetArrayLength(Names) - 1 do begin
      Name := Names[I];
      if Uppercase(Copy(Name, 1, Length(LegacyRunValueName()))) <>
         Uppercase(LegacyRunValueName()) then
        Continue;
      if Uppercase(Name) = Uppercase(RunValueName()) then
        Continue;
      if not RegQueryStringValue(HKEY_CURRENT_USER, RunKeyPath, Name, Data) then
        Continue;
      Exe := ExtractExePath(Data);
      if ShouldSweepDeleteTarget(Exe) then begin
        Log('sweep: delete Run value (this install or orphan), data=' + Exe + ', name=' + Name);
        RegDeleteValue(HKEY_CURRENT_USER, RunKeyPath, Name);
      end else
        Log('sweep: keep Run value (another live install), name=' + Name);
    end;
end;

// 指定任务名的计划任务是否存在（升级识别）
function TaskNameExists(const TaskName: String): Boolean;
var
  CmdResult: Integer;
begin
  Result := Exec('schtasks', '/Query /TN "' + TaskName + '"', '',
    SW_HIDE, ewWaitUntilTerminated, CmdResult) and (CmdResult = 0);
end;

// 本次安装的看门狗任务是否已存在（升级识别）
function WatchdogTaskExists(): Boolean;
begin
  Result := TaskNameExists(WatchdogTaskName());
end;

// 旧版无指纹任务是否存在（legacy→新升级识别：新指纹任务在升级前尚不存在，
// 只看新名会把 legacy 升级误判成全新安装）
function LegacyWatchdogTaskExists(): Boolean;
begin
  Result := TaskNameExists(LegacyWatchdogTaskName());
end;

// 按安装目录精确杀 Kynoptic 进程（0.2.1 曾以同样理由移除 watchdog 的映像名
// 全局杀 fallback：taskkill /F /IM /T 会扫杀全机同名进程及其子树，任何一次
// 覆盖升级都会误杀本机另一个独立安装目录里正在运行的生产实例）。改为
// PowerShell 按 exe 完整路径过滤：只杀 Path 位于 Dir 下的实例；返回杀完
// 之后仍在运行的同名异目录实例数（>0 即存在另一份安装，提示而非强杀）。
function KillProcessesInDir(const Dir: String): Integer;
var
  TmpFile: String;
  CmdResult: Integer;
  Lines: TArrayOfString;
  Ps: String;
begin
  Result := 0;
  TmpFile := ExpandConstant('{tmp}') + '\kyn-killcnt.txt';
  // $_ 与 $ 外层是 PowerShell 变量，Pascal 字符串不转义 $；单引号用 '' 表示
  Ps := '-NoProfile -ExecutionPolicy Bypass -Command "$n = @(''kynoptic-watchdog.exe'',''kynoptic-tray.exe'',''kynoptic-ctl.exe''); ' +
    'Get-Process -Name $n -ErrorAction SilentlyContinue | Where-Object { $_.Path -like ''' + Dir + '\*'' } | Stop-Process -Force; ' +
    '@(Get-Process -Name $n -ErrorAction SilentlyContinue | Where-Object { $_.Path -and ($_.Path -notlike ''' + Dir + '\*'') }).Count"';
  if Exec('cmd.exe', '/C powershell.exe ' + Ps + ' > "' + TmpFile + '" 2>&1',
    '', SW_HIDE, ewWaitUntilTerminated, CmdResult) then
    if LoadStringsFromFile(TmpFile, Lines) then
      Result := StrToIntDef(Trim(Lines[GetArrayLength(Lines) - 1]), 0);
  DeleteFile(TmpFile);
end;

// 文件是否被进程锁定（独占打开失败即锁定；不存在视为未锁定）
function FileLocked(const Path: String): Boolean;
var
  FS: TFileStream;
begin
  Result := False;
  if not FileExists(Path) then
    Exit;
  try
    FS := TFileStream.Create(Path, fmOpenReadWrite or fmShareExclusive);
    FS.Free;
  except
    Result := True;
  end;
end;

// 按映像名探测进程是否在运行（tasklist 结果经临时文件回读；tasklist 对
// 不存在的进程也返回 0，必须按输出内容判断，不能只看退出码）
function ProcessRunning(const Image: String): Boolean;
var
  TmpFile: String;
  CmdResult: Integer;
  Lines: TArrayOfString;
  I: Integer;
begin
  Result := False;
  TmpFile := ExpandConstant('{tmp}') + '\kyn-proclist.txt';
  if Exec('cmd.exe', '/C tasklist /FI "IMAGENAME eq ' + Image + '" > "' + TmpFile + '" 2>&1',
    '', SW_HIDE, ewWaitUntilTerminated, CmdResult) then
    if LoadStringsFromFile(TmpFile, Lines) then
      for I := 0 to GetArrayLength(Lines) - 1 do
        if Pos(Uppercase(Image), Uppercase(Lines[I])) > 0 then
          Result := True;
  DeleteFile(TmpFile);
end;

// 读指定任务的「计划任务状态」字段值（schtasks /FO LIST /V 输出中标签含
// Status/状态 的行；勿用 CSV 模式的状态列——实测其对「已禁用但仍在运行」
// 的任务会误显示为「正在运行」）。状态文案随系统语言变化（英文 Disabled /
// 中文 已禁用），只认这两种字面量；查不到任务或解析不出时返回 False。
function TaskStateDisabled(const TaskName: String): Boolean;
var
  TmpFile: String;
  CmdResult: Integer;
  Lines: TArrayOfString;
  I: Integer;
  S: String;
begin
  Result := False;
  TmpFile := ExpandConstant('{tmp}') + '\kyn-task-state.txt';
  Exec('cmd.exe', '/C schtasks /Query /TN "' + TaskName + '" /FO LIST /V > "' + TmpFile + '" 2>&1',
    '', SW_HIDE, ewWaitUntilTerminated, CmdResult);
  if LoadStringsFromFile(TmpFile, Lines) then
    for I := 0 to GetArrayLength(Lines) - 1 do begin
      S := TrimRight(Lines[I]);
      if (Pos('Status', S) > 0) or (Pos('状态', S) > 0) then
        if (Pos('Disabled', S) > 0) or (Pos('已禁用', S) > 0) then
          Result := True;
    end;
  DeleteFile(TmpFile);
end;

// 回读指定任务的状态/指向并写安装日志（仅记录不判失败）
procedure LogWatchdogTaskStateFor(const TaskName: String);
var
  TmpFile: String;
  CmdResult: Integer;
  Lines: TArrayOfString;
  I: Integer;
  S: String;
begin
  TmpFile := ExpandConstant('{tmp}') + '\kyn-task-query.txt';
  Exec('cmd.exe', '/C schtasks /Query /TN "' + TaskName + '" /FO LIST /V > "' + TmpFile + '" 2>&1',
    '', SW_HIDE, ewWaitUntilTerminated, CmdResult);
  if LoadStringsFromFile(TmpFile, Lines) then
    for I := 0 to GetArrayLength(Lines) - 1 do begin
      S := TrimRight(Lines[I]);
      if (Pos('Status', S) > 0) or (Pos('状态', S) > 0) or
         (Pos('Task To Run', S) > 0) or (Pos('要运行的任务', S) > 0) then
        Log('watchdog task> ' + S);
    end;
  DeleteFile(TmpFile);
end;

// 回读看门狗任务的状态/指向并写安装日志（仅记录不判失败）
procedure LogWatchdogTaskState();
begin
  LogWatchdogTaskStateFor(WatchdogTaskName());
end;

// DISABLE 指定任务并回读「计划任务状态」字段校验（勿用 CSV 模式列，见
// TaskStateDisabled 注释），失败重试一次；返回终态是否确为已禁用。
// 只记日志不判失败：DISABLE 回读异常不应阻断安装主流程。
function DisableTaskVerified(const TaskName: String): Boolean;
begin
  Result := False;
  if not RunHidden('schtasks', '/Change /TN "' + TaskName + '" /DISABLE') then
    Log('task DISABLE command failed: ' + TaskName);
  Result := TaskStateDisabled(TaskName);
  if not Result then begin
    Sleep(300);
    RunHidden('schtasks', '/Change /TN "' + TaskName + '" /DISABLE');
    Result := TaskStateDisabled(TaskName);
  end;
  if Result then
    Log('task DISABLE verified: ' + TaskName)
  else
    Log('task DISABLE NOT verified after retry: ' + TaskName);
end;

// 重建指向本次安装目录的看门狗任务：先删旧任务再建（/Create 一旦执行必刷新
// StartBoundary，/Delete + /Create 保证不留半新半旧状态），/Create 带重试、
// 退出码入日志，最后回读校验。任何失败都留日志，不再无声吞掉。
function RebuildWatchdogTask(): Boolean;
var
  I: Integer;
begin
  Result := False;
  // 旧任务可能指向旧 {app} 的死路径；不存在时 /Delete 失败，忽略
  RunHidden('schtasks', '/Delete /F /TN "' + WatchdogTaskName() + '"');
  for I := 1 to 3 do begin
    if RunHidden('schtasks', '/Create /F /SC MINUTE /MO 1 /TN "' + WatchdogTaskName() +
      '" /TR "' + Chr(39) +
      ExpandConstant('{app}') + '\kynoptic-watchdog.exe' + Chr(39) + ' watchdog --once"') then begin
      Result := True;
      Break;
    end;
    Log('schtasks /Create failed, retry ' + IntToStr(I));
    Sleep(500);
  end;
  if not Result then
    Log('watchdog task /Create FAILED after 3 retries');
end;

// 重建指向本次安装目录的看门狗任务并转 DISABLE、清 Run 值
//（未勾选 autostart / 静默升级尊重此前显式关闭，共用此路径）
procedure RebuildTaskDisabled();
begin
  if not RebuildWatchdogTask() then
    Log('RebuildTaskDisabled: task rebuild failed, stale task may remain');
  RunHidden('schtasks', '/Change /TN "' + WatchdogTaskName() + '" /DISABLE');
  RegDeleteValue(HKEY_CURRENT_USER, RunKeyPath, RunValueName());
  LogWatchdogTaskState();
end;

// 识别 NSIS 风格的独立 "/S" 参数（/SILENT、/SUPPRESSMSGBOXES 是完整开关，
// 不算命中）
function HasSwitchS(): Boolean;
var
  I: Integer;
  S: String;
begin
  Result := False;
  // 用 ParamStr 逐参数比较（{cmdline} 不是 Inno 常量，ExpandConstant 会抛
  // Unknown constant 使 InitializeSetup 致命失败——0.3.1 发布门禁实测翻车）
  for I := 1 to ParamCount() do begin
    S := Uppercase(Trim(ParamStr(I)));
    if (S = '/S') or (S = '-S') then begin
      Result := True;
      Exit;
    end;
  end;
end;

// ============================ 安装 ============================

function InitializeSetup(): Boolean;
begin
  Result := True;
  G_PostInstallDone := False;
  G_SetupInitialized := False;
  if HasSwitchS() then begin
    // /S 不被 Inno 识别，会弹出交互向导挂死无人值守安装：显式报错退出，
    // 引导改用 /SILENT 或 /VERYSILENT（配合 /SUPPRESSMSGBOXES 时直接中止）
    SuppressibleMsgBox('Kynoptic 不支持 /S 参数：请改用 /SILENT 或 /VERYSILENT 做静默安装。 / ' +
      'The /S switch is not supported; use /SILENT or /VERYSILENT for silent installation.',
      mbError, MB_OK, IDOK);
    Result := False;
  end;
end;

// 关闭所有会锁住 {app} 可执行文件的 Kynoptic 进程（watchdog/tray/ctl）：
// RM 关闭失败时的兜底是重试循环（杀进程→探测文件锁→等 1 秒再杀），不再
// 依赖被 /SUPPRESSMSGBOXES 折叠成 Abort 的 RestartManager 结果。
procedure KillLockedApps();
var
  App: String;
  TryNo: Integer;
  Foreign: Integer;
begin
  App := ExpandConstant('{app}');
  for TryNo := 1 to 5 do begin
    Foreign := KillProcessesInDir(App);
    if (not FileLocked(App + '\kynoptic-tray.exe')) and
       (not FileLocked(App + '\kynoptic-watchdog.exe')) and
       (not FileLocked(App + '\kynoptic-ctl.exe')) then
      Break;
    Log('files still locked, retry kill round ' + IntToStr(TryNo));
    Sleep(1000);
  end;
  // 另一份安装目录的实例仍在跑：只留痕/提示，不强杀（升级审查：
  // 旧实现按映像名全局扫杀会误伤本机其它 Kynoptic 安装）
  if Foreign > 0 then begin
    Log('another install dir still runs ' + IntToStr(Foreign) + ' Kynoptic process(es); left untouched');
    if not WizardSilent then
      SuppressibleMsgBox('检测到本机另一个安装目录的 Kynoptic 正在运行，本次安装未触碰它。 / ' +
        'Another Kynoptic installation is still running; it was left untouched.',
        mbInformation, MB_OK, IDOK);
  end;
end;

// 安装前禁用看门狗计划任务：CloseApplications 只处理已持句柄的进程，
// 防不住 watchdog 每分钟把旧托盘拉起来撞"文件被占用"（审查 P1）；
// 追加强杀残留进程（含 kynoptic-ctl.exe，此前漏杀）。
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  // 失败/取消补偿基线：记录 DISABLE 前用户自启动选择（补偿本身以机器
  // 实际状态为输入，见 DeinitializeSetup）
  // G_SetupInitialized 在此置 True（而非 InitializeSetup）：安装真正开始
  // 碰机器状态前，补偿逻辑才允许动 {app}——向导早期取消（初始化后、复制
  // 前）不越过守卫，DeinitializeSetup 的补偿全部可达（此前 := True 缺失，
  // 补偿是死代码，安装失败后看门狗任务停留 DISABLE）。
  G_SetupInitialized := True;
  G_PostInstallDone := False;
  // 失败/取消补偿基线：新旧两套名字都看（回归复审修复：legacy 升级时新指纹
  // 任务尚不存在，只看新名会把「用户此前显式关闭自启动」误判成默认开启，
  // 静默升级时会重新启用用户已关闭的自启动）
  G_AutostartWasDisabled :=
    (WatchdogTaskExists() and (not RegValueExists(
      HKEY_CURRENT_USER, RunKeyPath, RunValueName()))) or
    (LegacyWatchdogTaskExists() and (not RegValueExists(
      HKEY_CURRENT_USER, RunKeyPath, LegacyRunValueName())));
  // DISABLE 本名之外还要 DISABLE 旧名（回归复审修复）：legacy 升级的整个
  // 文件复制阶段旧任务仍指向旧目录的托盘，不禁用会被 watchdog 每分钟拉起
  // 撞"文件被占用"（审查 P1）；新指纹任务此时尚不存在，DISABLE 失败仅记日志。
  // 两者都回读「计划任务状态」字段校验（0.3.1 教训：/Change 退出码为 0 不代表
  // 终态生效；勿用 CSV 模式列——其对已禁用运行中任务误显示「正在运行」）。
  DisableTaskVerified(WatchdogTaskName());
  if not DisableTaskVerified(LegacyWatchdogTaskName()) then
    Log('Legacy watchdog task DISABLE skipped/failed (not upgraded or already removed)');
  KillLockedApps();
end;

// 自启动终态收敛：写 Run 值或"重建任务+DISABLE+清 Run 值"，统一在 ssDone
//（[Run] 全部执行完之后）调用，DISABLE/清理不会再被后续步骤覆盖。
procedure FinalizeAutostart();
begin
  if WizardIsTaskSelected('autostart') and
     not (WizardSilent and G_AutostartWasDisabled) then begin
    // 交互勾选或静默默认勾选：任务重建为 ENABLE 指向本次目录，并写 Run 值
    //（原 [Registry] 段移到此成功路径，失败/回滚后天然不残留）
    if not RebuildWatchdogTask() then
      Log('FinalizeAutostart: watchdog task rebuild failed');
    RegWriteStringValue(HKEY_CURRENT_USER, RunKeyPath, RunValueName(),
      '"' + ExpandConstant('{app}') + '\{#AppExeName}" --minimized');
    LogWatchdogTaskState();
  end else begin
    // 未勾选 / 静默升级尊重此前显式关闭：重建指向新目录 + 保持 DISABLE
    //（自启动审查：UsePreviousTasks=no 曾让每次覆盖升级把用户显式关闭的
    // 自启动静默改回开启；静默升级尊重升级前的关闭态）
    RebuildTaskDisabled();
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    G_PostInstallDone := True;
  if CurStep = ssDone then begin
    // 审查 P1-4：双写，避免升级场景半开半关——计划任务与 Run 值在这里统一
    // 收敛终态（此前散在 [Run] 段与 [Registry] 段，退出码被吞、失败不清理）
    FinalizeAutostart();
    // 旧名/畸名按指向归属清扫（新名终态收敛完之后再清，防升级窗口空档）
    CleanupAutostartEntries();
    // 安装目录写入用户 PATH（README/官网 MCP 配置假设裸命令 kynoptic 可用）
    AddInstallDirToUserPath();
    // 顺带清理指向已消失 Temp 目录的孤儿 PATH 项（见函数注释）
    RemoveOrphanTempPathEntries();
  end;
end;

// 安装失败/取消补偿：DeinitializeSetup 在向导以任何方式结束时都会被调用
//（包括文件复制失败与用户取消），但安装进程被 taskkill /F 强杀时不会执行。
// 补偿不再依赖安装前的一次性快照，以机器实际状态（任务存在性 + 进程）为
// 输入做幂等恢复并回读校验；进程被强杀留下的半升级孤儿态由组件启动路径的
// 自检兜底（tray/watchdog 启动逻辑，platform 域配合）。
procedure DeinitializeSetup();
var
  CmdResult: Integer;
begin
  // 初始化没走完就结束（命令行检测抛异常等）：什么都不补偿——此时 {app}
  // 未初始化，任何 ExpandConstant 都会再炸一次，且可能误动生产/另一目录的
  // 计划任务（0.3.1 发布门禁实测教训）。
  if not G_SetupInitialized then Exit;
  if G_PostInstallDone then
    Exit;
  if WatchdogTaskExists() then begin
    if RunHidden('schtasks', '/Change /TN "' + WatchdogTaskName() + '" /ENABLE') then
      Log('compensation: watchdog task re-enabled')
    else
      Log('compensation: watchdog task ENABLE failed');
    // 回读校验，DISABLE 未恢复会在日志留下痕迹
    LogWatchdogTaskState();
  end;
  // legacy 任务的补偿与 G_AutostartWasDisabled 的双名判断对称（回归复审修复：
  // legacy→新指纹升级失败回退后，legacy 看门狗任务停留 DISABLE 每分钟脱岗）。
  // legacy 任务存在且升级前用户未显式关闭自启动时一并 /ENABLE 并回读；此前
  // 已显式关闭（快照 True）则保持 DISABLE，不违背用户选择。
  if LegacyWatchdogTaskExists() and (not G_AutostartWasDisabled) then begin
    if RunHidden('schtasks', '/Change /TN "' + LegacyWatchdogTaskName() + '" /ENABLE') then
      Log('compensation: legacy watchdog task re-enabled')
    else
      Log('compensation: legacy watchdog task ENABLE failed');
    LogWatchdogTaskStateFor(LegacyWatchdogTaskName());
  end;
  // 旧托盘仍在 {app}（复制未完成的残留也以存在性为准）且未在运行时拉回
  // 后台运行；先探测避免与仍在跑的托盘并存
  if FileExists(ExpandConstant('{app}\kynoptic-tray.exe')) and
     not ProcessRunning('kynoptic-tray.exe') then
    Exec(ExpandConstant('{app}\kynoptic-tray.exe'), '--minimized', '',
      SW_HIDE, ewNoWait, CmdResult);
end;

// ============================ 卸载 ============================

var
  DeleteDataOnUninstall: Boolean;

procedure CurUninstallStepChanged(CurStep: TUninstallStep);
var
  AppDir, DataDir, HomeSkillDir: String;
  I: Integer;
  Leftovers: array[0..13] of String;
begin
  if CurStep = usUninstall then begin
    // 审查 P1-6：先静默杀掉本安装目录的 tray 与 watchdog，防止文件占用导致
    // 卸载残留（按路径精确杀，不扫杀其它安装目录的同名进程）
    KillProcessesInDir(ExpandConstant('{app}'));

    AppDir := ExpandConstant('{app}');
    DataDir := AppDir + '\data';
    // 卸载询问（默认"否"，即默认保留数据）
    // 静默卸载（/VERYSILENT /SUPPRESSMSGBOXES）下 MsgBox 不会消失会挂死：
    // 静默时跳过询问，默认保留数据（安全侧）。
    if UninstallSilent then
      DeleteDataOnUninstall := False
    else begin
      DeleteDataOnUninstall :=
        (MsgBox('是否同时删除用户数据目录？ / Also delete the user data directory?' #13#10 + DataDir + #13#10#13#10 +
                '（包含 kynoptic.db 与 settings.json，选"否"则保留数据）' #13#10 +
                '(contains kynoptic.db and settings.json; choose No to keep your data)',
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES);
      // 审查：用户选择删除后给一次不可逆警告（仅交互模式；数据一旦删除，
      // 历史记录无法找回，重装也不会恢复）
      if DeleteDataOnUninstall then
        MsgBox('警告：删除后所有历史数据将无法找回，重新安装也不会恢复。 / ' +
               'Warning: deleted history cannot be recovered, reinstalling will not bring it back.' #13#10 +
               '如需保留数据请取消本次卸载并重新选择"否"。 / To keep data, cancel and answer No.',
               mbInformation, MB_OK);
    end;

    // 清理运行期残留文件（忽略不存在的情况）
    Leftovers[0] := AppDir + '\tray-exit.flag';
    Leftovers[1] := AppDir + '\kynoptic-heartbeat';
    Leftovers[2] := AppDir + '\watchdog.log';
    Leftovers[3] := AppDir + '\tray-error.log';
    Leftovers[4] := AppDir + '\settings-audit.log';
    Leftovers[5] := AppDir + '\watchdog.log.old';
    Leftovers[6] := AppDir + '\watchdog-state.json';
    Leftovers[7] := AppDir + '\dashboard-port.txt';
    Leftovers[8] := AppDir + '\dashboard-error.log';
    Leftovers[9] := AppDir + '\kynoptic-tray.exe.bak';
    Leftovers[10] := AppDir + '\kynoptic.exe.bak';
    Leftovers[11] := AppDir + '\kynoptic-watchdog.exe.bak';
    Leftovers[12] := AppDir + '\watchdog.lock';
    Leftovers[13] := AppDir + '\SKILL.md.tmp';
    for I := 0 to 13 do begin
      if FileExists(Leftovers[I]) then
        DeleteFile(Leftovers[I]);
    end;
  end;

  if CurStep = usPostUninstall then begin
    AppDir := ExpandConstant('{app}');
    DataDir := AppDir + '\data';
    // Run 自启动值已从 [Registry] 段移除，卸载时由这里对称删除
    //（原 uninsdeletevalue 的等价物）；看门狗任务对称删除同理（任务名含
    // 运行期指纹，须用 [Code] 函数现算，不能在 [UninstallRun] 静态展开）。
    // 本名精确删之后，其余历史名（legacy/畸名/更旧指纹名）按指向归属清扫：
    // 指向本次 {app} 或孤儿才删，本机其它活安装（含同后缀异目录沙箱）不误删。
    RegDeleteValue(HKEY_CURRENT_USER, RunKeyPath, RunValueName());
    RunHidden('schtasks', '/Delete /F /TN "' + WatchdogTaskName() + '"');
    CleanupAutostartEntries();
    // 安装时写入用户 PATH 的安装目录项，卸载时对称移除（整项全等才剔除，
    // 不动用户自配条目）；顺带清理已消失 Temp 目录的孤儿项
    RemoveInstallDirFromUserPath();
    RemoveOrphanTempPathEntries();
    // skill install 对称清理（卸载审查 + 归属比对）：skill install 向三个
    // AI 客户端 home 目录写入 skills/<SkillDirName>/SKILL.md。卸载只删
    // 「指纹指向本次安装目录」或「无指纹的旧版残留」；指纹指向别处（另一
    // 份安装的技能）保持不动，异目录安装/卸载不再互删技能。
    for I := 0 to 2 do begin
      if I = 0 then
        HomeSkillDir := ExpandConstant('{%USERPROFILE}') + '\.zcode\skills\' + SkillDirName()
      else if I = 1 then
        HomeSkillDir := ExpandConstant('{%USERPROFILE}') + '\.claude\skills\' + SkillDirName()
      else
        HomeSkillDir := ExpandConstant('{%USERPROFILE}') + '\.cursor\skills\' + SkillDirName();
      if DirExists(HomeSkillDir) then begin
        case SkillMarkerState(HomeSkillDir) of
          0: DelTree(HomeSkillDir, True, True, True);
          1: Log('skill dir belongs to another install, kept: ' + HomeSkillDir);
        else
          DelTree(HomeSkillDir, True, True, True);
        end;
      end;
    end;
    if DeleteDataOnUninstall then begin
      DelTree(DataDir, True, True, True);
    end else if DirExists(DataDir) then begin
      // 卸载完成提示：数据已保留
      if UninstallProgressForm <> nil then
        UninstallProgressForm.StatusLabel.Caption :=
          '数据已保留 / Data kept at ' + DataDir;
    end;
  end;
end;

// 原卸载收尾的看门狗任务删除已移入 usPostUninstall 的 [Code]（任务名含
// 运行期安装目录指纹，须现算，不能在此静态展开）。

[CustomMessages]
english.AutoStartTask =Start Kynoptic automatically at login (includes the crash-watchdog task; unchecking disables both)
chinesesimplified.AutoStartTask =开机自动启动 Kynoptic（含崩溃自动拉起看门狗任务；取消勾选则两者一并停用）

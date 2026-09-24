; Kynoptic 安装器脚本（Inno Setup 6）
; CI: iscc kynoptic.iss /DAppVersion=<版本>（必须外部传入，见下）
; 版本漂移审查：忘传 /DAppVersion 时曾打包出 0.2.0 安装包而二进制是 0.2.2
; 的自相矛盾产物。本文件不再持有版本号缺省值，只认 /DAppVersion。

#ifndef AppVersion
#error 请以 /DAppVersion=<版本> 显式传入版本号（如 scripts/bump-version.mjs 写入 Cargo.toml 的值）
#endif

; 测试隔离后缀（/DTestSuffix=sbox1）：带后缀编译出的安装包使用独立任务名与
; Run 值名、独立输出文件名，可与真实安装并存做沙箱演练。仅测试构建使用，
; 生产构建不带此参数，命名与历史版本完全一致。AppId 仍为全局唯一（见下），
; 后缀不能隔离卸载记录，异目录演练卸载时仍会互删注册信息。
#ifdef TestSuffix
#define WatchdogTaskName "Kynoptic Watchdog-" + TestSuffix
#define RunValueName "Kynoptic-" + TestSuffix
#define OutTag "-" + TestSuffix
#else
#define WatchdogTaskName "Kynoptic Watchdog"
#define RunValueName "Kynoptic"
#define OutTag ""
#endif

#define AppName "Kynoptic"
#define AppPublisher "ardss"
#define AppExeName "kynoptic-tray.exe"

[Setup]
; 升级不记忆上次的任务勾选（审查 P1：旧版本默认未勾 autostart，记忆会让
; 修复后的默认勾选在升级场景永远不生效）。
UsePreviousTasks=no
; 永久不可改动：AppId 是升级/卸载识别的惟一键，改动后旧版本无法被新安装包
; 覆盖升级，会装出第二份程序并留下无法卸载的旧记录。
AppId={{E761967B-0337-4F1A-B522-1C45B57AFF1D}
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
; AI 客户端 skill 同步（静默、总是执行；升级覆盖安装也会刷新 SKILL.md）
Filename: "{app}\kynoptic.exe"; Parameters: "skill install"; Flags: runhidden
; 说明性 Description：让用户知道启动的是后台托盘进程而非窗口程序
Filename: "{app}\{#AppExeName}"; Description: "Launch Kynoptic (background tray - right-click the tray icon to open the dashboard / 启动 Kynoptic 后台托盘，右键托盘图标打开面板)"; Flags: nowait postinstall skipifsilent
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

// 静默执行外部命令（SW_HIDE 不弹窗），返回是否成功且退出码为 0
function RunHidden(const Exe, Params: String): Boolean;
var
  CmdResult: Integer;
begin
  Result := Exec(Exe, Params, '', SW_HIDE, ewWaitUntilTerminated, CmdResult);
  if Result then
    Result := (CmdResult = 0);
end;

// 看门狗计划任务是否已存在（升级识别）
function WatchdogTaskExists(): Boolean;
var
  CmdResult: Integer;
begin
  Result := Exec('schtasks', '/Query /TN "{#WatchdogTaskName}"', '',
    SW_HIDE, ewWaitUntilTerminated, CmdResult) and (CmdResult = 0);
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

// 回读看门狗任务的状态/指向并写安装日志（状态文案随系统语言变化，
// 英文 Disabled / 中文 已禁用，仅记录不判失败）
procedure LogWatchdogTaskState();
var
  TmpFile: String;
  CmdResult: Integer;
  Lines: TArrayOfString;
  I: Integer;
  S: String;
begin
  TmpFile := ExpandConstant('{tmp}') + '\kyn-task-query.txt';
  Exec('cmd.exe', '/C schtasks /Query /TN "{#WatchdogTaskName}" /FO LIST /V > "' + TmpFile + '" 2>&1',
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

// 重建指向本次安装目录的看门狗任务：先删旧任务再建（/Create 一旦执行必刷新
// StartBoundary，/Delete + /Create 保证不留半新半旧状态），/Create 带重试、
// 退出码入日志，最后回读校验。任何失败都留日志，不再无声吞掉。
function RebuildWatchdogTask(): Boolean;
var
  I: Integer;
begin
  Result := False;
  // 旧任务可能指向旧 {app} 的死路径；不存在时 /Delete 失败，忽略
  RunHidden('schtasks', '/Delete /F /TN "{#WatchdogTaskName}"');
  for I := 1 to 3 do begin
    if RunHidden('schtasks', '/Create /F /SC MINUTE /MO 1 /TN "{#WatchdogTaskName}" /TR "' + Chr(39) +
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
  RunHidden('schtasks', '/Change /TN "{#WatchdogTaskName}" /DISABLE');
  RegDeleteValue(HKEY_CURRENT_USER, RunKeyPath, '{#RunValueName}');
  LogWatchdogTaskState();
end;

// 识别 NSIS 风格的独立 "/S" 参数（/SILENT、/SUPPRESSMSGBOXES 是完整开关，
// 不算命中）
function HasSwitchS(): Boolean;
var
  Cmd: String;
  P: Integer;
begin
  Result := False;
  Cmd := Uppercase(Trim(ExpandConstant('{cmdline}')));
  P := Pos('/S', Cmd);
  while P > 0 do begin
    if (P + 1 >= Length(Cmd)) or (Cmd[P + 2] = ' ') or (Cmd[P + 2] = '"') or (Cmd[P + 2] = #9) then begin
      Result := True;
      Exit;
    end;
    Cmd := Copy(Cmd, P + 2, MaxInt);
    P := Pos('/S', Cmd);
  end;
end;

// ============================ 安装 ============================

function InitializeSetup(): Boolean;
begin
  Result := True;
  G_PostInstallDone := False;
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
  G_PostInstallDone := False;
  G_AutostartWasDisabled := WatchdogTaskExists() and (not RegValueExists(
    HKEY_CURRENT_USER, RunKeyPath, '{#RunValueName}'));
  if not RunHidden('schtasks', '/Change /TN "{#WatchdogTaskName}" /DISABLE') then
    Log('Watchdog task DISABLE skipped/failed');
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
    RegWriteStringValue(HKEY_CURRENT_USER, RunKeyPath, '{#RunValueName}',
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
    // 安装目录写入用户 PATH（README/官网 MCP 配置假设裸命令 kynoptic 可用）
    AddInstallDirToUserPath();
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
  if G_PostInstallDone then
    Exit;
  if WatchdogTaskExists() then begin
    if RunHidden('schtasks', '/Change /TN "{#WatchdogTaskName}" /ENABLE') then
      Log('compensation: watchdog task re-enabled')
    else
      Log('compensation: watchdog task ENABLE failed');
    // 回读校验，DISABLE 未恢复会在日志留下痕迹
    LogWatchdogTaskState();
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
    //（原 uninsdeletevalue 的等价物）
    RegDeleteValue(HKEY_CURRENT_USER, RunKeyPath, '{#RunValueName}');
    // 安装时写入用户 PATH 的安装目录项，卸载时对称移除（整项全等才剔除，
    // 不动用户自配条目）
    RemoveInstallDirFromUserPath();
    // skill install 对称清理（卸载审查）：skill install 曾向三个 AI 客户端
    // home 目录写入 skills/kynoptic/SKILL.md，卸载后残留死技能（指引指向
    // 已不存在的 %LOCALAPPDATA%\Programs\Kynoptic）。整树删除。
    for I := 0 to 2 do begin
      if I = 0 then
        HomeSkillDir := ExpandConstant('{%USERPROFILE}') + '\.zcode\skills\kynoptic'
      else if I = 1 then
        HomeSkillDir := ExpandConstant('{%USERPROFILE}') + '\.claude\skills\kynoptic'
      else
        HomeSkillDir := ExpandConstant('{%USERPROFILE}') + '\.cursor\skills\kynoptic';
      if DirExists(HomeSkillDir) then
        DelTree(HomeSkillDir, True, True, True);
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

[UninstallRun]
Filename: "schtasks"; Parameters: "/Delete /F /TN ""{#WatchdogTaskName}"""; Flags: runhidden; RunOnceId: "DelWatchdog"

[CustomMessages]
english.AutoStartTask =Start Kynoptic automatically at login (includes the crash-watchdog task; unchecking disables both)
chinesesimplified.AutoStartTask =开机自动启动 Kynoptic（含崩溃自动拉起看门狗任务；取消勾选则两者一并停用）

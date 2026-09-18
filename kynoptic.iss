; Kynoptic 安装器脚本（Inno Setup 6）
; CI: iscc kynoptic.iss /DAppVersion=0.1.0

#ifndef AppVersion
#define AppVersion "0.2.0"
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
OutputBaseFilename=Kynoptic-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
PrivilegesRequired=lowest
; force:托盘是无可视窗口的后台进程,收不到 WM_CLOSE,普通等待会永远卡在
; "正在关闭应用";直接终止旧进程(SQLite WAL 模式,任意时刻终止都安全)。
CloseApplications=force

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Files]
Source: "dist\kynoptic-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\kynoptic.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dist\kynoptic-watchdog.exe"; DestDir: "{app}"; Flags: ignoreversion
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

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Kynoptic"; ValueData: """{app}\{#AppExeName}"" --minimized"; Tasks: autostart; Flags: uninsdeletevalue

[Run]
; AI 客户端 skill 同步（静默、总是执行；升级覆盖安装也会刷新 SKILL.md）
Filename: "{app}\kynoptic.exe"; Parameters: "skill install"; Flags: runhidden
; 说明性 Description：让用户知道启动的是后台托盘进程而非窗口程序
Filename: "{app}\{#AppExeName}"; Description: "Launch Kynoptic (background tray - right-click the tray icon to open the dashboard / 启动 Kynoptic 后台托盘，右键托盘图标打开面板)"; Flags: nowait postinstall skipifsilent
; 看门狗:计划任务每分钟跑一次 `kynoptic watchdog --once`,托盘被杀/崩溃时自动拉起;
; 用户从托盘菜单主动退出则写旗标,watchdog 不拉起。随 autostart 任务一起安装。
; /F 覆盖建:换目录升级时旧任务指向旧 {app} 成为死任务,/F 保证任务重建后
; 必指向本次安装目录(审查 P1-10)。
Filename: "schtasks"; Parameters: "/Create /F /SC MINUTE /MO 1 /TN ""Kynoptic Watchdog"" /TR ""'{app}\kynoptic-watchdog.exe' watchdog --once"""; Flags: runhidden

[Code]
// ============================ 通用辅助 ============================

// 静默执行外部命令（SW_HIDE 不弹窗），返回是否成功且退出码为 0
function RunHidden(const Exe, Params: String): Boolean;
var
  CmdResult: Integer;
begin
  Result := Exec(Exe, Params, '', SW_HIDE, ewWaitUntilTerminated, CmdResult);
  if Result then
    Result := (CmdResult = 0);
end;

// 静默强杀进程树（审查 P1-5：DISABLE 计划任务防不住已在跑的 watchdog）
procedure KillProcessSilently(const Image: String);
begin
  // 忽略返回码：进程本来就没在跑时 taskkill 会返回非 0
  RunHidden('taskkill', '/F /IM "' + Image + '" /T');
end;

// ============================ 安装 ============================

// 安装前禁用看门狗计划任务：CloseApplications 只处理已持句柄的进程，
// 防不住 watchdog 每分钟把旧托盘拉起来撞"文件被占用"（审查 P1）；
// 追加强杀残留 watchdog（忽略返回码、静默不弹窗）。
function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not RunHidden('schtasks', '/Change /TN "Kynoptic Watchdog" /DISABLE') then
    Log('Watchdog task DISABLE skipped/failed');
  KillProcessSilently('kynoptic-watchdog.exe');
  // 审查 P1：CloseApplications=force 与 PrepareToInstall 之间有窗口，watchdog
  // 每 15s/每分钟都可能把托盘拉回来重新锁住 {app}\kynoptic-tray.exe，导致
  // ignoreversion 复制失败且静默升级无声中止。这里显式补杀托盘。
  KillProcessSilently('kynoptic-tray.exe');
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then begin
    // 审查 P1-4：双写，避免升级场景半开半关。
    // 计划任务已无条件 /Create（静默安装不勾任务也会建——此前静默升级会
    // 静默丢看门狗）。未勾选 autostart：任务转 DISABLE（防无人值守拉起），
    // 并清掉旧安装残留的 Run 值。
    if not WizardIsTaskSelected('autostart') then begin
      RunHidden('schtasks', '/Change /TN "Kynoptic Watchdog" /DISABLE');
      RegDeleteValue(HKEY_CURRENT_USER,
        'Software\Microsoft\Windows\CurrentVersion\Run', 'Kynoptic');
    end;
  end;
end;

// ============================ 卸载 ============================

var
  DeleteDataOnUninstall: Boolean;

procedure CurUninstallStepChanged(CurStep: TUninstallStep);
var
  AppDir, DataDir: String;
  I: Integer;
  Leftovers: array[0..13] of String;
begin
  if CurStep = usUninstall then begin
    // 审查 P1-6：先静默杀掉 tray 与 watchdog，防止文件占用导致卸载残留
    KillProcessSilently('kynoptic-tray.exe');
    KillProcessSilently('kynoptic-watchdog.exe');

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
Filename: "schtasks"; Parameters: "/Delete /F /TN ""Kynoptic Watchdog"""; Flags: runhidden; RunOnceId: "DelWatchdog"

[CustomMessages]
english.AutoStartTask =Start Kynoptic automatically at login (with crash watchdog)
chinesesimplified.AutoStartTask =开机自动启动 Kynoptic（含崩溃自动拉起看门狗）

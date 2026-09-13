; Kynoptic 安装器脚本（Inno Setup 6）
; CI: iscc kynoptic.iss /DAppVersion=0.1.0

#define AppName "Kynoptic"
#define AppPublisher "ardss"
#define AppExeName "kynoptic-tray.exe"

[Setup]
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

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\{#AppName} CLI"; Filename: "{app}\kynoptic.exe"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "autostart"; Description: "{cm:AutoStartTask}"; Flags: unchecked

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Kynoptic"; ValueData: """{app}\{#AppExeName}"" --minimized"; Tasks: autostart; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; 看门狗:计划任务每分钟跑一次 `kynoptic watchdog --once`,托盘被杀/崩溃时自动拉起;
; 用户从托盘菜单主动退出则写旗标,watchdog 不拉起。随 autostart 任务一起安装。
; /F 覆盖建:换目录升级时旧任务指向旧 {app} 成为死任务,/F 保证任务重建后
; 必指向本次安装目录(审查 P1-10)。
Filename: "schtasks"; Parameters: "/Create /F /SC MINUTE /MO 1 /TN ""Kynoptic Watchdog"" /TR ""'{app}\kynoptic-watchdog.exe' watchdog --once"""; Tasks: autostart; Flags: runhidden

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
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then begin
    // 审查 P1-4：双写，避免升级场景半开半关。
    // 勾选 autostart：注册表 Run 值由 [Registry] 段按任务勾选写入，
    // 计划任务由 [Run] 段 /Create /F 重建（覆盖旧 DISABLE 僵尸/旧目录死任务）。
    // 未勾选：清掉旧安装留下的 DISABLE 僵尸任务与残留 Run 值。
    if not WizardIsTaskSelected('autostart') then begin
      RunHidden('schtasks', '/Delete /F /TN "Kynoptic Watchdog"');
      RegDeleteStringValue(HKEY_CURRENT_USER,
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
  Leftovers: array[0..4] of String;
begin
  if CurStep = usUninstall then begin
    // 审查 P1-6：先静默杀掉 tray 与 watchdog，防止文件占用导致卸载残留
    KillProcessSilently('kynoptic-tray.exe');
    KillProcessSilently('kynoptic-watchdog.exe');

    AppDir := ExpandConstant('{app}');
    DataDir := AppDir + '\data';
    // 卸载询问（默认"否"，即默认保留数据）
    DeleteDataOnUninstall :=
      (MsgBox('是否同时删除用户数据目录？' #13#10 + DataDir + #13#10#13#10 +
              '（包含 kynoptic.db 与 settings.json，选"否"则保留数据）',
              mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES);

    // 清理运行期残留文件（忽略不存在的情况）
    Leftovers[0] := AppDir + '\tray-exit.flag';
    Leftovers[1] := AppDir + '\kynoptic-heartbeat';
    Leftovers[2] := AppDir + '\watchdog.log';
    Leftovers[3] := AppDir + '\tray-error.log';
    Leftovers[4] := AppDir + '\settings-audit.log';
    for I := 0 to 4 do begin
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
          '数据已保留在 ' + DataDir;
    end;
  end;
end;

[UninstallRun]
Filename: "schtasks"; Parameters: "/Delete /F /TN ""Kynoptic Watchdog"""; Flags: runhidden; RunOnceId: "DelWatchdog"

[CustomMessages]
english.AutoStartTask =Start Kynoptic automatically at login (with crash watchdog)
chinesesimplified.AutoStartTask =开机自动启动 Kynoptic（含崩溃自动拉起看门狗）

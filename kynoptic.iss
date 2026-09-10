; Kynoptic 安装器脚本（Inno Setup 6）
; CI: iscc kynoptic.iss /DAppVersion=0.1.0

#define AppName "Kynoptic"
#define AppPublisher "ardss"
#define AppExeName "kynoptic-tray.exe"

[Setup]
AppId={{8A6E2F3B-4C1D-4E7A-9B2F-KYNOPTIC001}}
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
Filename: "schtasks"; Parameters: "/Create /F /SC MINUTE /MO 1 /TN ""Kynoptic Watchdog"" /TR ""'{app}\kynoptic.exe' watchdog --once"""; Tasks: autostart; Flags: runhidden

[UninstallRun]
Filename: "schtasks"; Parameters: "/Delete /F /TN ""Kynoptic Watchdog"""; Flags: runhidden; RunOnceId: "DelWatchdog"

[CustomMessages]
english.AutoStartTask =Start Kynoptic automatically at login (with crash watchdog)
chinesesimplified.AutoStartTask =开机自动启动 Kynoptic（含崩溃自动拉起看门狗）

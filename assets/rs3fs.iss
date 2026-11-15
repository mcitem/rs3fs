#define AppName "rs3fs"
#define AppExeName "rs3fs.exe"
#define AppExe AddBackslash(SourcePath) + "..\target\release\" + AppExeName
#define AppVersion GetFileVersionString(AppExe)

#if AppVersion == ""
  #error Build the Windows x64 release with Cargo before compiling this installer.
#endif

#ifndef AppRuntime
  #error Run cargo run --locked --example build-installer to locate the Visual C++ redistributable.
#endif

[Setup]
AppId={{1589CAA6-96E1-46A4-AD0A-7BEC3788774B}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=mcitem
AppCopyright=Copyright © 2026 mcitem
DefaultDirName={localappdata}\Programs\{#AppName}
DisableDirPage=no
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\target\installer
OutputBaseFilename={#AppName}-{#AppVersion}-windows-x64-setup
SetupIconFile=s3fs.ico
UninstallDisplayIcon={app}\{#AppExeName}
VersionInfoVersion={#GetVersionNumbersString(AppExe)}
VersionInfoTextVersion={#AppVersion}
VersionInfoProductTextVersion={#AppVersion}
WizardStyle=modern
LanguageDetectionMethod=none
Compression=lzma2
SolidCompression=yes
CloseApplicationsFilter={#AppExeName}
RestartApplications=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
Source: "{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#AppRuntime}"; DestDir: "{app}"
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\{#AppExeName}"; Parameters: "--remove-own-startup"; RunOnceId: "RemoveOwnStartup"; Flags: runhidden skipifdoesntexist

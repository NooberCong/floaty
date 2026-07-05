; Floaty installer — per-user, no admin required.
; Build: ISCC.exe res\setup.iss   (output → target\dist\FloatySetup-<version>.exe)

#define AppVersion "0.1.0"

[Setup]
AppId={{7A1FDA37-6E2C-4C51-9B45-8E5D0F6A2C11}
AppName=Floaty
AppVersion={#AppVersion}
AppPublisher=NooberCong
AppPublisherURL=https://github.com/NooberCong/floaty
AppSupportURL=https://github.com/NooberCong/floaty/issues
DefaultDirName={autopf}\Floaty
; Per-user install: {autopf} resolves to %LOCALAPPDATA%\Programs, no UAC.
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
DisableDirPage=yes
OutputDir=..\target\dist
OutputBaseFilename=FloatySetup-{#AppVersion}
SetupIconFile=floaty.ico
UninstallDisplayIcon={app}\floaty.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; Floaty's single-instance mutex: prompts to close a running Floaty first.
AppMutex=Local\FloatySingleInstance
LicenseFile=..\LICENSE

[Files]
Source: "..\target\release\floaty.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Floaty"; Filename: "{app}\floaty.exe"

[Run]
Filename: "{app}\floaty.exe"; Description: "Launch Floaty"; Flags: nowait postinstall skipifsilent

[Registry]
; Floaty writes this itself when "Start with Windows" is enabled; make sure
; the uninstaller removes it so no dangling autostart survives.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "Floaty"; Flags: dontcreatekey uninsdeletevalue

[UninstallRun]
; Stop a running Floaty so the exe can be deleted.
Filename: "{cmd}"; Parameters: "/C taskkill /IM floaty.exe /F"; Flags: runhidden skipifdoesntexist; RunOnceId: "KillFloaty"

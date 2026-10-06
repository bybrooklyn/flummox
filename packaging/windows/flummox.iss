#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef SourceDir
  #error SourceDir is required
#endif
#ifndef AppNumericVersion
  #define AppNumericVersion AppVersion
#endif

[Setup]
AppId={{D0F57A65-E179-41A1-9096-EC094AF43D89}
AppName=Flummox
AppVersion={#AppVersion}
VersionInfoVersion={#AppNumericVersion}
VersionInfoProductVersion={#AppNumericVersion}
VersionInfoTextVersion={#AppVersion}
VersionInfoProductTextVersion={#AppVersion}
AppPublisher=Brooklyn
AppPublisherURL=https://github.com/bybrooklyn/flummox
AppSupportURL=https://github.com/bybrooklyn/flummox/issues
DefaultDirName={localappdata}\Programs\Flummox
DefaultGroupName=Flummox
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=flummox-{#AppVersion}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
LicenseFile=..\..\LICENSE
UninstallDisplayIcon={app}\flummox-gui.exe
CloseApplications=yes
RestartApplications=no

[Files]
Source: "{#SourceDir}\flummox.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\flummox-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\RELEASE-NOTES.md"; DestDir: "{app}"; Flags: ignoreversion

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Icons]
Name: "{group}\Flummox"; Filename: "{app}\flummox-gui.exe"
Name: "{group}\Uninstall Flummox"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Flummox"; Filename: "{app}\flummox-gui.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\flummox-gui.exe"; Description: "Launch Flummox"; Flags: nowait postinstall skipifsilent

[Code]
function InitializeSetup(): Boolean;
begin
  Result := True;
  if not WizardSilent then
    Result := MsgBox('Close Flummox and finish all compression jobs before installing or upgrading.', mbInformation, MB_OKCANCEL) = IDOK;
end;

function StopOwnedWorker(RemoveStartup: Boolean): Boolean;
var
  ExitCode: Integer;
  Arguments: String;
begin
  Result := True;
  if not FileExists(ExpandConstant('{app}\flummox.exe')) then Exit;
  Arguments := '--native-worker-exit';
  if RemoveStartup then Arguments := Arguments + ' --remove-owned-startup';
  Result := Exec(ExpandConstant('{app}\flummox.exe'), Arguments, '', SW_HIDE, ewWaitUntilTerminated, ExitCode);
  if Result then
    { Legacy CLI versions return 2 for an unsupported worker command. }
    Result := (ExitCode = 0) or (ExitCode = 2);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not StopOwnedWorker(False) then
    Result := 'Flummox is finishing a storage operation. Wait, then retry the upgrade.';
end;

function InitializeUninstall(): Boolean;
begin
  Result := StopOwnedWorker(True);
  if not Result and not UninstallSilent then
    MsgBox('Flummox is finishing a storage operation. Wait, then retry uninstalling.', mbError, MB_OK);
end;

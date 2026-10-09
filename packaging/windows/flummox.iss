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
; Release builds generate this file and check it before compiling the
; installer. The CI install test has no notices to ship.
Source: "..\..\THIRD-PARTY-LICENSES.txt"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
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

var
  WorkerProblem: String;

{ Stops this install's background worker. Returns True when none is running any
  more, whether it was never running, left before the request or finished its file.
  --native-worker-exit exits 0 when the worker is gone and 3 while it is still
  finishing a file. Anything else leaves WorkerProblem set. }
function StopOwnedWorker(RemoveStartup: Boolean): Boolean;
var
  ExitCode: Integer;
  Arguments: String;
begin
  Result := True;
  WorkerProblem := '';
  if not FileExists(ExpandConstant('{app}\flummox.exe')) then Exit;
  Arguments := '--native-worker-exit';
  if RemoveStartup then Arguments := Arguments + ' --remove-owned-startup';
  if not Exec(ExpandConstant('{app}\flummox.exe'), Arguments, '', SW_HIDE, ewWaitUntilTerminated, ExitCode) then
  begin
    { Exec failed, so ExitCode is a Windows error. 2 and 3 mean the file or its
      folder vanished after the check above, so there is no worker to wait for. }
    Result := (ExitCode = 2) or (ExitCode = 3);
    if not Result then
      WorkerProblem := Format('Flummox could not ask its background worker to stop (Windows error %d).', [ExitCode]);
    Exit;
  end;
  { Legacy CLI versions return 2 for an unsupported worker command. }
  Result := (ExitCode = 0) or (ExitCode = 2);
  if ExitCode = 3 then
    WorkerProblem := 'Flummox is finishing a storage operation.'
  else if not Result then
    WorkerProblem := Format('Flummox could not stop its background worker (code %d). Quit it from the tray icon.', [ExitCode]);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not StopOwnedWorker(False) then
    Result := WorkerProblem + ' Wait, then retry the upgrade.';
end;

function InitializeUninstall(): Boolean;
begin
  Result := StopOwnedWorker(True);
  if not Result and not UninstallSilent then
    MsgBox(WorkerProblem + ' Wait, then retry uninstalling.', mbError, MB_OK);
end;

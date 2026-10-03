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
CloseApplications=no
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

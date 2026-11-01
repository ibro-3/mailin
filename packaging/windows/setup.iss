; Mailin Inno Setup script
; Compile with: iscc setup.iss
; Output: mailin-setup-<version>.exe

#define AppName "Mailin"
#define AppVersion "0.1.0"
#define AppPublisher "Mailin Contributors"
#define AppURL "https://github.com/mailin-app/mailin"
#define AppExeName "mailin-winui.exe"
#define AppIcon "Assets\Square44x44Logo.ico"

[Setup]
AppId={{1A2B3C4D-5E6F-7G8H-9I0J-KLMNOPQRSTUV}}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
AppUpdatesURL={#AppURL}
DefaultDirName={localappdata}\{#AppName}
DefaultGroupName={#AppName}
AllowNoIcons=yes
OutputDir=..
OutputBaseFilename=mailin-setup-{#AppVersion}
Compression=lzma
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
SetupLogging=yes
DisableDirPage=no
DisableProgramGroupPage=yes
SetupIconFile={#AppIcon}

; Architecture
ArchitecturesInstallIn64BitMode=x64
ArchitecturesAllowed=x64

; Signing (configure for release)
; SignTool=signtool
; SignParams=/a /fd sha256 /tr http://timestamp.digicert.com /td sha256 $f

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Application executable and bootstrap DLL
Source: "mailin-winui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "Microsoft.WindowsAppRuntime.Bootstrap.dll"; DestDir: "{app}"; Flags: ignoreversion

; Windows App SDK runtime installer (run on first install)
Source: "WindowsAppRuntimeInstall-x64.exe"; DestDir: "{app}"; Flags: ignoreversion; Check: not IsWindowsAppSdkInstalled()

; Assets
Source: "Assets\*"; DestDir: "{app}\Assets"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"
Name: "{commondesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
; Install Windows App SDK runtime if not present (silent, no restart)
Filename: "{app}\WindowsAppRuntimeInstall-x64.exe"; Parameters: "/install /quiet /norestart"; StatusMsg: "Installing Windows App SDK runtime..."; Check: not IsWindowsAppSdkInstalled(); Flags: runhidden waituntilterminated
; Launch application
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Optionally uninstall Windows App SDK runtime if no other apps need it
; Filename: "{app}\WindowsAppRuntimeInstall-x64.exe"; Parameters: "/uninstall /quiet"; Check: CanUninstallWindowsAppSdk()

[Code]
var
  WindowsAppSdkInstalled: Boolean;

function IsWindowsAppSdkInstalled(): Boolean;
begin
  Result := WindowsAppSdkInstalled;
end;

function InitializeSetup(): Boolean;
var
  Key: String;
  Version: String;
begin
  { Check if Windows App SDK 1.8+ is already installed }
  { The framework package is registered under HKLM\SOFTWARE\Microsoft\WindowsAppRuntime }
  Key := 'SOFTWARE\Microsoft\WindowsAppRuntime';
  if RegKeyExists(HKLM, Key) then
  begin
    if RegQueryStringValue(HKLM, Key, 'Version', Version) then
    begin
      { Version format: "1.8.260921001" }
      if CompareStr(Version, '1.8.0') >= 0 then
        WindowsAppSdkInstalled := True;
    end;
  end;
  Result := True;
end;

function CanUninstallWindowsAppSdk(): Boolean;
begin
  { Only uninstall if we installed it and no other apps use it }
  { This is a conservative check - in practice, leave it installed }
  Result := False;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
end;
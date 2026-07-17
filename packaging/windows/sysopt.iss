#ifndef AppVersion
  #define AppVersion "0.7.0-rc1"
#endif
#ifndef BuildDir
  #define BuildDir "build"
#endif

[Setup]
AppId={{E49E20C1-4B98-48D0-B724-BD66751419C1}
AppName=SysOpt
AppVersion={#AppVersion}
AppPublisher=SysOpt
DefaultDirName={localappdata}\Programs\SysOpt
DefaultGroupName=SysOpt
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
OutputDir=output
OutputBaseFilename=SysOpt-Setup-{#AppVersion}-windows-x86_64
UninstallDisplayName=SysOpt
ChangesEnvironment=yes

[Files]
Source: "{#BuildDir}\sysopt.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "control-center.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "task-helper.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "config.install.toml"; DestDir: "{userappdata}\sysopt"; DestName: "config.toml"; Flags: onlyifdoesntexist uninsneveruninstall

[Icons]
Name: "{group}\SysOpt Control Center"; Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File ""{app}\control-center.ps1"""; WorkingDir: "{app}"
Name: "{group}\Diagnóstico SysOpt"; Filename: "{cmd}"; Parameters: "/k ""{app}\sysopt.exe"" --doctor"
Name: "{group}\Desinstalar SysOpt"; Filename: "{uninstallexe}"
Name: "{userdesktop}\SysOpt Control Center"; Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File ""{app}\control-center.ps1"""; WorkingDir: "{app}"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Crear acceso directo en el escritorio"; Flags: unchecked
Name: "autostart"; Description: "Iniciar SysOpt automáticamente al iniciar sesión"; Flags: checkedonce

[Run]
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\task-helper.ps1"" -BinPath ""{app}\sysopt.exe"" -Install -Start"; Flags: runhidden waituntilterminated; Tasks: autostart
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File ""{app}\control-center.ps1"""; Description: "Abrir SysOpt Control Center"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\sysopt.exe"; Parameters: "--shutdown"; Flags: runhidden waituntilterminated; RunOnceId: "ShutdownSysOpt"
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\task-helper.ps1"" -BinPath ""{app}\sysopt.exe"" -Remove"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveSysOptTask"


[Code]
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
  ExistingBinary: String;
begin
  Result := '';
  ExistingBinary := ExpandConstant('{app}\sysopt.exe');
  if FileExists(ExistingBinary) then
  begin
    Exec(ExistingBinary, '--shutdown', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Sleep(500);
  end;
end;

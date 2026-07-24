; Installer MDM (Inno Setup 6). Buildato dalla CI: ISCC.exe /DAppVersion=x.y.z installer.iss
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{2FE01BEC-8EC8-4F14-8CCC-1F695C82CF13}
AppName=MDM
AppVersion={#AppVersion}
AppPublisher=simiriva95
DefaultDirName={localappdata}\MDM
PrivilegesRequired=lowest
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableWelcomePage=yes
WizardStyle=modern
OutputBaseFilename=mdm-setup
OutputDir=out
Compression=lzma2
SolidCompression=yes
UninstallDisplayIcon={app}\mdm.exe

[Languages]
Name: "italian"; MessagesFile: "compiler:Languages\Italian.isl"

[Messages]
italian.FinishedLabel=MDM è installato.%nUltimo passo (una volta sola): in Chrome apri chrome://extensions, attiva "Modalità sviluppatore" e con "Carica estensione non pacchettizzata" scegli la cartella:%n%n{app}\extension

[Files]
Source: "..\app\target\release\mdm.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\extension\*"; DestDir: "{app}\extension"; Flags: recursesubdirs ignoreversion

[Registry]
Root: HKCU; Subkey: "Software\Google\Chrome\NativeMessagingHosts\com.sriva.downloader"; ValueType: string; ValueData: "{app}\com.sriva.downloader.json"; Flags: uninsdeletekey

[UninstallDelete]
Type: files; Name: "{app}\com.sriva.downloader.json"

[Code]
procedure CurStepChanged(CurStep: TSetupStep);
var
  ExePath, Manifest: string;
begin
  if CurStep = ssPostInstall then
  begin
    ExePath := ExpandConstant('{app}\mdm.exe');
    StringChangeEx(ExePath, '\', '\\', True);
    Manifest :=
      '{' + #13#10 +
      '  "name": "com.sriva.downloader",' + #13#10 +
      '  "description": "Mini Download Manager native host",' + #13#10 +
      '  "path": "' + ExePath + '",' + #13#10 +
      '  "type": "stdio",' + #13#10 +
      '  "allowed_origins": ["chrome-extension://gmffaagflamiefieafmcoipcnmajgmdh/"]' + #13#10 +
      '}' + #13#10;
    SaveStringToFile(ExpandConstant('{app}\com.sriva.downloader.json'), Manifest, False);
  end;
end;

[Run]
Filename: "chrome.exe"; Parameters: "chrome://extensions"; Description: "Apri chrome://extensions per caricare l'estensione"; Flags: postinstall shellexec skipifsilent
Filename: "{app}\mdm.exe"; Description: "Avvia MDM"; Flags: postinstall nowait skipifsilent

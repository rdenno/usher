#ifndef AppVersion
  #define AppVersion "0.3.0"
#endif
#ifndef VersionInfoVersion
  #define VersionInfoVersion AppVersion
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif
#ifndef RepoRoot
  #define RepoRoot ".."
#endif

[Setup]
AppId={{8E9B7D4C-4B27-4F3F-9F1E-2C6A5B8D1E42}
AppName=Usher
AppVersion={#AppVersion}
AppVerName=Usher {#AppVersion}
AppPublisher=Usher
AppPublisherURL=https://github.com/rdenno/usher
AppSupportURL=https://github.com/rdenno/usher/issues
AppUpdatesURL=https://github.com/rdenno/usher/releases
VersionInfoVersion={#VersionInfoVersion}
DefaultDirName={localappdata}\Programs\Usher
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UsePreviousAppDir=yes
UsePreviousSetupType=yes
UsePreviousTasks=yes
CloseApplications=force
CloseApplicationsFilter=*.exe
ChangesEnvironment=yes
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
OutputBaseFilename=usher-setup-windows-x86_64
UninstallDisplayName=Usher
UninstallDisplayIcon={app}\usher.exe

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Types]
Name: "client"; Description: "Client only (recommended for laptop)"
Name: "full"; Description: "Client + Host (stream from and to this PC)"
Name: "host"; Description: "Host only (stream this PC over SSH)"
Name: "custom"; Description: "Custom installation"; Flags: iscustom

[Components]
Name: "client"; Description: "Usher Client (usher.exe)"; Types: client full custom
Name: "host"; Description: "Usher Host (usher-host.exe)"; Types: host full

[Tasks]
Name: "addtopath"; Description: "Add Usher to your User PATH (run 'usher' from any terminal)"; GroupDescription: "Environment:"

[Files]
Source: "{#BinDir}\usher.exe"; DestDir: "{app}"; Components: client; Flags: ignoreversion
Source: "{#BinDir}\usher-host.exe"; DestDir: "{app}"; Components: host; Flags: ignoreversion
Source: "{#RepoRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Messages]
FinishedLabel=Setup has finished installing [name] on your computer.%n%nOpen a new PowerShell or Terminal window and run:%n    usher <your-ssh-host>
FinishedLabelNoIcons=Setup has finished installing [name] on your computer.%n%nOpen a new PowerShell or Terminal window and run:%n    usher <your-ssh-host>

[Code]
function NormalizePathEntry(const S: String): String;
begin
  Result := Trim(S);
  while (Length(Result) > 3) and (Result[Length(Result)] = '\') do
    Result := Copy(Result, 1, Length(Result) - 1);
end;

function FilterUserPath(const PathList, TargetDir: String; var Found: Boolean): String;
var
  Remaining, Entry, NormTarget, NormEntry: String;
  SepPos: Integer;
begin
  Result := '';
  Found := False;
  NormTarget := Uppercase(NormalizePathEntry(TargetDir));
  Remaining := PathList;

  while Length(Remaining) > 0 do
  begin
    SepPos := Pos(';', Remaining);
    if SepPos > 0 then
    begin
      Entry := Copy(Remaining, 1, SepPos - 1);
      Remaining := Copy(Remaining, SepPos + 1, Length(Remaining) - SepPos);
    end else
    begin
      Entry := Remaining;
      Remaining := '';
    end;

    Entry := Trim(Entry);
    if Entry <> '' then
    begin
      NormEntry := Uppercase(NormalizePathEntry(Entry));
      if NormEntry = NormTarget then
        Found := True
      else if Result = '' then
        Result := Entry
      else
        Result := Result + ';' + Entry;
    end;
  end;
end;

procedure AddDirToUserPath(const Dir: String);
var
  OrigPath, CleanedPath, NewPath, NormDir: String;
  Found: Boolean;
begin
  NormDir := NormalizePathEntry(Dir);
  if NormDir = '' then
    Exit;

  if not RegQueryStringValue(HKCU, 'Environment', 'Path', OrigPath) then
    OrigPath := '';

  CleanedPath := FilterUserPath(OrigPath, NormDir, Found);
  if Found then
    Exit;

  if CleanedPath = '' then
    NewPath := NormDir
  else
    NewPath := CleanedPath + ';' + NormDir;

  RegWriteExpandStringValue(HKCU, 'Environment', 'Path', NewPath);
end;

procedure RemoveDirFromUserPath(const Dir: String);
var
  OrigPath, NewPath, NormDir: String;
  Found: Boolean;
begin
  NormDir := NormalizePathEntry(Dir);
  if NormDir = '' then
    Exit;

  if not RegQueryStringValue(HKCU, 'Environment', 'Path', OrigPath) then
    Exit;

  NewPath := FilterUserPath(OrigPath, NormDir, Found);
  if not Found then
    Exit;

  if NewPath = '' then
    RegDeleteValue(HKCU, 'Environment', 'Path')
  else
    RegWriteExpandStringValue(HKCU, 'Environment', 'Path', NewPath);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddDirToUserPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveDirFromUserPath(ExpandConstant('{app}'));
end;

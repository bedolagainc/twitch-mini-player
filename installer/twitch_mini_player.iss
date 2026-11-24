; Установщик Mini Twitch Player (Inno Setup 6).
;
; Собирается после scripts\fetch-vendor.ps1 и cargo build --release:
;   ISCC.exe /DMyAppVersion=0.18.0 installer\twitch_mini_player.iss
;
; Per-user установка: без прав администратора, в %LOCALAPPDATA%\Programs.
; Модели распознавания и перевода ставятся в %APPDATA%\twitch_mini_player\models,
; поэтому обновление плеера их не затирает.

#define MyAppName "Mini Twitch Player"
#define MyAppExeName "twitch_mini_player.exe"
#define MyAppPublisher "bedolagainc"
#define MyAppUrl "https://github.com/bedolagainc/twitch-mini-player"

#ifndef MyAppVersion
  #define MyAppVersion "0.18.0"
#endif

[Setup]
AppId={{6F1B2A54-9C3D-4A7E-8B21-5D0E7C4A1F63}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppUrl}
AppSupportURL={#MyAppUrl}/issues
AppUpdatesURL={#MyAppUrl}/releases
DefaultDirName={localappdata}\Programs\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
LicenseFile=..\LICENSE
OutputDir=..\dist
OutputBaseFilename=twitch_mini_player-{#MyAppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
CloseApplications=yes
SetupLogging=yes

[Languages]
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"

[Tasks]
Name: "desktopicon"; Description: "Создать ярлык на рабочем столе"; GroupDescription: "Ярлыки:"; Flags: unchecked

[Files]
; Плеер
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\CHANGELOG.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\FIRST-RUN.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\docs\*"; DestDir: "{app}\docs"; Flags: ignoreversion recursesubdirs createallsubdirs

; Видео
Source: "..\vendor\libmpv\libmpv-2.dll"; DestDir: "{app}"; Flags: ignoreversion

; streamlink — в путь, который плеер ищет без настройки:
; %LOCALAPPDATA%\Programs\Streamlink\bin\streamlink.exe
; onlyifdoesntexist: чужую установку Streamlink не перезаписываем.
Source: "..\vendor\streamlink\*"; DestDir: "{localappdata}\Programs\Streamlink"; Flags: ignoreversion recursesubdirs createallsubdirs onlyifdoesntexist

; Распознавание речи: движок рядом с плеером, модели — в профиле пользователя
Source: "..\vendor\whisper\*.dll"; DestDir: "{app}\tools\whisper"; Flags: ignoreversion
Source: "..\vendor\models\*.bin"; DestDir: "{userappdata}\twitch_mini_player\models"; Flags: ignoreversion

; Перевод: ONNX Runtime рядом с плеером, модель — в профиле пользователя
Source: "..\vendor\onnx\onnxruntime.dll"; DestDir: "{app}\tools\onnx"; Flags: ignoreversion
Source: "..\vendor\mt\*"; DestDir: "{userappdata}\twitch_mini_player\models\mt"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\Удалить {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Запустить {#MyAppName}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Пустые каталоги после удаления; пользовательские файлы
; (settings.json, favorites.txt, capture.wav) не трогаем.
Type: dirifempty; Name: "{userappdata}\twitch_mini_player\models\mt"
Type: dirifempty; Name: "{userappdata}\twitch_mini_player\models"
Type: dirifempty; Name: "{localappdata}\Programs\Streamlink\bin"
Type: dirifempty; Name: "{localappdata}\Programs\Streamlink"
Type: dirifempty; Name: "{app}\tools\whisper"
Type: dirifempty; Name: "{app}\tools\onnx"

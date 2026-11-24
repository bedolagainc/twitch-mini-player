<#
.SYNOPSIS
    Скачивает движок распознавания речи (whisper.cpp, DLL) и модель Whisper.

.DESCRIPTION
    Бинарники и модели НЕ хранятся в репозитории (см. .gitignore): скрипт
    скачивает их из официальных источников и проверяет SHA-256, чтобы битый
    файл после обрыва связи не приводил к молчаливому падению плеера.

    Движок кладётся в tools\whisper (рядом с .exe при сборке релиза),
    модель — в %APPDATA%\twitch_mini_player\models (не теряется при
    обновлении программы и не требует прав администратора).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\fetch-whisper.ps1
    powershell -ExecutionPolicy Bypass -File scripts\fetch-whisper.ps1 -Model small
    powershell -ExecutionPolicy Bypass -File scripts\fetch-whisper.ps1 -Model none
#>
[CmdletBinding()]
param(
    # none — только движок, tiny/base/small — плюс модель (~32 / 60 / 190 МБ)
    [ValidateSet('none', 'tiny', 'base', 'small')]
    [string]$Model = 'base',
    [string]$ToolsDir,
    [string]$ModelsDir,
    [switch]$Force
)

# $PSScriptRoot недоступен в блоке param(), поэтому пути считаем здесь.
if (-not $ToolsDir)  { $ToolsDir  = Join-Path $PSScriptRoot '..\tools\whisper' }
if (-not $ModelsDir) { $ModelsDir = Join-Path $env:APPDATA 'twitch_mini_player\models' }

$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'  # без него Invoke-WebRequest медленный

# PS 5.1: Invoke-WebRequest качает крупные файлы в разы медленнее WebClient.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Get-Remote {
    param([string]$Url, [string]$Path)
    # curl.exe есть в Windows 10+ и на раннерах CI: качает быстро и стабильно.
    $curl = (Get-Command curl.exe -ErrorAction SilentlyContinue).Source
    if ($curl) {
        & $curl -L --retry 3 --retry-delay 2 --max-time 1800 -o $Path $Url
        if ($LASTEXITCODE -ne 0) { throw "не удалось скачать $Url (curl: $LASTEXITCODE)" }
        return
    }
    $wc = New-Object System.Net.WebClient
    $wc.Headers.Add('User-Agent', 'twitch-mini-player')
    try { $wc.DownloadFile($Url, $Path) } finally { $wc.Dispose() }
}

# --- Движок: официальная сборка whisper.cpp (CPU x64, AVX2) ------------------
# В релизах-тегах (v1.9.x) ассетов нет, бинарники публикуются в сборках b*.
$EngineUrl = 'https://github.com/ggml-org/whisper.cpp/releases/download/b5907/whisper-bin-x64.zip'
$EngineSha = 'ECCA07B277D943E9B9F4712856A144B857271A6B333104E511E1988E80DB582B'
# Берём только то, что нужно плееру: ядро входа в API и ggml (whisper.dll 1.3 МБ).
# ggml-cpu-*.dll (8 шт., ~8 МБ) подхватываются по маске: ggml сам выбирает
# вариант под микроархитектуру процессора. SDL2/llama/тесты из архива не нужны.
$EngineFiles = @('whisper.dll', 'ggml.dll', 'ggml-base.dll')
$EngineGlobs = @('ggml-cpu-*.dll')

# --- Модели: только английские (.en) — точнее и быстрее многоязычных ----------
$ModelUrl = 'https://huggingface.co/ggerganov/whisper.cpp/resolve/main/'
$ModelMirror = 'https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/'
$Models = @{
    tiny  = @{ File = 'ggml-tiny.en-q5_1.bin';  Bytes = 32166155;
               Sha = 'C77C5766F1CEF09B6B7D47F21B546CBDDD4157886B3B5D6D4F709E91E66C7C2B' }
    base  = @{ File = 'ggml-base.en-q5_1.bin';  Bytes = 59721011;
               Sha = '4BAF70DD0D7C4247BA2B81FAFD9C01005AC77C2F9EF064E00DCF195D0E2FDD2F' }
    small = @{ File = 'ggml-small.en-q5_1.bin'; Bytes = 190098681;
               Sha = 'BFDFF4894DCB76BBF647D56263EA2A96645423F1669176F4844A1BF8E478AD30' }
}

function Save-Checked {
    param([string]$Url, [string]$Mirror, [string]$Path, [string]$Sha, [long]$Bytes)

    $expect = $Sha.ToUpperInvariant()
    if ((Test-Path -LiteralPath $Path) -and -not $Force) {
        $have = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
        if ($have -eq $expect) {
            Write-Host ("  ok   {0} (уже есть)" -f (Split-Path $Path -Leaf))
            return
        }
        Write-Host ("  !    {0}: хеш не совпал, перекачиваем" -f (Split-Path $Path -Leaf))
    }

    $dir = Split-Path -Parent $Path
    if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    $part = "$Path.part"
    foreach ($src in @($Url, $Mirror)) {
        if ([string]::IsNullOrWhiteSpace($src)) { continue }
        try {
            Write-Host ("  ...  {0}" -f (Split-Path $Path -Leaf))
            Get-Remote -Url $src -Path $part
            break
        } catch {
            Write-Warning ("источник недоступен: {0}" -f $src)
        }
    }
    if (-not (Test-Path -LiteralPath $part)) { throw "не удалось скачать $Path" }

    $have = (Get-FileHash -LiteralPath $part -Algorithm SHA256).Hash
    if ($have -ne $expect) {
        Remove-Item -LiteralPath $part -Force
        throw ("SHA-256 не совпал: {0}`n  ожидали {1}`n  получили {2}" -f $Path, $expect, $have)
    }
    if ($Bytes -gt 0 -and (Get-Item -LiteralPath $part).Length -ne $Bytes) {
        Remove-Item -LiteralPath $part -Force
        throw "размер файла не совпал: $Path"
    }
    Move-Item -LiteralPath $part -Destination $Path -Force
    Write-Host ("  ok   {0} ({1:N1} МБ)" -f (Split-Path $Path -Leaf), ((Get-Item $Path).Length / 1MB))
}

Write-Host 'Движок распознавания речи (whisper.cpp):'
$dll = Join-Path $ToolsDir 'whisper.dll'
$needEngine = $Force -or -not (Test-Path -LiteralPath $dll)
if ($needEngine) {
    $zip = Join-Path $env:TEMP 'whisper-bin-x64.zip'
    Save-Checked -Url $EngineUrl -Mirror '' -Path $zip -Sha $EngineSha -Bytes 0
    $unpack = Join-Path $env:TEMP 'whisper-bin-x64'
    if (Test-Path -LiteralPath $unpack) { Remove-Item -Recurse -Force -LiteralPath $unpack }
    Expand-Archive -LiteralPath $zip -DestinationPath $unpack -Force
    if (-not (Test-Path -LiteralPath $ToolsDir)) { New-Item -ItemType Directory -Force -Path $ToolsDir | Out-Null }
    $take = @(Get-ChildItem -Path $unpack -Recurse -File | Where-Object { $EngineFiles -contains $_.Name })
    foreach ($g in $EngineGlobs) { $take += Get-ChildItem -Path $unpack -Recurse -File -Filter $g }
    foreach ($f in $take) { Copy-Item -LiteralPath $f.FullName -Destination $ToolsDir -Force }
    Write-Host ("  ok   {0} файлов в {1}" -f $take.Count, (Resolve-Path $ToolsDir))
} else {
    Write-Host ("  ok   whisper.dll (уже есть)")
}

if ($Model -ne 'none') {
    $m = $Models[$Model]
    Write-Host ("Модель {0} (~{1:N0} МБ):" -f $m.File, ($m.Bytes / 1MB))
    $path = Join-Path $ModelsDir $m.File
    Save-Checked -Url ($ModelUrl + $m.File) -Mirror ($ModelMirror + $m.File) `
                 -Path $path -Sha $m.Sha -Bytes $m.Bytes
    Write-Host ''
    Write-Host ("Готово. Модель: {0}" -f $path)
}

Write-Host ''
Write-Host ("Движок: {0}" -f (Resolve-Path $ToolsDir))

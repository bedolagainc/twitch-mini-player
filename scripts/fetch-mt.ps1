<#
.SYNOPSIS
    Скачивает модель перевода en→ru (opus-mt-en-ru, int8) и ONNX Runtime.

.DESCRIPTION
    Бинарники и модели НЕ хранятся в репозитории (см. .gitignore): скрипт
    берёт их из официальных источников и проверяет SHA-256, чтобы битый файл
    после обрыва связи не приводил к молчаливому падению перевода.

    Модель кладётся в %APPDATA%\twitch_mini_player\models\mt (не теряется при
    обновлении программы), onnxruntime.dll — в tools\onnx (рядом с .exe).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\fetch-mt.ps1
    powershell -ExecutionPolicy Bypass -File scripts\fetch-mt.ps1 -Force
#>
[CmdletBinding()]
param(
    # Версия пакета Microsoft.ML.OnnxRuntime (совпадает с api-31 в ort).
    [string]$OnnxVersion = '1.31.1',
    [string]$ToolsDir,
    [string]$ModelsDir,
    [switch]$Force
)

if (-not $ToolsDir)  { $ToolsDir  = Join-Path $PSScriptRoot '..\tools\onnx' }
if (-not $ModelsDir) { $ModelsDir = Join-Path $env:APPDATA 'twitch_mini_player\models\mt' }

$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'

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

$HfBase = 'https://huggingface.co/Xenova/opus-mt-en-ru/resolve/main/'
$Files = @(
    @{ Path = 'tokenizer.json';                    Name = 'tokenizer.json';
       Bytes = 7205388;
       Sha = '981CD3D9FEF6BB4DDA8082AFDA716B65DEB6717CB3EF35AC0B57002C09DEC2BB' },
    @{ Path = 'generation_config.json';            Name = 'generation_config.json';
       Bytes = 293;
       Sha = '885345001AC4F978B5E295A04EB6BD6C346EBC7381E30D5B3DD927B178D8A5EF' },
    @{ Path = 'onnx/encoder_model_quantized.onnx'; Name = 'encoder_model_quantized.onnx';
       Bytes = 51628446;
       Sha = '97F28D8295D231B7DA721DED06FA8C034787DF2122CD8F85B904ABDC4D15BD53' },
    @{ Path = 'onnx/decoder_model_quantized.onnx'; Name = 'decoder_model_quantized.onnx';
       Bytes = 58560874;
       Sha = '7E4E602C31130176B086CD4BF31D6D08D44A6A367617E910870E0EA3E5408574' }
)

function Save-Checked {
    param([string]$Url, [string]$Path, [string]$Sha, [long]$Bytes)

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
    Write-Host ("  ...  {0}" -f (Split-Path $Path -Leaf))
    Get-Remote -Url $Url -Path $part

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

Write-Host 'Модель перевода en→ru (opus-mt-en-ru, int8):'
foreach ($f in $Files) {
    Save-Checked -Url ($HfBase + $f.Path) -Path (Join-Path $ModelsDir $f.Name) `
                 -Sha $f.Sha -Bytes $f.Bytes
}

Write-Host 'ONNX Runtime:'
$dll = Join-Path $ToolsDir 'onnxruntime.dll'
if ((Test-Path -LiteralPath $dll) -and -not $Force) {
    Write-Host '  ok   onnxruntime.dll (уже есть)'
} else {
    $nupkg = Join-Path $env:TEMP "onnxruntime-$OnnxVersion.nupkg"
    $zip   = Join-Path $env:TEMP "onnxruntime-$OnnxVersion.zip"
    $url   = "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/$OnnxVersion/microsoft.ml.onnxruntime.$OnnxVersion.nupkg"
    Write-Host ("  ...  onnxruntime {0}" -f $OnnxVersion)
    Get-Remote -Url $url -Path $nupkg
    Copy-Item -LiteralPath $nupkg -Destination $zip -Force
    $unpack = Join-Path $env:TEMP "onnxruntime-$OnnxVersion"
    if (Test-Path -LiteralPath $unpack) { Remove-Item -Recurse -Force -LiteralPath $unpack }
    Expand-Archive -LiteralPath $zip -DestinationPath $unpack -Force
    $found = Get-ChildItem -Path $unpack -Recurse -Filter 'onnxruntime.dll' |
             Where-Object { $_.FullName -like '*win-x64*' } | Select-Object -First 1
    if (-not $found) { throw 'в пакете нет runtimes\win-x64\native\onnxruntime.dll' }
    if (-not (Test-Path -LiteralPath $ToolsDir)) { New-Item -ItemType Directory -Force -Path $ToolsDir | Out-Null }
    Copy-Item -LiteralPath $found.FullName -Destination $dll -Force
    Write-Host ("  ok   onnxruntime.dll ({0:N1} МБ)" -f ((Get-Item $dll).Length / 1MB))
}

Write-Host ''
Write-Host ("Готово. Модель: {0}" -f (Resolve-Path $ModelsDir))
Write-Host ("Рантайм: {0}" -f (Resolve-Path $ToolsDir))

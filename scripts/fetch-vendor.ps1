<#
.SYNOPSIS
    Скачивает всё, что нужно установщику: libmpv, streamlink, whisper.cpp, модели.

.DESCRIPTION
    В репозитории бинарников нет (см. .gitignore) — скрипт собирает их в vendor\
    из официальных источников и проверяет SHA-256, где он известен. Структура та,
    на которую рассчитан installer\twitch_mini_player.iss:

        vendor\libmpv\libmpv-2.dll
        vendor\streamlink\bin\streamlink.exe + зависимости
        vendor\whisper\*.dll
        vendor\models\ggml-base.en-q5_1.bin
        vendor\mt\tokenizer.json + onnx-модели
        vendor\onnx\onnxruntime.dll

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\fetch-vendor.ps1
    powershell -ExecutionPolicy Bypass -File scripts\fetch-vendor.ps1 -Force
#>
[CmdletBinding()]
param(
    [string]$Dir,
    # none — без движка распознавания (только плеер)
    [ValidateSet('none', 'tiny', 'base', 'small')]
    [string]$WhisperModel = 'base',
    # Сборки mpv: https://github.com/shinchiro/mpv-winbuild-cmake/releases
    [string]$MpvTag = '20261111',
    [string]$StreamlinkTag = '8.6.3-1',
    [switch]$Force
)

if (-not $Dir) { $Dir = Join-Path $PSScriptRoot '..\vendor' }
$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'
$Ua = @{ 'User-Agent' = 'twitch-mini-player' }

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

function Get-7z {
    foreach ($p in @((Get-Command 7z -ErrorAction SilentlyContinue).Source,
                     'C:\Program Files\7-Zip\7z.exe',
                     'C:\Program Files (x86)\7-Zip\7z.exe')) {
        if ($p -and (Test-Path -LiteralPath $p)) { return $p }
    }
    throw 'нужен 7-Zip (7z.exe): https://www.7-zip.org/ — или распакуйте mpv-dev вручную'
}

function Save-Checked {
    param([string]$Url, [string]$Path, [string]$Sha, [long]$Bytes)

    if ($Sha) {
        $expect = $Sha.ToUpperInvariant()
        if ((Test-Path -LiteralPath $Path) -and -not $Force) {
            $have = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
            if ($have -eq $expect) {
                Write-Host ("  ok   {0} (уже есть)" -f (Split-Path $Path -Leaf))
                return
            }
        }
    } elseif ((Test-Path -LiteralPath $Path) -and -not $Force) {
        Write-Host ("  ok   {0} (уже есть)" -f (Split-Path $Path -Leaf))
        return
    }

    $parent = Split-Path -Parent $Path
    if (-not (Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    $part = "$Path.part"
    Write-Host ("  ...  {0}" -f (Split-Path $Path -Leaf))
    Get-Remote -Url $Url -Path $part

    if ($Sha) {
        $have = (Get-FileHash -LiteralPath $part -Algorithm SHA256).Hash
        if ($have -ne $Sha.ToUpperInvariant()) {
            Remove-Item -LiteralPath $part -Force
            throw ("SHA-256 не совпал: {0}`n  ожидали {1}`n  получили {2}" -f $Path, $Sha, $have)
        }
    }
    if ($Bytes -gt 0 -and (Get-Item -LiteralPath $part).Length -ne $Bytes) {
        Remove-Item -LiteralPath $part -Force
        throw "размер файла не совпал: $Path"
    }
    Move-Item -LiteralPath $part -Destination $Path -Force
    Write-Host ("  ok   {0} ({1:N1} МБ)" -f (Split-Path $Path -Leaf), ((Get-Item $Path).Length / 1MB))
}

# --- libmpv -------------------------------------------------------------------
$mpvDst = Join-Path $Dir 'libmpv\libmpv-2.dll'
if (-not (Test-Path -LiteralPath $mpvDst) -or $Force) {
    Write-Host 'libmpv (mpv-dev):'
    $rel = Invoke-RestMethod -UseBasicParsing -Headers $Ua `
        -Uri "https://api.github.com/repos/shinchiro/mpv-winbuild-cmake/releases/tags/$MpvTag"
    $asset = $rel.assets | Where-Object { $_.name -like 'mpv-dev-x86_64-*.7z' } | Select-Object -First 1
    if (-not $asset) { throw "в релизе mpv $MpvTag нет mpv-dev-x86_64-*.7z" }
    $arch = Join-Path $env:TEMP "mpv-dev-$MpvTag.7z"
    $sha = if ($asset.digest) { $asset.digest -replace '^sha256:', '' } else { '' }
    Save-Checked -Url $asset.browser_download_url -Path $arch -Sha $sha -Bytes $asset.size

    $unpack = Join-Path $env:TEMP "mpv-dev-$MpvTag"
    if (Test-Path -LiteralPath $unpack) { Remove-Item -Recurse -Force -LiteralPath $unpack }
    & (Get-7z) x -y "-o$unpack" $arch 'libmpv-2.dll' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'не удалось распаковать mpv-dev (7z)' }
    $dll = Get-ChildItem -Path $unpack -Recurse -Filter 'libmpv-2.dll' | Select-Object -First 1
    if (-not $dll) { throw 'в архиве mpv-dev нет libmpv-2.dll' }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $mpvDst) | Out-Null
    Copy-Item -LiteralPath $dll.FullName -Destination $mpvDst -Force
    Write-Host ("  ok   libmpv-2.dll ({0:N1} МБ)" -f ((Get-Item $mpvDst).Length / 1MB))
} else {
    Write-Host 'libmpv: ok (уже есть)'
}

# --- streamlink (portable сборка streamlink/windows-builds) --------------------
$slExe = Join-Path $Dir 'streamlink\bin\streamlink.exe'
if (-not (Test-Path -LiteralPath $slExe) -or $Force) {
    Write-Host 'streamlink:'
    try {
        $rel = Invoke-RestMethod -UseBasicParsing -Headers $Ua `
            -Uri "https://api.github.com/repos/streamlink/windows-builds/releases/tags/$StreamlinkTag"
        $asset = $rel.assets | Where-Object { $_.name -like '*-x86_64.zip' } | Select-Object -First 1
        if (-not $asset) { throw "в релизе streamlink $StreamlinkTag нет x86_64.zip" }
        $zip = Join-Path $env:TEMP $asset.name
        $sha = if ($asset.digest) { $asset.digest -replace '^sha256:', '' } else { '' }
        Save-Checked -Url $asset.browser_download_url -Path $zip -Sha $sha -Bytes $asset.size

        $unpack = Join-Path $env:TEMP 'streamlink-portable'
        if (Test-Path -LiteralPath $unpack) { Remove-Item -Recurse -Force -LiteralPath $unpack }
        Expand-Archive -LiteralPath $zip -DestinationPath $unpack -Force
        $found = Get-ChildItem -Path $unpack -Recurse -Filter 'streamlink.exe' | Select-Object -First 1
        if (-not $found) { throw 'в архиве streamlink нет streamlink.exe' }
        # Единая раскладка: streamlink\bin\streamlink.exe + зависимости рядом.
        $dst = Join-Path $Dir 'streamlink\bin'
        if (Test-Path -LiteralPath (Join-Path $Dir 'streamlink')) {
            Remove-Item -Recurse -Force -LiteralPath (Join-Path $Dir 'streamlink')
        }
        New-Item -ItemType Directory -Force -Path $dst | Out-Null
        Copy-Item -Path (Join-Path $found.Directory.FullName '*') -Destination $dst -Recurse -Force
    } catch {
        # GitHub недоступен — берём streamlink, установленный в системе
        # (в нём уже раскладка bin\streamlink.exe).
        $sys = (Get-Command streamlink.exe -ErrorAction SilentlyContinue).Source
        if (-not $sys) { throw }
        Write-Warning ("не удалось скачать streamlink ({0}); берём локальную установку: {1}" -f `
            $_.Exception.Message, $sys)
        $root = Split-Path -Parent (Split-Path -Parent $sys)
        $dst = Join-Path $Dir 'streamlink'
        if (Test-Path -LiteralPath $dst) { Remove-Item -Recurse -Force -LiteralPath $dst }
        New-Item -ItemType Directory -Force -Path $dst | Out-Null
        Copy-Item -Path (Join-Path $root '*') -Destination $dst -Recurse -Force
    }
    $size = (Get-ChildItem (Join-Path $Dir 'streamlink') -Recurse -File |
             Measure-Object Length -Sum).Sum
    Write-Host ("  ok   streamlink ({0:N0} МБ)" -f ($size / 1MB))
} else {
    Write-Host 'streamlink: ok (уже есть)'
}

# --- распознавание речи и перевод (переиспользуем точечные скрипты) -----------
if ($WhisperModel -ne 'none') {
    Write-Host 'Распознавание речи:'
    & (Join-Path $PSScriptRoot 'fetch-whisper.ps1') -Model $WhisperModel `
        -ToolsDir (Join-Path $Dir 'whisper') -ModelsDir (Join-Path $Dir 'models') -Force:$Force
}

Write-Host 'Перевод en→ru:'
& (Join-Path $PSScriptRoot 'fetch-mt.ps1') `
    -ToolsDir (Join-Path $Dir 'onnx') -ModelsDir (Join-Path $Dir 'mt') -Force:$Force

# --- итог ---------------------------------------------------------------------
Write-Host ''
$total = (Get-ChildItem -LiteralPath $Dir -Recurse -File | Measure-Object Length -Sum).Sum
Write-Host ("Готово: {0} — {1:N0} МБ" -f (Resolve-Path $Dir), ($total / 1MB))

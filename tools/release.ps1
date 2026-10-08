# Build and package the Windows release: dist\FH1Rewrite-<version>-win64.zip
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1 [-Version 0.1.0] [-NoBuild]
#
# Layout (crates/fh1-launcher/src/main.rs):
#   FH1 Rewrite.exe            launcher: install wizard (FH1 required, FH2/FM4 optional) + Play
#   bin\fh1-engine.exe         the game
#   bin\fh1setup.exe           asset converter, built with --features fh2,fm4
#   bin\extract-xiso.exe       ISO extraction (pinned XboxDev build, SHA-256 checked)
#   bin\ffmpeg.exe             XMA decoding during setup (pinned gyan.dev essentials build, SHA-256 checked)
#   bin\*.dll                  VC++ runtime (app-local, Microsoft redistributable)
#   licenses\, README.txt
#
# The XEX key: dist\xex_key.txt (git-ignored; 32 hex digits) is embedded into fh1setup.exe at compile time through
# FH1_XEX_KEY_EMBED (fh1-shaders xex.rs retail_key). It is never written to the repository.
param([string]$Version = "", [switch]$NoBuild)
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root
if (-not $Version) {
    $Version = (Select-String -Path crates\fh1-launcher\Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
}
$Dist = Join-Path $Root "dist"
$Cache = Join-Path $Dist "cache"
$Out = Join-Path $Dist "FH1Rewrite-$Version"
New-Item -ItemType Directory -Force $Cache | Out-Null

function Get-Pinned($Url, $Sha256, $Name) {
    $p = Join-Path $Cache $Name
    if (-not (Test-Path $p) -or (Get-FileHash $p -Algorithm SHA256).Hash -ne $Sha256) {
        Write-Host "downloading $Url"
        Invoke-WebRequest -Uri $Url -OutFile $p -UseBasicParsing
        $got = (Get-FileHash $p -Algorithm SHA256).Hash
        if ($got -ne $Sha256) { throw "$Name hash mismatch: $got" }
    }
    return $p
}

if (-not $NoBuild) {
    $keyFile = Join-Path $Dist "xex_key.txt"
    if (-not (Test-Path $keyFile)) { throw "dist\xex_key.txt missing (32 hex digits; never committed)" }
    $env:FH1_XEX_KEY_EMBED = (Get-Content $keyFile -Raw).Trim()
    $cargoArgs = @("build", "--release", "-p", "fh1setup", "--features", "fh2,fm4", "-p", "fh1-launcher", "-p", "fh1-engine")
    # The maintainers' machine serialises builds through a local lock script; elsewhere plain cargo.
    if (Test-Path tools\cargo-locked.ps1) {
        & powershell -NoProfile -ExecutionPolicy Bypass -File tools\cargo-locked.ps1 @cargoArgs
    } else {
        & cargo @cargoArgs
    }
    $code = $LASTEXITCODE
    Remove-Item Env:FH1_XEX_KEY_EMBED
    if ($code -ne 0) { throw "build failed ($code)" }
}

if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
$Bin = Join-Path $Out "bin"
$Lic = Join-Path $Out "licenses"
New-Item -ItemType Directory -Force $Bin, $Lic | Out-Null
Copy-Item target\release\fh1-launcher.exe (Join-Path $Out "FH1 Rewrite.exe")
Copy-Item target\release\fh1-engine.exe, target\release\fh1setup.exe $Bin

# extract-xiso (same pinned build as crates/fh1setup/src/extract.rs).
$xz = Get-Pinned "https://github.com/XboxDev/extract-xiso/releases/download/build-202505152050/extract-xiso-Win64_Release.zip" `
    "FEC88D03C7EFD6205AB09BE4ABBA70C0AFD0EB27A5709F0A6235B828BA5AC11E" "extract-xiso.zip"
$xd = Join-Path $Cache "extract-xiso"
if (-not (Test-Path $xd)) { Expand-Archive $xz $xd }
Copy-Item (Get-ChildItem $xd -Recurse -Filter extract-xiso.exe | Select-Object -First 1).FullName $Bin
Set-Content (Join-Path $Lic "extract-xiso.txt") "extract-xiso (https://github.com/XboxDev/extract-xiso), build-202505152050. See the project's LICENSE.TXT; copyright its authors."

# ffmpeg (XMA2 decoder for the engine sounds), pinned essentials build.
$fz = Get-Pinned "https://github.com/GyanD/codexffmpeg/releases/download/7.1.1/ffmpeg-7.1.1-essentials_build.zip" `
    "04861D3339C5EBE38B56C19A15CF2C0CC97F5DE4FA8910E4D47E5E6404E4A2D4" "ffmpeg-7.1.1-essentials_build.zip"
$fd = Join-Path $Cache "ffmpeg"
if (-not (Test-Path $fd)) { Expand-Archive $fz $fd }
Copy-Item (Get-ChildItem $fd -Recurse -Filter ffmpeg.exe | Select-Object -First 1).FullName $Bin
Copy-Item (Get-ChildItem $fd -Recurse -Filter LICENSE | Select-Object -First 1).FullName (Join-Path $Lic "ffmpeg-LICENSE.txt")
Set-Content (Join-Path $Lic "ffmpeg.txt") "ffmpeg 7.1.1 essentials build by gyan.dev (GPLv3). Source: https://ffmpeg.org/releases/ffmpeg-7.1.1.tar.xz and https://www.gyan.dev/ffmpeg/builds/"

# VC++ runtime, app-local.
$crt = Get-ChildItem "${env:ProgramFiles(x86)}\Microsoft Visual Studio\*\*\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT", "$env:ProgramFiles\Microsoft Visual Studio\*\*\VC\Redist\MSVC\*\x64\Microsoft.VC*.CRT" -ErrorAction SilentlyContinue | Select-Object -Last 1
if (-not $crt) { throw "VC++ redist folder not found (Visual Studio Build Tools)" }
Copy-Item (Join-Path $crt.FullName "msvcp140.dll"), (Join-Path $crt.FullName "vcruntime140.dll"), (Join-Path $crt.FullName "vcruntime140_1.dll") $Bin

Copy-Item LICENSE (Join-Path $Lic "FH1-Rewrite-GPL-3.0.txt")
@"
FH1 Rewrite $Version (Windows x64)
==================================

An unofficial, clean reimplementation of Forza Horizon (Xbox 360, 2012). Not affiliated with Microsoft, Turn 10 or
Playground Games. No game files are included: you need your own Forza Horizon disc image.

1. Unzip this folder anywhere with plenty of free space (about 30 GB for Forza Horizon alone).
2. Open "FH1 Rewrite.exe".
3. Choose your Forza Horizon disc: the .iso, a .zip holding it, or the folder it is in.
   Optional: tick Forza Horizon 2 and/or Forza Motorsport 4 if you own them and choose their discs too.
   Games you don't add stay locked in the menus. Forza Motorsport 3 is coming later.
4. Click Install and wait (up to an hour on slower PCs). Then click PLAY.

Controls: W/S or RT/LT throttle/brake, A/D or left stick steer, Space/A handbrake, C/RB camera, Esc/Start pause.
Logs: data\logs. Source code: https://github.com/Sm1jjj/fh1_rewrite
"@ | Set-Content (Join-Path $Out "README.txt")

$zip = Join-Path $Dist "FH1Rewrite-$Version-win64.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $Out -DestinationPath $zip -CompressionLevel Optimal
Write-Host "release: $zip ($([math]::Round((Get-Item $zip).Length / 1MB)) MB)"

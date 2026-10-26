param(
    [string]$Version = "3.1.0",
    [string]$Arch = "win-x64",
    [string]$OutDir = "",
    [string]$SourceZip = "",
    [string]$ExpectedSha256 = "81D3A024207C272524E4D992BFAF352A86256B00AFAC414F0405CE465642CFB7"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) {
    $OutDir = Join-Path $repoRoot "src-tauri/resources/amule/$Arch"
}
if (Test-Path (Join-Path $OutDir "amuled.exe")) {
    Write-Host "aMule already packaged at $OutDir"
    exit 0
}

$tmp = Join-Path $env:TEMP ("fdm-amule-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmp | Out-Null

if (-not $SourceZip) {
    $asset = if ($Arch -eq "win-x64") { "aMule-$Version-Windows-x64.zip" } else { "aMule-$Version-Windows-arm64.zip" }
    $url = "https://github.com/amule-org/amule/releases/download/$Version/$asset"
    $SourceZip = Join-Path $tmp $asset
    Write-Host "downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $SourceZip -Headers @{ "User-Agent" = "FDM-packager" }
}

$hash = (Get-FileHash -LiteralPath $SourceZip -Algorithm SHA256).Hash
if ($ExpectedSha256 -and $hash -ne $ExpectedSha256) {
    throw "aMule zip SHA256 mismatch: $hash != $ExpectedSha256"
}

Expand-Archive -LiteralPath $SourceZip -DestinationPath $tmp
$portable = Get-ChildItem $tmp -Directory | Where-Object { Test-Path (Join-Path $_.FullName "bin") } | Select-Object -First 1
if (-not $portable) { throw "portable aMule folder not found" }
$bin = Join-Path $portable.FullName "bin"

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
python (Join-Path $PSScriptRoot "amule_dll_closure.py") $bin $OutDir "amuled.exe" "amuleapi.exe"
if ($LASTEXITCODE -ne 0) { throw "DLL closure packaging failed" }

$shareSrc = Join-Path $portable.FullName "share/amule"
$shareDst = Join-Path $OutDir "share/amule"
if (Test-Path $shareSrc) {
    New-Item -ItemType Directory -Path $shareDst -Force | Out-Null
    Copy-Item -Path (Join-Path $shareSrc "amuleapi-static") -Destination $shareDst -Recurse -Force
}
$license = Join-Path $portable.FullName "share/doc/amule/LICENSE.md"
if (Test-Path $license) {
    Copy-Item -LiteralPath $license -Destination (Join-Path $OutDir "LICENSE.md") -Force
}
$total = (Get-ChildItem $OutDir -Recurse -File | Measure-Object Length -Sum).Sum
Write-Host ("packaged aMule into {0} ({1:N1} MiB)" -f $OutDir, ($total / 1MB))
Write-Host "temporary extraction left at $tmp"

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
. (Join-Path $PSScriptRoot "build-env.ps1")
if (-not (Test-Path (Join-Path $root "src-tauri/resources/amule/win-x64/amuled.exe"))) {
    & (Join-Path $PSScriptRoot "package-amule.ps1")
}
npx tauri build

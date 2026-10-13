# Harness v1.1.0 — Windows PowerShell One-Line Installer
# Installs `harness.exe` into %LOCALAPPDATA%\Programs\Harness\bin and adds it to your User PATH
# so you can open any PowerShell / Windows Terminal window and type `harness` directly.
$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
Set-Location $ScriptDir

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\Harness\bin"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$Prebuilt = Join-Path $ScriptDir "target\x86_64-pc-windows-gnu\release\harness.exe"
$NativeRelease = Join-Path $ScriptDir "target\release\harness.exe"

if (Test-Path $NativeRelease) {
    Copy-Item -Force $NativeRelease (Join-Path $InstallDir "harness.exe")
} elseif (Test-Path $Prebuilt) {
    Copy-Item -Force $Prebuilt (Join-Path $InstallDir "harness.exe")
} else {
    Write-Host "==> Building Harness v1.1.0 (release profile)..."
    cargo build --workspace --release
    Copy-Item -Force $NativeRelease (Join-Path $InstallDir "harness.exe")
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not $UserPath) { $UserPath = "" }
$PathParts = $UserPath -split ";" | Where-Object { $_ -ne "" }
if ($PathParts -notcontains $InstallDir) {
    $NewPath = ($PathParts + $InstallDir) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    $env:Path = "$InstallDir;$env:Path"
    Write-Host "==> Added $InstallDir to your User PATH."
}

Write-Host "==> Installed harness to $InstallDir\harness.exe"
Write-Host "==> Done! Open any folder in PowerShell / Windows Terminal and just type: harness"

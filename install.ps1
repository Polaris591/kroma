# Kroma v1.2.0 — Windows PowerShell One-Line Installer
# Installs `kroma.exe` into %USERPROFILE%\.kroma\bin and adds it to your User PATH
# so you can open any PowerShell / Windows Terminal window and type `kroma` directly.
$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
Set-Location $ScriptDir

$InstallDir = Join-Path $env:USERPROFILE ".kroma\bin"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$Prebuilt = Join-Path $ScriptDir "target\x86_64-pc-windows-gnu\release\kroma.exe"
$NativeRelease = Join-Path $ScriptDir "target\release\kroma.exe"

if (Test-Path $NativeRelease) {
    Copy-Item -Force $NativeRelease (Join-Path $InstallDir "kroma.exe")
} elseif (Test-Path $Prebuilt) {
    Copy-Item -Force $Prebuilt (Join-Path $InstallDir "kroma.exe")
} else {
    Write-Host "==> Building Kroma v1.2.0 (release profile)..."
    cargo build --workspace --release
    Copy-Item -Force $NativeRelease (Join-Path $InstallDir "kroma.exe")
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

Write-Host "==> Installed kroma to $InstallDir\kroma.exe"
Write-Host "==> Done! Open any folder in PowerShell / Windows Terminal and just type: kroma"

# Create a standalone, user-data-free Windows x64 release directory and ZIP.
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File desktop/scripts/package-portable.ps1
[CmdletBinding()]
param(
  [string]$BinaryPath = "",
  [string]$OutputDirectory = ""
)
$ErrorActionPreference = 'Stop'
$desktopRoot = Split-Path -Parent $PSScriptRoot
$projectConfig = Get-Content -LiteralPath (Join-Path $desktopRoot 'src-tauri\tauri.conf.json') -Raw | ConvertFrom-Json
$version = $projectConfig.version
if (-not $BinaryPath) {
  $gnuBinary = Join-Path $desktopRoot 'src-tauri\target\x86_64-pc-windows-gnu\release\kroma-desktop.exe'
  $msvcBinary = Join-Path $desktopRoot 'src-tauri\target\release\kroma-desktop.exe'
  if (Test-Path -LiteralPath $gnuBinary) {
    $BinaryPath = $gnuBinary
  } else {
    $BinaryPath = $msvcBinary
  }
}
if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
  throw "Kroma Desktop release executable missing: $BinaryPath"
}
$BinaryPath = (Resolve-Path -LiteralPath $BinaryPath).Path
$loaderPath = Join-Path (Split-Path -Parent $BinaryPath) 'WebView2Loader.dll'
if (-not (Test-Path -LiteralPath $loaderPath -PathType Leaf)) {
  throw "WebView2Loader.dll missing beside $BinaryPath. Cannot make a self-contained portable package."
}
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $desktopRoot 'release' }
$null = New-Item -ItemType Directory -Force -Path $OutputDirectory
$releaseName = "Kroma-Desktop-$version-Windows-x64"
$releaseFolder = Join-Path $OutputDirectory $releaseName
if (Test-Path -LiteralPath $releaseFolder) { Remove-Item -LiteralPath $releaseFolder -Recurse -Force }
$null = New-Item -ItemType Directory -Force -Path $releaseFolder
Copy-Item -LiteralPath $BinaryPath -Destination (Join-Path $releaseFolder 'Kroma Desktop.exe')
Copy-Item -LiteralPath $loaderPath -Destination (Join-Path $releaseFolder 'WebView2Loader.dll')
Copy-Item -LiteralPath (Join-Path $desktopRoot 'PORTABLE-README.txt') -Destination (Join-Path $releaseFolder 'README.txt')
$hashes = @()
foreach ($name in @('Kroma Desktop.exe', 'WebView2Loader.dll')) {
  $fileHash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $releaseFolder $name)).Hash.ToLowerInvariant()
  $hashes += "$fileHash  $name"
}
Set-Content -LiteralPath (Join-Path $releaseFolder 'SHA256.txt') -Value $hashes -Encoding ascii
$zip = Join-Path $OutputDirectory "$releaseName.zip"
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
Compress-Archive -LiteralPath $releaseFolder -DestinationPath $zip -CompressionLevel Optimal
Write-Host "Portable release: $releaseFolder"
Write-Host "ZIP: $zip"
Write-Host "Size: $([math]::Round((Get-Item -LiteralPath $zip).Length / 1MB, 1)) MB"

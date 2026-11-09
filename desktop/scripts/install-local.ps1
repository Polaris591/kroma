# Deploy a locally built Kroma Desktop installer to the current Windows user.
# Clean only known Kroma build artifacts from the Windows Desktop, preserving
# every file in a project-side archive and leaving one proper app shortcut.
[CmdletBinding()]
param(
  [switch]$SkipLaunch,
  [switch]$SkipInstall,
  [switch]$KeepDesktopArtifacts
)
$ErrorActionPreference = 'Stop'
$desktopRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$installer = Join-Path $desktopRoot 'src-tauri\target\x86_64-pc-windows-gnu\release\bundle\nsis\Kroma Desktop_0.3.0_x64-setup.exe'
$userDesktop = [Environment]::GetFolderPath('Desktop')
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
  throw "Kroma Desktop 0.3.0 release installer not found: $installer"
}
$running = @(Get-Process -Name 'kroma-desktop','Kroma Desktop','kroma-desktop-preview' -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
  throw 'An existing Kroma Desktop is running. Close it first to avoid replacing an active binary or competing for session locks.'
}
Write-Output "INSTALLER: $installer"
Write-Output "INSTALLER SHA256: $((Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant())"
if (-not $SkipInstall) {
  $setup = Start-Process -FilePath $installer -ArgumentList '/S' -Wait -PassThru
  if ($setup.ExitCode -ne 0) {
    throw "The Kroma Desktop NSIS installer failed with exit code $($setup.ExitCode). Existing Desktop files were not moved."
  }
  Write-Output 'INSTALL: NSIS installer completed successfully.'
} else {
  Write-Output 'INSTALL: preserving already installed 0.3.0 from the previous successful installer invocation.'
}

# Prefer the registered per-user installation; fall back to the two common
# Tauri/NSIS current-user locations if the registry has no InstallLocation.
$installed = @()
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall'
if (Test-Path $uninstallKey) {
  $entries = @(Get-ChildItem $uninstallKey -ErrorAction SilentlyContinue |
    Get-ItemProperty -ErrorAction SilentlyContinue |
    Where-Object { $_.DisplayName -like '*Kroma Desktop*' })
  foreach ($entry in $entries) {
    if ($entry.InstallLocation) {
      # NSIS writes quoted InstallLocation registry values.
      $directory = ([string]$entry.InstallLocation).Trim().Trim('"')
      if ($directory -and (Test-Path -LiteralPath $directory -PathType Container)) {
        foreach ($filename in @('kroma-desktop.exe','Kroma Desktop.exe')) {
          $installed += Join-Path $directory $filename
        }
      }
    }
    if ($entry.DisplayIcon) {
      $installed += ($entry.DisplayIcon -replace ',\d+$','').Trim('"')
    }
  }
}
foreach ($base in @(
  (Join-Path $env:LOCALAPPDATA 'Kroma Desktop'),
  (Join-Path $env:LOCALAPPDATA 'Programs\Kroma Desktop')
)) {
  foreach ($filename in @('kroma-desktop.exe','Kroma Desktop.exe')) {
    $installed += Join-Path $base $filename
  }
}
$installedExe = $installed | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
  Select-Object -First 1
if (-not $installedExe) {
  throw 'NSIS exited successfully, but the installed executable could not be located. Desktop artifacts were not moved.'
}
$installedExe = (Resolve-Path -LiteralPath $installedExe).Path
Write-Output "INSTALLED EXE: $installedExe"
Write-Output "INSTALLED VERSION: $((Get-Item -LiteralPath $installedExe).VersionInfo.ProductVersion)"

# Verify real GUI startup before making the desktop changes. Do not close
# the successfully launched app; the user should be able to start using it.
if (-not $SkipLaunch) {
  $app = Start-Process -FilePath $installedExe -PassThru
  Start-Sleep -Seconds 6
  $app.Refresh()
  if ($app.HasExited -or $app.MainWindowHandle -eq 0) {
    throw "The installed Kroma Desktop did not open a visible window. Desktop artifacts were not moved."
  }
  Write-Output "GUI STARTUP: OK, PID=$($app.Id), TITLE=$($app.MainWindowTitle)"
}

$shortcutPath = Join-Path $userDesktop 'Kroma Desktop.lnk'
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($shortcutPath)
$link.TargetPath = $installedExe
$link.WorkingDirectory = Split-Path -Parent $installedExe
$link.IconLocation = "$installedExe,0"
$link.Description = 'Kroma Desktop 0.3.0 (installed application)'
$link.Save()
if (-not (Test-Path -LiteralPath $shortcutPath -PathType Leaf)) {
  throw 'Failed to create the new Desktop shortcut. Existing build artifacts were not moved.'
}
Write-Output "DESKTOP SHORTCUT: $shortcutPath"

if (-not $KeepDesktopArtifacts) {
  # These seven filenames were produced by our earlier Desktop build workflow.
  # Move them into the existing release directory instead of deleting anything.
  $archive = Join-Path $desktopRoot 'release\desktop-file-archive'
  $null = New-Item -ItemType Directory -Path $archive -Force
  $oldFiles = @(
    'Kroma Desktop 0.2.0 Portable.zip',
    'Kroma Desktop 0.2.0 Setup.exe',
    'Kroma Desktop 0.2.1 Portable.zip',
    'Kroma Desktop 0.2.1 Setup.exe',
    'Kroma Desktop 0.3.0 Portable.zip',
    'Kroma Desktop 0.3.0 Setup.exe',
    'Kroma Desktop.exe'
  )
  foreach ($filename in $oldFiles) {
    $source = Join-Path $userDesktop $filename
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { continue }
    $destination = Join-Path $archive $filename
    if (Test-Path -LiteralPath $destination) {
      $stem = [System.IO.Path]::GetFileNameWithoutExtension($filename)
      $extension = [System.IO.Path]::GetExtension($filename)
      for ($counter = 1; $counter -lt 1000; $counter++) {
        $candidate = Join-Path $archive "$stem-archive-$counter$extension"
        if (-not (Test-Path -LiteralPath $candidate)) {
          $destination = $candidate
          break
        }
      }
      if (Test-Path -LiteralPath $destination) {
        throw "No unused archive filename for $filename"
      }
    }
    Move-Item -LiteralPath $source -Destination $destination -ErrorAction Stop
    Write-Output "ARCHIVED: $filename"
  }
  Write-Output "DESKTOP ARCHIVE: $archive"
}
Write-Output 'DEPLOYMENT COMPLETE'

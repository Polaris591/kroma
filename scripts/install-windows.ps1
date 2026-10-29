param([switch]$Offline)
$ErrorActionPreference = "Stop"

# One default location, no silent process kills or session interruption.
$running = @(Get-CimInstance Win32_Process |
    Where-Object { $_.Name -eq "kroma.exe" })
if ($running.Count -gt 0) {
    $list = ($running | ForEach-Object {
        "PID $($_.ProcessId): $($_.ExecutablePath)"
    }) -join "; "
    throw "Close any running Kroma terminals before updating: $list"
}

$root = Split-Path -Parent $PSScriptRoot
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin\kroma.exe"
$shadow = Join-Path $env:USERPROFILE "bin\kroma.exe"
$arguments = @("install", "--path", $root, "--locked", "--force")
if ($Offline) { $arguments += "--offline" }
& cargo @arguments
if ($LASTEXITCODE -ne 0) { throw "Cargo install failed; existing binary preserved." }

# The historical ~/bin copy shadows Cargo's binary on Windows PATH.
if ((Test-Path -LiteralPath $shadow) -and ($shadow -ne $cargoBin)) {
    Remove-Item -LiteralPath $shadow -Force
    Write-Output "Removed shadow binary: $shadow"
}
if (-not (Test-Path -LiteralPath $cargoBin)) {
    throw "Cargo installation did not produce $cargoBin"
}
& $cargoBin --build-info
Write-Output "PATH candidates:"
& where.exe kroma
Write-Output "Restart the terminal to pick up the new executable."

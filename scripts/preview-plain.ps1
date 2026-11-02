# Compatibility launcher: A / Plain is now Kroma's default TUI.
# No environment flag or persistent UI preference change is needed.
$manifest = Join-Path $PSScriptRoot '../Cargo.toml'
& cargo run --locked --manifest-path $manifest -- @args
exit $LASTEXITCODE

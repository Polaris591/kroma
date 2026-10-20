#!/bin/sh
# Run from the extracted Kroma source root, with a Rust 2021-compatible toolchain installed.
set -eu
cd "$(dirname "$0")/.."
printf '%s\n' '[0] Offline source wiring checks'
python3 scripts/verify_nextgen_static.py
python3 scripts/test-triad-paired-gate.py
printf '%s\n' '[1] Workspace formatting'
cargo fmt --all -- --check
printf '%s\n' '[2] Cache wire-mode regression fixtures'
cargo test -p kroma-core --lib nextgen_cache_wire_tests --locked
printf '%s\n' '[3] Cache-aware compaction budget decisions and history invariants'
cargo test -p kroma-core --lib cache_aware_compaction --locked
cargo test -p kroma-core --lib nextgen_history_guard_tests --locked
printf '%s\n' '[4] MCP HTTP protocol isolated fixtures + trusted config'
cargo test -p kroma-core --test mcp_http_2026 --locked
cargo test -p kroma-core --test mcp_config --locked
printf '%s\n' '[5] P0 cache write accounting parser and cost ledger tests'
cargo test -p kroma-core --lib cache_write_accounting_tests --locked
cargo test -p kroma-core --lib cache_writes_cannot_be_mispriced_as_free_or_uncached --locked
cargo test -p kroma-core --lib cache_write_unknown_is_not_a_measured_zero_on_writing_models --locked
printf '%s\n' '[5b] Native search replay / malformed provider guard'
cargo test -p kroma-core --lib native_tool_search_replay_guard_tests --locked
cargo test -p kroma-core --lib malformed_response_shape_tests --locked
printf '%s\n' '[6] Full workspace regression'
cargo test --workspace --locked
printf '%s\n' '[7] Strict workspace clippy'
cargo clippy --workspace --all-targets --locked -- -D warnings
printf '%s\n' 'RUST OFFLINE GATES PASSED. Live-provider canary and A/B remain separate mandatory gates.'

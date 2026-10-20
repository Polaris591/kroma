#!/usr/bin/env python3
"""Offline source integrity checks, NOT a Rust compiler or a provider test."""
from pathlib import Path
import subprocess, sys
root = Path(__file__).resolve().parents[1]
checks = {
  'OpenAI explicit input breakpoint': ('crates/kroma-core/src/provider/openai_responses.rs','prompt_cache_breakpoint'),
  'OpenAI conservative capability gate': ('crates/kroma-core/src/provider/openai_responses.rs','model_capabilities(model)'),
  'OpenAI usage split invariant': ('crates/kroma-core/src/provider/openai_responses.rs','validate_provider_cache_usage'),
  'Unknown cache writes do not appear free': ('crates/kroma-core/src/usage_cost.rs','cache-write usage unknown'),
  'MCP stateless HTTP adapter': ('crates/kroma-core/src/mcp_http.rs','MCP_HTTP_PROTOCOL'),
  'MCP metadata header': ('crates/kroma-core/src/mcp_http.rs','MCP-Protocol-Version'),
  'MCP annotated parameter headers': ('crates/kroma-core/src/mcp_http.rs','compile_headers'),
  'MCP cache isolated per server instance': ('crates/kroma-core/src/mcp_http.rs','registered_tools: HashMap'),
  'Legacy stdio remains present': ('crates/kroma-core/src/mcp.rs','pub async fn initialize'),
  'Trusted config HTTP option': ('crates/kroma-core/src/config_file.rs','"http_url" => server.http_url'),
  'Context cache-aware opt-in': ('crates/kroma-core/src/agent_loop.rs','KROMA_CACHE_AWARE_COMPACTION'),
  'Context non-tool invariant': ('crates/kroma-core/src/context_projector.rs','authoritative_history_preserved'),
  'MCP live loopback Rust fixture': ('crates/kroma-core/tests/mcp_http_2026.rs','http_mcp_2026_json_catalog'),
  'Native tool-search replay refuses unsupported item': ('crates/kroma-core/src/provider/openai_responses.rs','reject_unsupported_native_output'),
  'MCP bounded annotated headers': ('crates/kroma-core/src/mcp_http.rs','MAX_TOTAL_ANNOTATED_HEADER_BYTES'),
  'MCP strict JSON-RPC response shape': ('crates/kroma-core/src/mcp_http.rs','exactly one of result/error'),
  'Compaction discounts non-reclaimable multimodal output': ('crates/kroma-core/src/context_projector.rs','reclaimable_tool_bytes'),
  'Provider-aware paired gate': ('scripts/triad_paired_gate.py','no_cost_saving_claim_unknown_billing'),
  'Provider-aware paired gate fixtures': ('scripts/test-triad-paired-gate.py','test_quality_regression_blocks_savings'),
  'Context planner Rust fixture': ('crates/kroma-core/src/cache_aware_compaction.rs','budget_never_sacrificed_for_cache_hit'),
}
failures = []
for label,(file,needle) in checks.items():
    f=root/file
    if not f.exists() or needle not in f.read_text('utf8'):
        failures.append(label)
    print(('PASS' if label not in failures else 'FAIL')+' static: '+label)
if (root/'.git').is_dir():
    cmd=subprocess.run(['git','diff','--check','HEAD'],cwd=root,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
    print('PASS git diff --check' if cmd.returncode==0 else 'FAIL git diff --check')
    if cmd.returncode: failures.append('git diff --check')
else:
    print('SKIP git diff --check (the source archive intentionally excludes VCS history)')
print('Rust compiler/tests: NOT RUN (static source checks are not a substitute)')
if failures:
    print('Failed:',', '.join(failures),file=sys.stderr)
    sys.exit(1)

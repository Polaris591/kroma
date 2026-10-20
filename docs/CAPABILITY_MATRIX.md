# 能力支持矩阵（M4-03，由 docs/capabilities.json 生成）

由 `scripts/capability-matrix.py` 生成，不手工维护。六轴含义见蓝图 §5.3。

| capability | delivery | platform | authorization | lifecycle | evidence | default | wired |
|---|---|---|---|---|---|---|---|
| host_verification_policy | implemented | unverified | default_on | not_managed | contract_tested | on | rpc |
| canonical_tournament_intent | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc |
| host_budget_ceiling | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc,bench |
| request_budget_lifecycle | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc |
| typed_compaction_metadata | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc,resume |
| bounded_io | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc,bench,worker |
| wal_capacity | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc,resume |
| windows_writer_ownership | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc |
| execution_boundary | implemented | unverified | requires_decision | not_managed | live_tested | off_or_conditional | cli |
| container_lifecycle | implemented | unverified | requires_decision | not_managed | live_tested | off_or_conditional | cli |
| run_plan_assembly | implemented | unverified | default_on | not_managed | contract_tested | on | cli,bench |
| tool_discovery_activation | implemented | unverified | requires_decision | not_managed | contract_tested | off_or_conditional | cli,rpc |
| mcp_handshake | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc |
| cache_breakpoints | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc |
| evidence_store | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc,resume |
| repo_graph_budget | implemented | unverified | default_on | not_managed | contract_tested | on | cli,rpc |
| experiment_manifest | implemented | unverified | requires_decision | not_managed | contract_tested | off_or_conditional | cli |
| usage_cost_ledger | implemented | unverified | requires_decision | not_managed | contract_tested | off_or_conditional | none |
| provenance_identity | implemented | unverified | default_on | not_managed | contract_tested | on | cli,bench |
| ahe_isolation | implemented | unverified | default_on | not_managed | contract_tested | on | cli |

## 读法与边界

- 共 20 项，其中 default_on 15 项、contract_tested 20 项。
- `platform_support` 全部是 `unverified`：本树没有任何第二平台的能力级验证。
- `lifecycle` 全部是 `not_managed`：激活租约（C00-04/05）尚未接进运行路径。
- `live_tested` 全部为 false：没有真实 Provider 或真实沙箱的能力级实测。
- 发布成熟度**不**由本矩阵给出；正式发布需要所有者单独授权（本目标边界内不做）。

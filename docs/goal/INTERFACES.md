# Goal 接口与授权边界（M0-02）

两件事分开写：**接口是什么**（由二进制和 API 自己回答），**能做什么**（由所有者批准的
`ENVELOPE.json` 回答）。两者都不靠猜。

复算：

```bash
python3 scripts/goal-interface.py probe --ci          # 记录真实接口事实
python3 scripts/goal-interface.py authorize <op>      # 退出码 0/2/3/4
python3 scripts/test-goal-interface.py                # A1/A2/A3 三条定向检查
```

## 1. 已核实的接口（2026-10-18，HEAD e51c0a3）

| 接口 | 事实 | 来源 |
|---|---|---|
| `kroma` 二进制 | `kroma 1.5.0`，与 `Cargo.toml` 工作区版本一致 | `--version` 实跑 |
| 入口 surface | `run` / `repl` / `acp` / `serve` / `bench` 均出现在二进制自己的 `--help` | `--help` 实跑，记录 help 的 sha256 |
| 工作区版本 | workspace `1.5.0`（`kroma-core` + `kroma-cli`） | `Cargo.toml` |
| 工具链 | rustc `1.85.1` | `rustc --version` |
| git | 分支 `integration/v4`，无 remote | `git remote -v` |
| GitHub 仓库 API | `Polaris591/kroma` → HTTP 200，public | `api.github.com` |
| 最近 workflow run | id `39106253179`，conclusion `success` | runs API |
| 该 run 的 artifact | `total_count = 0` | artifacts API |

`--help` 是手写分节文本，不是 clap 生成的结构化清单。因此探针**不做语法解析**，
只记录 help 全文摘要并逐一确认本目标依赖的五个 surface 是否被二进制自己声明。

## 2. 授权矩阵（`docs/goal/ENVELOPE.json`）

| 操作 | 决定 | 退出码 | 依据 |
|---|---|---|---|
| `local_work` | allowed | 0 | 所有者确认：本树内实现 32 包 |
| `paid_provider` | allowed | 0 | 所有者明确无限额授权 `gemini-3.8-flash`（`cobblemon/gemini-3.8-flash-high`）；每组实验仍须先落预注册 |
| `ci_artifact_read` | **BLOCKED** | 2 | 环境无 `GITHUB_TOKEN`；artifact 下载需 `actions:read`，且最近一次 run 的 artifact 数为 0 |
| `production_key` | **REFUSED** | 3 | 生产签名密钥离线，本目标不读不签 |
| `release` | **REFUSED** | 3 | 正式发布需单独授权 |
| `push_protected` | **REFUSED** | 3 | 本树无 remote，推受保护分支不在范围 |

退出码约定：`0` 允许、`2` 合法但缺接口/凭据（BLOCKED，带原因）、`3` 未授权（REFUSED，带原因）、
`4` envelope 缺失或损坏。未知操作一律 `3`，不会因为"没写规则"而被默认放行。

## 3. 明确未打通的部分

- **CI 侧证据**：本树没有 remote，`ci_artifact_read` 为 BLOCKED。要解除，需导出带
  `actions:read` 的 `GITHUB_TOKEN` 后重跑 `probe --ci`；即使解除，最近一次 run 也没有 artifact 可读，
  所以"candidate tree 的 CI 证据"当前不存在，不能拿 public 仓库的历史 run 顶替。
- **Worker 委派**：本目标由所有者指定为直接执行，不再走 Controller→Worker 通道。
  `controller-interface-status.json` 记录的是上一轮的历史事实，不再作为当前执行路径。
- **付费 pilot**：授权已存在，但 M1-06 之前需要 M1-01..05 的边界先落地；在此之前不发真实请求。

## 4. 证据

- `/opt/kroma-v4/evidence/m0-02/interface-probe.json`：本文件第 1 节全部事实的机器输出
- `/opt/kroma-v4/evidence/m0-02/test-goal-interface.log`：A1/A2/A3 定向检查
- `/opt/kroma-v4/evidence/m0-02/clippy.log`、`full-suite.log`：包收口门禁

# M4-02 可撤销经验与记忆修订

`ExperienceStore` 早已存在（978 行，含 scope/source 信任规则、counterexample、版本失效、revoke、
带预算的 retrieve）。本包不改产品代码，补三条钉住性质的测试 `tests/m402_experience.rs`：

- **A1 反证是新增关系**：`with_counterexample` 只附加 counterexample，原 `evidence` 不变；
  `revoke` 后记录仍可读、`revoked_reason` 保留、原始证据一字不改。
- **A2 按来源限 scope**：`Repository` 来源不能 `promote` 到 `Global`，`Model` 来源不能到 `Repo`，
  都报 `UntrustedSource`。
- **A3 顺序与撤销**：`retrieve(Repo)` 会带出更窄的 Task 级经验（更窄的作用域在更宽上下文里适用），
  **不会**带出更宽的 Global 级；撤销后不再被检索但 `get` 仍可取；无证据的 accept 报 `MissingEvidence`。

过程中我误覆盖了 `experience.rs`（写完才发现基线已有同名模块），已 `git checkout` 还原，
本包最终是**零产品代码改动**。这条要记着：新建模块前先确认同名文件不存在。

定向 3 条通过；全量见 `evidence/m4-02/full-suite.log`。

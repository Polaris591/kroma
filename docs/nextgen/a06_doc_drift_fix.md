# KR-A06 文档漂移修正记录

修正对象：`docs/CAPABILITIES.md` 的「许可证」一节。
修正后文件 sha256：`805dab708409ea3800e6b05b2dc7d50ab937e549d5ab0ec339dd338c34a09e35`

## 漂移

该节原文写着「许可证文件与条款**不由本审查代为选择**：需要项目所有者确认后再补」，
但根目录 `LICENSE` 已是 MIT（Copyright (c) 2026 Kroma contributors），`Cargo.toml` 的
`license` 字段也是 MIT。文档与源码事实不一致。

## 处理

按任务要求「保留历史报告并加 superseded 标识，不覆盖原始证据」：

- 当前状态改为「已选定：MIT」，并指向 `LICENSE` 与 `docs/nextgen/dependency_licenses.json`；
- 原文以 superseded 引用块保留，注明 2026-10-21 / KR-A06，不再代表当前状态。

没有删除历史结论：漂移本身需要可追溯，删掉就等于把一次文档事故抹平。

## 未处理

其他历史报告（GATE0-8_ACCEPTANCE_REPORT.md 等）未逐一复核版本一致性。它们记录的是各自
交付时刻的状态，不构成本轮结论；本轮只修正了与源码事实直接冲突的许可证表述。

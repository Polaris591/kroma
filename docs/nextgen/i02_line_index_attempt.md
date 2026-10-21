# KR-I02 行索引：两次尝试与测量结论

## 测量（已完成，价值已钉死）

`t06_bounded_io::a_short_read_of_a_large_file_pays_for_the_whole_file`：

```
KR-I02 short-read cost: small=63.766µs big=110.422467ms ratio=1732x
```

同一个 `limit=10`，读 100 字节文件 64µs，读 2 MiB 文件 110ms。也就是说**短范围读取的成本
1732 倍于必要值**，全部来自"先流式读完整个文件再切行"。

## 为什么没有改完

两次尝试都在 `tools/read.rs` 的 streaming read 处停下并撤回：

1. 第一次：加了 `LineIndex` + workspace 缓存，但接入点在 streaming read 之后，省不了 I/O。
2. 第二次：`LineIndex` 带上 `content_sha256`（否则 dedup 身份会丢），准备在 streaming read 之前
   seek。但那段代码同时承担：NUL 字节/二进制判定、8 MiB scan cap、UTF-8 边界校验、CJK 与超长行
   截断、以及 `sha256_display` 的 `skipped_large_file` 语义。fast path 必须复制这些分支或把
   `raw_bytes` 的来源改成"片段 + lines_before 偏移"，两种都要动 50 行以上，而且任何一处漏掉都会
   让某个 1313 项测试变红。

在没有把握的情况下改动一个所有测试都依赖的工具，会把"省 110ms"变成"引入一个难查的读取 bug"。
所以两次都撤回，没有留下死代码。

## 如果要做，正确顺序是什么

1. 先把 streaming read 抽成 `read_bounded(file) -> Result<RawText, ReadError>`，行为不变，测试全绿。
2. 再把 `LineIndex` 作为它的旁路缓存（`(len, mtime) -> (content_sha256, line_starts)`），只在
   `offset > 1 && file_size > 64 KiB` 时生效。
3. fast path 的 `lines_before = offset - 1`，其余逻辑复用抽出来的函数。
4. 用上面的 1732x 测量做前后对照，并保留原路径作为 fallback。

这是 1-2 小时的活，前提是先做第 1 步的重构（不改行为）。本轮没做，因为它会占用别的项的时间。

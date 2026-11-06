# Review and design archive

This directory preserves investigations, design iterations, screenshots and local
verification notes from earlier Kroma development. **It is an archive, not an
up-to-date list of defects or an assertion that every recorded test still passes.**
Some entries describe old branches, rejected approaches or environment-specific
experiments.

## Where to look

- [Deep review and follow-up evidence](deep-review/) — review probes, repair
  notes and verification output.
- [Release preparation evidence](release-prep/) — staged package, test and
  document-check captures; not proof that a release was published.
- [Batch 3/4 measurements](batch-3-4/) — comparisons and performance fixtures.
- [Model catalog research](catalog-expansion/) — historical model candidates.
- TUI studies and visual iterations: directories starting with **tui-**.

For **current behavior**, consult the [source and tests](../src/),
[architecture](../docs/architecture.md), [documentation index](../docs/README.md)
and [latest commits](https://github.com/Polaris591/kroma/commits/main).
Do not derive a live defect, security guarantee or benchmark ranking from an
archived report without repeating its test against the target commit.

Historical files are deliberately retained in place to preserve links and
context. Do not rewrite or delete evidence as part of routine documentation
tidying. New accepted behaviors belong in current guides and source tests.

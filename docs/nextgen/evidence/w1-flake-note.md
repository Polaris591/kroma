# W1 flake record

`cargo test --workspace` (run 7) reported 1289 passed, 1 failed:
`b08c_git_metadata::carries_a_sha256_commit_through`.

This is the known parallel-load flake in the baseline suite, not a regression from W1: the target
passes 5/5 twice when run exclusively with `--test-threads=1`.

Logs: `w1-b08c-exclusive-run1.log`, `w1-b08c-exclusive-run2.log`.
Full-suite log including the failure: `w1-workspace-test.log`.

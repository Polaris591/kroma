## Summary

What problem does this solve, and what changed?

## Scope and design

- Why is this the smallest reasonable change?
- Does it affect Agent Loop, provider wire formats, WAL, context projection, plugins, cache behavior or the CLI/JSONL contract?
- Are default no-plugin behavior and existing sessions preserved?

## Validation

List **commands actually executed** and their results. Mark unrun checks explicitly.

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --locked`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] Relevant JS plugin / npm tests
- [ ] Relevant Desktop / Project Memory tests (if changed)
- [ ] Platform or live-provider tests (if required)

## Risks and compatibility

Document remaining uncertainty, migrations, performance impacts and cancellation/error behavior.

## Safety and release

- [ ] No credentials, private session contents or generated secret files included
- [ ] No undocumented network calls, implicit model requests or new approval flows
- [ ] User docs and changelog considered where behavior changed

See [CONTRIBUTING.md](../CONTRIBUTING.md). A checked box is a contributor's assertion, not proof of CI status.

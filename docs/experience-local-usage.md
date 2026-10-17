# Explicit local experience (experimental)

Default OFF. Available in `kroma repl` / `kroma tui` through the trusted local terminal only:

1. `/experience load /absolute/private-dir/candidates.json`
2. `/experience list` — inspect premise, action and evidence yourself.
3. `/experience accept repo:KEY` — explicitly accept one reviewed candidate for this process.
4. `/experience on` — enable bounded request-context injection.
5. `/experience revoke repo:KEY` or `/experience off` — stop using it.

`/experience status` reports enablement and the bound revision. This command is not registered as
JSON-RPC, a model tool, or a remotely callable mutation. Candidates and accepted decisions live in
one local dispatcher process; restarting requires reloading/reviewing/accepting. Reloading after a
successful import is refused, so it cannot clear a local revocation. The first delivery does not
include headless flags, cross-process persistence, automatic learning or global promotion.

Linux import requirements: candidates must be outside the workspace, in a current-user-owned 0700
parent directory; the regular file must be current-user-owned, mode0600, nlink1, not a symlink, at
most256KiB. Other platforms refuse import until a verified private-ACL contract exists. No credentials
belong in the file. The candidate schema rejects unknown fields, including `status`, `accepted_by`,
`source`, or self-reported trusted identity. Maximum64 records; each field at most1024 Unicode chars.

Example (replace workspace/revision with the actual canonical workspace and current content revision):

    {
      "workspace_root": "/absolute/project",
      "revision": "CURRENT_CONTENT_REVISION",
      "records": [{
        "key": "parser-boundaries",
        "premise": "working on parser boundary cases",
        "action": "re-run the existing boundary checks before concluding",
        "evidence": "operator-reviewed run/evidence reference",
        "required_policy": "allow-write",
        "counterexample": "does not apply to unrelated components",
        "conditions": [{"path":"src/parser.rs","sha256":"64_HEX_DIGITS"}],
        "expires_at_ms": 2000000000000
      }]
    }

Find the content revision in session status/revisions after a real prompt (or obtain it with the
SnapshotStore content-identity API). Imported workspace+revision must match live tracked bytes,
not merely an old session status. Any tracked content change stops injection until a new process
loads/reviews an applicable file. Exact `required_policy` must match both acceptance and use.
Optional file SHA conditions are checked through WorkspaceCap/no-follow and bounded reads.
Expiry, revocation, and candidate status are checked again for each model request.

Injected guidance is repository data, not tool authority. Existing policy/approval remains decisive.
Up to8 applicable records and at most2KiB of dynamic context are added to the current request, with
JSON escaping. The system prompt/tool-schema prefix stays byte-identical. Injection is deliberately
not appended to the session transcript/WAL, so off/revoke does not leave the guidance in replayed
history. The injected bytes enter prompt-size/token reservation estimates. With the feature off,
no extra workspace scan or prompt mutation is performed.

Limits: snapshots exclude sensitive/oversized/special files; this is not a proof about arbitrary
external side effects. Same-uid malicious code can attack local files/processes: private paths are
not a substitute for the OS sandbox. Explicit local acceptance is an operator decision, not a claim
that the runtime independently proved every prose statement true. Model output is never imported
as automatically trusted knowledge.

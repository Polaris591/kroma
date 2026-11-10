#!/usr/bin/env python3
"""Round 12 preregistered multi-arm prompt ablation with independent acceptance.

Six arms on one fixed binary and provider profile; no different model, tools,
terminal dialect, output format, temperature, budgets, or context policy.
Crossed deterministic pair orders and resumable one-shot results. All user
project code is written only inside isolated synthetic fixtures.

Runs are billable. A run is a strict success only when: an external oracle
passes, the agent itself ran Cargo tests, and it exited normally before cap.
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
from pathlib import Path
import random
import subprocess
import tarfile

import round4_windows as original_tasks
import round7_readable_shell_holdout as unicode
import round9_complete_function_read as work
import round11_finish_guidance as prior
import agent_task_suite as suite
from round9_post_success_audit import analyze, load_trace

ROOT = Path(__file__).resolve().parents[1]
LAB = Path("D:/Kroma/experiments/kroma-round12-prompt-tournament")
ARMS = ("baseline", "lean", "focus", "finish", "focus_finish", "dynamic")
SCREEN = ("rust_multifile", "rust_sparse_lookup", "rust_restore_metadata", "rust_history_unicode")
HOLDOUT = ("rust_session_unicode_preview",)
ALL_TASKS = SCREEN + HOLDOUT
SEED = 20261110

# A genuinely new module/path. The lowercased hit position measures expanded
# Unicode scalar count but incorrectly slices the original Unicode text.
PREVIEW_TEST = '''
#[cfg(test)]
mod round12_session_unicode_preview {
    use super::*;

    #[test]
    fn snippet_centers_case_fold_expansion() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "round12-unicode-preview-{}-{}", std::process::id(),
            NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root)?;
        let catalog = SessionCatalog::from_default(root.join("main.json"))?;
        let (_, path) = catalog.new_path()?;
        let store = SessionStore::open_at(path, &root)?;
        let text = "İ".repeat(90) + "UNIQUE_SESSION_NEEDLE" + &"尾".repeat(70);
        store.save(
            &[
                crate::protocol::Message::new("system", "Round12 fixture identity"),
                crate::protocol::Message::new("user", text),
            ],
            &crate::context_state::CompactionState::default(),
        )?;
        drop(store);
        let hits = catalog.search_user_messages("unique_session_needle", 5, 10)?;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("UNIQUE_SESSION_NEEDLE"),
            "snippet omitted actual match: {:?}", hits[0].snippet);
        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
'''

PREVIEW_PROMPT = (
    "In this real Rust codebase, Desktop session search finds a message but "
    "the displayed preview can omit the actual matched phrase when the "
    "message contains Unicode that expands under lowercase mapping before "
    "the match. Fix the snippet positioning while keeping its length bound, "
    "user-only filter, and other session behaviors. Do not remove or modify "
    "existing tests; run the targeted cargo test --locked --offline --lib "
    "round12_session_unicode_preview and report verified behavior. "
    "Work only in this isolated checkout; do not commit or access credentials."
)

# Blind sparse coverage: unlike the previous round, do NOT tell the agent that
# its existing tests omit a requirement. The user still explicitly requires
# correct lookups, so a green test alone is inadequate.
SPARSE_PROMPT = prior.SPARSE["prompt"].replace(
    "The existing targeted parser test only exercises parse_records; "
    "do not assume that proves get_record is correct. ", ""
)

def sha(blob: str) -> str:
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()


def environment(project: Path, trial: Path, arm: str) -> dict[str, str]:
    env = prior.OLD_ENVIRONMENT(project, trial, "baseline")
    # prior.OLD_ENVIRONMENT is the original round9 helper, without any new arm.
    if arm in ("lean", "focus", "finish", "focus_finish"):
        env["KROMA_EXPERIMENT_PROMPT_ARM"] = arm
    elif arm == "dynamic":
        env["KROMA_EXPERIMENT_ADAPTIVE_FINISH"] = "1"
    return env


def prepare_preview(project: Path) -> str:
    archive = subprocess.check_output(["git", "archive", "HEAD"], cwd=ROOT)
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as jar:
        jar.extractall(project, filter="data")
    path = project / "src" / "sessions.rs"
    source = path.read_text(encoding="utf-8")
    if "let position = lower[..index].chars().count();" not in source:
        raise RuntimeError("preview fixture drift")
    if "round12_session_unicode_preview" in source:
        raise RuntimeError("new preview regression module already present")
    path.write_text(source + "\n" + PREVIEW_TEST, encoding="utf-8")
    return sha(PREVIEW_TEST)


def prepare_fixture(task: str, project: Path) -> str:
    if task == "rust_session_unicode_preview":
        return prepare_preview(project)
    if task == "rust_history_unicode":
        return unicode.export_fixture(project)
    if task == "rust_sparse_lookup":
        suite.create_fixture(project, prior.SPARSE)
        return hashlib.sha256((project / "tests/basic.rs").read_bytes()).hexdigest()
    return prior.OLD_PREPARE(task, project)


def verify_preview(project: Path, env: dict, fingerprint: str):
    path = project / "src" / "sessions.rs"
    source = path.read_text(encoding="utf-8")
    if not source.endswith(PREVIEW_TEST) or sha(PREVIEW_TEST) != fingerprint:
        return False, "regression tests were modified"
    a, log_a = work.cargo_test(project, env, "round12_session_unicode_preview")
    b, log_b = work.cargo_test(project, env, "search_only_user_messages_in_recent_archives")
    return a and b, (
        "preview and existing search tests pass"
        if a and b else ("preview: "+log_a+" existing: "+log_b)[-900:]
    )


def verify(task: str, project: Path, env: dict, frozen: str):
    if task == "rust_session_unicode_preview":
        return verify_preview(project, env, frozen)
    if task == "rust_history_unicode":
        return unicode.verify(project, frozen)
    if task == "rust_sparse_lookup":
        return prior.verify(task, project, env, frozen)
    return prior.OLD_VERIFY(task, project, env, frozen)


def setup() -> None:
    # The round9 runner resolves all helper names dynamically at runtime.
    # No modifications to the old benchmark sources.
    work.LAB = LAB
    work.environment = environment
    work.prepare_fixture = prepare_fixture
    work.verify = verify
    original_tasks.TASKS["rust_history_unicode"] = {"prompt": unicode.PROMPT}
    original_tasks.TASKS["rust_sparse_lookup"] = {"prompt": SPARSE_PROMPT}
    original_tasks.TASKS["rust_session_unicode_preview"] = {"prompt": PREVIEW_PROMPT}


def order_for(task: str, repeat: int, arms: tuple[str, ...]) -> list[str]:
    # Fix arm order *per task* and reverse every second repeat, so splitting
    # a trial across invocations cannot accidentally reset the crossover.
    order = list(arms)
    seed = int.from_bytes(hashlib.sha256(f"{SEED}:{task}".encode()).digest()[:8])
    random.Random(seed).shuffle(order)
    if repeat % 2 == 0:
        order.reverse()
    return order


def preflight() -> None:
    root = LAB / "preflight"
    # Name includes fixture hash to make accidental reuse detectable.
    project = root / "preview-broken"
    project.mkdir(parents=True, exist_ok=False)
    frozen = prepare_preview(project)
    env = environment(project, root, "baseline")
    ok, detail = verify_preview(project, env, frozen)
    if ok:
        raise RuntimeError("preview negative control unexpectedly PASSES")
    print("PREVIEW_NEGATIVE_CONTROL_FAILS", detail[-450:], flush=True)
    sparse = root / "sparse-broken"
    sparse.mkdir(parents=True, exist_ok=False)
    frozen = prepare_fixture("rust_sparse_lookup", sparse)
    env = environment(sparse, root, "baseline")
    test_ok = subprocess.run(
        ["cargo", "test", "--locked", "--offline", "--test", "basic"],
        cwd=sparse, env=env, capture_output=True, timeout=70,
    ).returncode == 0
    verified, _ = verify("rust_sparse_lookup", sparse, env, frozen)
    if not test_ok or verified:
        raise RuntimeError("sparse counterexample must have green public test but fail hidden acceptance")
    print("SPARSE_POSITIVE_PUBLIC_AND_NEGATIVE_HIDDEN_CONFIRMED", flush=True)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--tasks", default=",".join(SCREEN))
    p.add_argument("--arms", default=",".join(ARMS))
    p.add_argument("--repeat-start", type=int, default=1)
    p.add_argument("--repeats", type=int, default=1)
    p.add_argument("--timeout", type=int, help="override each task's default timeout")
    p.add_argument("--max-runs", type=int, default=3)
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--preflight", action="store_true")
    args = p.parse_args()
    setup()
    arms = tuple(a.strip() for a in args.arms.split(",") if a.strip())
    tasks = tuple(t.strip() for t in args.tasks.split(",") if t.strip())
    if args.self_test:
        assert set(arms) <= set(ARMS) and len(set(arms)) == len(arms)
        assert set(tasks) <= set(ALL_TASKS)
        assert order_for("rust_multifile",1,arms) == order_for("rust_multifile",1,arms)
        assert order_for("rust_multifile",2,arms) == list(reversed(order_for("rust_multifile",1,arms)))
        assert "do not assume" not in SPARSE_PROMPT
        print("round12 driver self-test OK", flush=True)
        return
    if not tasks or not arms or len(set(arms)) != len(arms) or len(set(tasks)) != len(tasks):
        p.error("empty or duplicate arms/tasks")
    if not set(arms) <= set(ARMS) or not set(tasks) <= set(ALL_TASKS):
        p.error("unknown arm or task")
    if not (1 <= args.repeats <= 5 and 1 <= args.max_runs <= 36):
        p.error("invalid repeat/count")
    if args.timeout is not None and not (45 <= args.timeout <= 240):
        p.error("invalid timeout")
    if not work.BINARY.is_file():
        p.error("missing release binary")
    LAB.mkdir(parents=True, exist_ok=True)
    if args.preflight:
        preflight()
        return

    result_path = LAB / "results.json"
    task_fingerprints = {
        "rust_restore_metadata": sha(work.RESTORE_PROMPT+work.MISSING),
        "rust_multifile": sha(original_tasks.TASKS["rust_multifile"]["prompt"]),
        "rust_sparse_lookup": sha(SPARSE_PROMPT+prior.HIDDEN),
        "rust_history_unicode": sha(unicode.PROMPT+unicode.HIDDEN),
        "rust_session_unicode_preview": sha(PREVIEW_PROMPT+PREVIEW_TEST),
    }
    metadata = {
        "schema": 1, "seed": SEED,
        "model": "deepseek-v4.1-flash", "thinking": "high",
        "arms_available": list(ARMS), "task_fingerprints": task_fingerprints,
        "binary_sha256": hashlib.sha256(work.BINARY.read_bytes()).hexdigest(),
        "source_head": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
    }
    report = json.loads(result_path.read_text(encoding="utf-8")) if result_path.is_file() else {
        **metadata, "results": [],
    }
    for k, v in metadata.items():
        if report.get(k) != v:
            raise RuntimeError("data protocol changed: " + k)
    completed = {(x.get("task"), x.get("repeat"), x.get("arm")) for x in report["results"]}
    launched = 0
    # Each invocation continues the same deterministic schedule.
    for repeat in range(args.repeat_start, args.repeat_start+args.repeats):
        for task in tasks:
            for arm in order_for(task, repeat, arms):
                if (task, repeat, arm) in completed or launched >= args.max_runs:
                    continue
                print("START", task, repeat, arm, flush=True)
                cap = args.timeout or {
                    "rust_multifile": 95,
                    "rust_sparse_lookup": 120,
                    "rust_restore_metadata": 185,
                    "rust_history_unicode": 175,
                    "rust_session_unicode_preview": 185,
                }[task]
                try:
                    row = work.run_one(task, repeat, arm, cap)
                    row["cap"] = cap
                    trace_path = Path(row["logs"]) / "session.json.wal"
                    if trace_path.is_file():
                        row["post_success_audit"] = analyze(load_trace(trace_path))
                except Exception as error:
                    row = {
                        "task":task, "repeat":repeat, "arm":arm,
                        "strict_pass":False,
                        "runner_error":type(error).__name__ + ": " + str(error)[:220]
                    }
                report["results"].append(row)
                result_path.write_text(
                    json.dumps(report,ensure_ascii=False,indent=2)+"\n",
                    encoding="utf-8"
                )
                launched += 1
                m = row.get("metrics") or {}
                a = row.get("post_success_audit") or {}
                print("END", task,repeat,arm,"strict",row.get("strict_pass"),
                      "verified",row.get("verified"),"wall",row.get("wall_s"),
                      "req",m.get("requests"),"input",m.get("input"),
                      "tools",row.get("tools"),"postpass",a.get("calls_after_pass"),
                      "error",row.get("runner_error"),flush=True)
            if launched >= args.max_runs:
                break
        if launched >= args.max_runs:
            break
    print("REPORT",result_path,"rows",len(report["results"]),flush=True)


if __name__ == "__main__":
    main()

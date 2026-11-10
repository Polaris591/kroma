#!/usr/bin/env python3
"""Round 13 independent coding-task portfolio: baseline vs evidence-finish prompt.

External post-run tests remain hidden from the agent until its process exits.
One binary, one provider, one model effort, seeded crossover, separate fixtures.
No prompt variant may alter execution, commands, model/tool schema, or tests.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import re
import subprocess
import sys
import time

import agent_task_suite as suite
from round4_windows import parse_kroma
from round6_rust_trace_diagnosis import summarize_kroma

LAB = Path("D:/Kroma/experiments/kroma-round13-fresh-portfolio")
BINARY = Path("D:/Kroma/target-kroma-check/release/kroma.exe")
ARMS = ("baseline", "finish")
SEED = 20261110
NODE = "node"
TEST_RE = re.compile(r"(?:npm\s+test|node(?:\.exe)?\s+--test|cargo\s+test)\b", re.I)

TASKS = {
  "node_csv_stream": {
    "time_cap": 105,
    "prompt": (
      "Repair the ESM CSV parser in src/csv.mjs. parseCsv(text) must return rows "
      "of strings and support commas inside quoted fields, doubled quote escapes, "
      "CRLF, embedded newlines inside quoted fields, empty trailing fields, "
      "and a final record without a newline. Keep the module API unchanged. "
      "Run npm test and any necessary relevant checks. Do not modify tests, "
      "install packages, commit, or access credentials."
    ),
    "files": {
      "package.json": '{"name":"round13-csv","private":true,"type":"module","scripts":{"test":"node --test tests/basic.test.mjs"}}\n',
      "src/csv.mjs": """export function parseCsv(text) {
  if (!text) return [];
  return text.trimEnd().split(/\\r?\\n/).map(row => row.split(","));
}
""",
      "tests/basic.test.mjs": """import test from "node:test";
import assert from "node:assert/strict";
import { parseCsv } from "../src/csv.mjs";
test("plain CSV stays supported", () => {
  assert.deepEqual(parseCsv("a,b\\n1,2"), [["a","b"],["1","2"]]);
  assert.deepEqual(parseCsv(""), []);
});
""",
    },
    "holdout_name": "tests/round13_holdout.test.mjs",
    "holdout": '''import test from "node:test";
import assert from "node:assert/strict";
import { parseCsv } from "../src/csv.mjs";
test("quoted comma and escaped quotes", () => {
  assert.deepEqual(parseCsv('alpha,"b,c","say ""yes"""\\r\\n1,2,3'),
    [["alpha","b,c",'say "yes"'],["1","2","3"]]);
});
test("line breaks, empty trailing field and final record", () => {
  assert.deepEqual(parseCsv('a,"b\\nc",\\r\\nz,,z'),
    [["a","b\\nc",""],["z","","z"]]);
});
''',
  },
  "node_retry_cache": {
    "time_cap": 100,
    "prompt": (
      "Repair createLoader in src/loader.mjs. Concurrent requests for one key "
      "must share a single in-flight call; successful results must stay cached, "
      "but a failed load must not poison the cache: a later call for the same "
      "key must retry. Also keep unrelated keys independent. Preserve the "
      "public API and existing tests. Run npm test and relevant checks. "
      "Do not modify tests, install packages, commit, or access credentials."
    ),
    "files": {
      "package.json": '{"name":"round13-cache","private":true,"type":"module","scripts":{"test":"node --test tests/basic.test.mjs"}}\n',
      "src/loader.mjs": """export function createLoader(fetcher) {
  const cache = new Map();
  return async function load(key) {
    if (cache.has(key)) return cache.get(key);
    const pending = Promise.resolve().then(() => fetcher(key));
    cache.set(key, pending);
    return pending;
  };
}
""",
      "tests/basic.test.mjs": """import test from "node:test";
import assert from "node:assert/strict";
import { createLoader } from "../src/loader.mjs";
test("single successful fetch is shared and cached", async () => {
  let calls = 0;
  const load = createLoader(async (key) => { calls++; return key.toUpperCase(); });
  assert.deepEqual(await Promise.all([load("a"),load("a")]), ["A","A"]);
  assert.equal(await load("a"), "A");
  assert.equal(calls, 1);
});
""",
    },
    "holdout_name": "tests/round13_holdout.test.mjs",
    "holdout": """import test from "node:test";
import assert from "node:assert/strict";
import { createLoader } from "../src/loader.mjs";
test("rejected shared call allows a later retry", async () => {
  let calls = 0;
  const load = createLoader(async () => {
    calls++;
    if (calls === 1) throw new Error("transient");
    return "recovered";
  });
  const failed = await Promise.allSettled([load("a"),load("a")]);
  assert.equal(failed[0].status, "rejected");
  assert.equal(failed[1].status, "rejected");
  assert.equal(calls, 1);
  assert.equal(await load("a"), "recovered");
  assert.equal(await load("a"), "recovered");
  assert.equal(calls, 2);
});
test("keys are isolated", async () => {
  const calls = [];
  const load = createLoader(async key => { calls.push(key); return key; });
  assert.deepEqual(await Promise.all([load("x"),load("y")]), ["x","y"]);
  assert.deepEqual(calls, ["x","y"]);
});
""",
  },
  "rust_store_consistency": {
    "time_cap": 120,
    "prompt": (
      "Repair this small Rust crate's Store API. Updating or deleting an entry "
      "must never return a stale value from its read cache; ensure get, put "
      "and delete preserve consistency, including deleting then reinserting. "
      "Keep the public API and tests unchanged. Run cargo test --locked --offline "
      "and report checks. Do not commit or access credentials."
    ),
    "files": {
      "Cargo.toml": '[package]\nname = "round13_store"\nversion = "0.1.0"\nedition = "2021"\n',
      "Cargo.lock": '# This file is automatically @generated by Cargo.\nversion = 4\n\n[[package]]\nname = "round13_store"\nversion = "0.1.0"\n',
      "src/lib.rs": """mod store;
pub use store::Store;
""",
      "src/store.rs": """use std::collections::HashMap;

#[derive(Default)]
pub struct Store {
    data: HashMap<String, String>,
    cache: HashMap<String, Option<String>>,
}

impl Store {
    pub fn new() -> Self { Self::default() }
    pub fn get(&mut self, key: &str) -> Option<String> {
        if let Some(value) = self.cache.get(key) { return value.clone(); }
        let value = self.data.get(key).cloned();
        self.cache.insert(key.to_owned(), value.clone());
        value
    }
    pub fn put(&mut self, key: &str, value: &str) {
        self.data.insert(key.to_owned(), value.to_owned());
    }
    pub fn delete(&mut self, key: &str) -> Option<String> {
        self.data.remove(key)
    }
}
""",
      "tests/basic.rs": """use round13_store::Store;
#[test]
fn first_read_after_put() {
    let mut store = Store::new();
    store.put("key", "one");
    assert_eq!(store.get("key"), Some("one".into()));
}
""",
    },
    "holdout_name": "tests/round13_holdout.rs",
    "holdout": """use round13_store::Store;
#[test]
fn read_after_update_is_fresh() {
    let mut store = Store::new();
    store.put("key", "one");
    assert_eq!(store.get("key"), Some("one".into()));
    store.put("key", "two");
    assert_eq!(store.get("key"), Some("two".into()));
}
#[test]
fn deleting_and_reinserting_clears_positive_and_negative_cache() {
    let mut store = Store::new();
    assert_eq!(store.get("missing"), None);
    store.put("missing", "hi");
    assert_eq!(store.get("missing"), Some("hi".into()));
    assert_eq!(store.delete("missing"), Some("hi".into()));
    assert_eq!(store.get("missing"), None);
    store.put("missing", "again");
    assert_eq!(store.get("missing"), Some("again".into()));
}
""",
  },
  "rust_split_frames": {
    "time_cap": 125,
    "prompt": (
      "Repair the Rust stream framing decoder. Frames have a four-byte "
      "big-endian payload length; Decoder::push must accept arbitrary split "
      "network chunks, preserve partial headers and payloads, emit multiple "
      "complete frames in order, and support empty payloads. Keep the encode "
      "and Decoder public API unchanged. Run cargo test --locked --offline "
      "and relevant regression checks. Do not modify tests, commit or access credentials."
    ),
    "files": {
      "Cargo.toml": '[package]\nname = "round13_frames"\nversion = "0.1.0"\nedition = "2021"\n',
      "Cargo.lock": '# This file is automatically @generated by Cargo.\nversion = 4\n\n[[package]]\nname = "round13_frames"\nversion = "0.1.0"\n',
      "src/lib.rs": """mod framing;
pub use framing::{encode, Decoder};
""",
      "src/framing.rs": """pub fn encode(payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self { Self::default() }
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(chunk);
        let mut ready = Vec::new();
        while self.buffer.len() >= 4 {
            let len = u32::from_be_bytes(self.buffer[..4].try_into().unwrap()) as usize;
            if self.buffer.len() < 4 + len {
                self.buffer.clear();
                break;
            }
            ready.push(self.buffer[4..4 + len].to_vec());
            self.buffer.drain(..4 + len);
        }
        ready
    }
}
""",
      "tests/basic.rs": """use round13_frames::{encode, Decoder};
#[test]
fn complete_frame_decodes() {
    let mut d = Decoder::new();
    assert_eq!(d.push(&encode(b"ok")), vec![b"ok".to_vec()]);
}
""",
    },
    "holdout_name": "tests/round13_holdout.rs",
    "holdout": """use round13_frames::{encode, Decoder};
#[test]
fn split_header_and_payload_survive() {
    let frame = encode(b"hello");
    let mut decoder = Decoder::new();
    assert!(decoder.push(&frame[..2]).is_empty());
    assert!(decoder.push(&frame[2..5]).is_empty());
    assert_eq!(decoder.push(&frame[5..]), vec![b"hello".to_vec()]);
}
#[test]
fn several_frames_and_empty_payload_keep_order() {
    let mut decoder = Decoder::new();
    let mut bytes = encode(b"one");
    bytes.extend_from_slice(&encode(b""));
    bytes.extend_from_slice(&encode(b"three"));
    assert_eq!(decoder.push(&bytes), vec![b"one".to_vec(), b"".to_vec(), b"three".to_vec()]);
}
""",
  },
}


def fingerprint(task: dict) -> str:
    return hashlib.sha256(json.dumps({
        "prompt": task["prompt"],
        "files": task["files"],
        "holdout": task["holdout"],
    }, sort_keys=True, ensure_ascii=False).encode()).hexdigest()


def environment(project: Path, trial: Path, arm: str) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("KROMA_")}
    env.update({
        "KROMA_PROVIDER_PROFILE": "deepseek",
        "KROMA_DEEPSEEK_THINKING": "off",
        "KROMA_REASONING_EFFORT": "high",
        "KROMA_PROFILE": "1",
        "KROMA_PLUGIN_DIR": str(trial / "disabled-global-plugins" / "plugins"),
        "KROMA_WORKDIR": str(project),
        "KROMA_SESSION_FILE": str(trial / "session.json"),
        "CARGO_TARGET_DIR": "D:/Kroma/target-kroma-check",
        "CARGO_INCREMENTAL": "0",
    })
    env["KROMA_EXPERIMENT_PROMPT_ARM"] = "finish" if arm == "finish" else "baseline"
    return env


def make_fixture(project: Path, task_name: str) -> str:
    task = TASKS[task_name]
    suite.create_fixture(project, task)
    return hashlib.sha256((project / ("tests/basic.test.mjs" if task_name.startswith("node_") else "tests/basic.rs")).read_bytes()).hexdigest()


def test_command(task_name: str, hidden: bool):
    if task_name.startswith("node_"):
        paths = ["tests/basic.test.mjs"]
        if hidden:
            paths.append(TASKS[task_name]["holdout_name"])
        return ["node", "--test", *paths]
    return ["cargo", "test", "--locked", "--offline", "--quiet"]


def oracle(task_name: str, project: Path, env: dict, expected_public: str, hidden: bool) -> tuple[bool, str]:
    task = TASKS[task_name]
    public_path = "tests/basic.test.mjs" if task_name.startswith("node_") else "tests/basic.rs"
    file = project / public_path
    if not file.exists() or hashlib.sha256(file.read_bytes()).hexdigest() != expected_public:
        return False, "agent modified public tests"
    hidden_path = project / task["holdout_name"]
    if hidden:
        hidden_path.write_text(task["holdout"], encoding="utf-8")
    try:
        result = subprocess.run(
            test_command(task_name, hidden), cwd=project, env=env, capture_output=True,
            text=True, timeout=70,
        )
    except Exception as exc:
        return False, f"{type(exc).__name__}: {str(exc)[:140]}"
    return result.returncode == 0, (
        "all tests passed" if result.returncode == 0
        else ("exit " + str(result.returncode) + " " + result.stdout[-250:] +
              " " + result.stderr[-400:])[-730:]
    )


def agent_tested(wal: Path) -> bool:
    if not wal.is_file():
        return False
    for line in wal.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            msg = json.loads(line.split("\t", 1)[0]).get("message", {})
        except ValueError:
            continue
        if msg.get("role") != "assistant":
            continue
        for call in msg.get("tool_calls") or []:
            if call.get("function", {}).get("name") != "shell":
                continue
            try:
                command = json.loads(call["function"]["arguments"]).get("command", "")
            except (KeyError, ValueError):
                continue
            if TEST_RE.search(command):
                return True
    return False


def run_one(task_name: str, arm: str, repeat: int, cap: int, lab: Path) -> dict:
    trial = lab / task_name / str(repeat) / arm
    project = trial / "project"
    project.mkdir(parents=True, exist_ok=False)
    frozen = make_fixture(project, task_name)
    env = environment(project, trial, arm)
    cmd = [str(BINARY), "--print", "--no-mcp", TASKS[task_name]["prompt"]]
    start = time.monotonic()
    proc = subprocess.Popen(
        cmd, cwd=project, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0,
    )
    timed_out = False
    try:
        output, errors = proc.communicate(timeout=cap)
    except subprocess.TimeoutExpired:
        timed_out = True
        if os.name == "nt":
            subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                           capture_output=True, timeout=20)
        else:
            proc.kill()
        output, errors = proc.communicate(timeout=15)
    wall = round(time.monotonic() - start, 3)
    (trial / "stdout.log").write_bytes(output)
    (trial / "stderr.log").write_bytes(errors)
    metrics, _, tools, profile = parse_kroma(
        output.decode("utf-8", "replace"), errors.decode("utf-8", "replace"),
        trial / "session.json",
    )
    verified, detail = oracle(task_name, project, env, frozen, hidden=True)
    tests_run = agent_tested(trial / "session.json.wal")
    completed = proc.returncode == 0 and not timed_out
    return {
        "task": task_name, "arm": arm, "repeat": repeat, "cap": cap,
        "wall_s": wall, "timed_out": timed_out, "exit": proc.returncode,
        "verified": verified, "verification_detail": detail,
        "agent_tested": tests_run,
        "finished": completed,
        "strict_pass": bool(verified and tests_run and completed),
        "metrics": metrics, "profile": profile, "tools": len(tools),
        "trace": summarize_kroma(trial / "stderr.log"),
        "logs": str(trial),
    }


def order_for(task_name: str, repeat: int) -> list[str]:
    rng = random.Random(int.from_bytes(hashlib.sha256(
        f"{SEED}:{task_name}".encode()).digest()[:8]))
    order = list(ARMS)
    rng.shuffle(order)
    if repeat % 2 == 0:
        order.reverse()
    return order


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--lab", type=Path, default=LAB)
    p.add_argument("--tasks", default=",".join(TASKS))
    p.add_argument("--repeats", type=int, default=2)
    p.add_argument("--repeat-start", type=int, default=1)
    p.add_argument("--max-runs", type=int, default=2)
    p.add_argument("--timeout", type=int, default=None)
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--preflight", action="store_true")
    args = p.parse_args()
    names = [s.strip() for s in args.tasks.split(",") if s.strip()]
    if args.self_test:
        assert len(TASKS) == 4
        assert all(s in TASKS for s in names)
        assert order_for("node_retry_cache", 2) == order_for("node_retry_cache", 1)[::-1]
        assert all("install packages" in t["prompt"] or "cargo test" in t["prompt"] for t in TASKS.values())
        print("round13 driver self-test OK", flush=True)
        return
    if not names or any(t not in TASKS for t in names):
        p.error("invalid tasks")
    if args.timeout is not None and not (35 <= args.timeout <= 240):
        p.error("invalid timeout")
    if not (1 <= args.repeats <= 4 and 1 <= args.max_runs <= 32):
        p.error("invalid limits")
    lab = args.lab.resolve()
    lab.mkdir(parents=True, exist_ok=True)
    if args.preflight:
        for task_name in names:
            project = lab / "preflight" / task_name
            project.mkdir(parents=True, exist_ok=False)
            expected = make_fixture(project, task_name)
            env = environment(project, project.parent, "baseline")
            public_green, _ = oracle(task_name, project, env, expected, hidden=False)
            hidden_green, detail = oracle(task_name, project, env, expected, hidden=True)
            if not public_green or hidden_green:
                raise RuntimeError(f"preflight defective {task_name}: public={public_green} holdout={hidden_green}: {detail}")
            print("PREFLIGHT", task_name, "public_pass", public_green,
                  "holdout_pass", hidden_green, flush=True)
        return
    if not BINARY.is_file():
        p.error("candidate Kroma release not available")
    result_file = lab / "results.json"
    metadata = {
        "schema": 1, "seed": SEED, "arms": list(ARMS), "tasks": {
            k: fingerprint(task) for k, task in TASKS.items()
        }, "binary_sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        "model": "deepseek-v4.1-flash", "thinking": "high",
    }
    report = json.loads(result_file.read_text(encoding="utf-8")) if result_file.exists() else {
        **metadata, "results": [],
    }
    for key, value in metadata.items():
        if report.get(key) != value:
            raise RuntimeError("benchmark metadata drift: " + key)
    completed = {(r.get("task"), r.get("repeat"), r.get("arm")) for r in report["results"]}
    attempts = 0
    for repeat in range(args.repeat_start, args.repeat_start + args.repeats):
        for task_name in names:
            for arm in order_for(task_name, repeat):
                if (task_name, repeat, arm) in completed or attempts >= args.max_runs:
                    continue
                cap = args.timeout or TASKS[task_name]["time_cap"]
                print("START", repeat, task_name, arm, flush=True)
                try:
                    row = run_one(task_name, arm, repeat, cap, lab)
                except Exception as exc:
                    row = {
                        "repeat": repeat, "task": task_name, "arm": arm,
                        "strict_pass": False,
                        "runner_error": f"{type(exc).__name__}: {str(exc)[:180]}",
                    }
                report["results"].append(row)
                result_file.write_text(json.dumps(
                    report, ensure_ascii=False, indent=2, default=str
                )+"\n", encoding="utf-8")
                attempts += 1
                metrics = row.get("metrics") or {}
                print("END", repeat, task_name, arm, "pass", row.get("strict_pass"),
                      "verified", row.get("verified"), "tested", row.get("agent_tested"),
                      "wall", row.get("wall_s"), "requests", metrics.get("requests"),
                      "tools", row.get("tools"),
                      "error", row.get("runner_error"), flush=True)
            if attempts >= args.max_runs:
                break
        if attempts >= args.max_runs:
            break
    print("REPORT", result_file, "rows", len(report["results"]), flush=True)


if __name__ == "__main__":
    main()

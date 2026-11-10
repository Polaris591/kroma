#!/usr/bin/env python3
"""p-limit upstream historical issues #108 and #109.

Reuses the Round14 historical-pr benchmark driver, with exact upstream
index.js/index.d.ts/package.json base blobs and a vendored upstream yocto-
queue@1.2.1 dependency from its actual GitHub release tag. No npm installs.
"""
from __future__ import annotations
from pathlib import Path
import hashlib
import round14_real_pr_cases as runner

ROOT=Path(__file__).resolve().parents[1]
runner.LAB=Path("D:/Kroma/experiments/kroma-round14-real-plimit-v2")

TASKS={
    "p-limit-108":{
        "repo":"sindresorhus/p-limit","pr":108,
        "source":ROOT/"benchmarks/round14_fixture_p-limit-108.json",
        "cap":115,
        "prompt":(
            "Fix a real p-limit library regression: the map method on a "
            "limiter fails when detached as const {map} = limit, because it "
            "depends on call-time receiver binding. A detached map must share "
            "the same concurrency limit as direct limiter calls, preserve "
            "mapper index/order, and work with ordinary iterables. Modify "
            "only necessary implementation code, preserve API, run "
            "node --test tests/public.test.mjs and relevant verification. "
            "Do not edit tests, install packages, commit or access credentials."
        ),
        "public":'''import test from "node:test";
import assert from "node:assert/strict";
import pLimit from "../index.js";
test("regular limit map works", async () => {
  const limit=pLimit(2);
  assert.deepEqual(await limit.map([2,3,4],async (n,i)=>n+i),[2,4,6]);
});
test("direct limit caps concurrency",async () => {
  const limit=pLimit(1);
  assert.deepEqual(await Promise.all([limit(async()=>1),limit(async()=>2)]),[1,2]);
});
''',
        "holdout":'''import test from "node:test";
import assert from "node:assert/strict";
import pLimit from "../index.js";
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
test("detached map uses configured limiter concurrently with direct work",async () => {
  const limit=pLimit(1);
  const {map}=limit;
  let running=0,max=0;
  const work=async (n,i) => {
    running++;
    max=Math.max(max,running);
    await sleep(8);
    running--;
    return n+(i??0);
  };
  const a=limit(work,1);
  const b=map([2,3],work);
  assert.deepEqual(await Promise.all([a,b]),[1,[2,4]]);
  assert.equal(max,1);
});
test("detached map handles Set iterable and source indices",async () => {
  const limit=pLimit(2);const {map}=limit;
  assert.deepEqual(await map(new Set(["a","b","c"]),async(v,i)=>[v,i]),
    [["a",0],["b",1],["c",2]]);
});
''',
    },
    "p-limit-109":{
        "repo":"sindresorhus/p-limit","pr":109,
        "source":ROOT/"benchmarks/round14_fixture_p-limit-109.json",
        "cap":115,
        "prompt":(
            "Fix a real p-limit regression: limitFunction accepts the "
            "rejectOnClear configuration but its returned limited function "
            "does not expose clearQueue, so callers cannot reject queued "
            "work. The limited function should expose the existing limiter's "
            "clearQueue behavior without changing task concurrency or the "
            "functional calling API. Keep the TypeScript declaration "
            "consistent with the new method. Run "
            "node --test tests/public.test.mjs and relevant checks. "
            "Do not edit tests, install packages, commit, or access credentials."
        ),
        "public":'''import test from "node:test";
import assert from "node:assert/strict";
import pLimit, {limitFunction} from "../index.js";
test("regular limiter preserves results",async () => {
  const limit=pLimit(2);
  assert.deepEqual(await Promise.all([limit(async()=>1),limit(async()=>2)]),[1,2]);
});
test("limitFunction regular calls work",async () => {
  const task=limitFunction(async x=>x*2,{concurrency:1});
  assert.deepEqual(await Promise.all([task(2),task(3)]),[4,6]);
});
''',
        "holdout":'''import test from "node:test";
import assert from "node:assert/strict";
import {limitFunction} from "../index.js";
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
test("limitFunction exposes and uses clearQueue rejection", async () => {
  const run=limitFunction(async () => {await sleep(20);return 1;},
    {concurrency:1,rejectOnClear:true});
  const running=run();
  const pending=run();
  assert.equal(typeof run.clearQueue,"function");
  const rejection=assert.rejects(pending,{name:"AbortError"});
  run.clearQueue();
  assert.equal(await running,1);
  await rejection;
});
''',
    },
}
runner.TASKS.clear()
runner.TASKS.update(TASKS)
_original_make=runner.make_fixture

def make_fixture(project,task_name):
    fixed=_original_make(project,task_name)
    dependency=project/"node_modules"/"yocto-queue"
    dependency.mkdir(parents=True,exist_ok=True)
    for file,asset in [
        ("index.js",ROOT/"benchmarks/round14_fixture_yocto_index.txt"),
        ("package.json",ROOT/"benchmarks/round14_fixture_yocto_package.txt"),
    ]:
        (dependency/file).write_text(asset.read_text(encoding="utf-8"),encoding="utf-8",newline="\n")
    return fixed

runner.make_fixture=make_fixture

if __name__=="__main__":
    runner.main()

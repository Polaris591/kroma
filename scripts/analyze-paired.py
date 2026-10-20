#!/usr/bin/env python3
"""Paired analysis for a `kroma bench` report, with the preregistered rules applied.

The blueprint's rules that decide outcomes here, not in prose:

* the task is the sampling unit, so resampling is over tasks (clusters), never over rows;
* a difference whose interval includes zero is `inconclusive`, not "no difference";
* non-inferiority needs a preregistered delta and an interval lower bound above -delta;
* a ratio with a zero denominator is `undefined`, never 0;
* cost is compared only after quality is shown non-inferior.

Usage:
    python3 analyze-paired.py --bench report.json --baseline single_agent --arm adaptive_tournament_3 \
        --delta-pp 5 --resamples 2000 [--json]

Exit codes: 0 analysis produced, 2 unusable input (missing arms / no tasks).
"""

import argparse
import json
import random
import sys


def cluster_deltas(results, baseline, arm):
    """Per-task paired difference: arm minus baseline, on resolved (0/1) and tokens."""
    by_task = {}
    for row in results:
        task = row["task_id"]
        strategy = row["strategy"]
        bucket = by_task.setdefault(strategy, {}).setdefault(task, {"resolved": [], "tokens": []})
        bucket["resolved"].append(1 if row["resolved"] else 0)
        bucket["tokens"].append(row.get("total_tokens") or 0)

    tasks = sorted(set(by_task.get(baseline, {})) & set(by_task.get(arm, {})))
    if not tasks:
        return None

    deltas = []
    for task in tasks:
        base = by_task[baseline][task]
        cand = by_task[arm][task]
        base_rate = sum(base["resolved"]) / len(base["resolved"])
        cand_rate = sum(cand["resolved"]) / len(cand["resolved"])
        base_tokens = sum(base["tokens"]) / len(base["tokens"])
        cand_tokens = sum(cand["tokens"]) / len(cand["tokens"])
        deltas.append(
            {
                "task": task,
                "quality_pp": (cand_rate - base_rate) * 100.0,
                "token_delta": cand_tokens - base_tokens,
                "token_ratio": (cand_tokens / base_tokens) if base_tokens else None,
            }
        )
    return deltas


def bootstrap(deltas, field, resamples, seed=20261019):
    """Percentile interval over a cluster bootstrap of the per-task deltas."""
    values = [row[field] for row in deltas if row.get(field) is not None]
    if not values:
        return None
    rng = random.Random(seed)
    means = []
    for _ in range(resamples):
        sample = [values[rng.randrange(len(values))] for _ in values]
        means.append(sum(sample) / len(sample))
    means.sort()
    low = means[int(0.025 * (len(means) - 1))]
    high = means[int(0.975 * (len(means) - 1))]
    return {"mean": sum(values) / len(values), "ci_low": low, "ci_high": high, "n": len(values)}


def verdict(quality, delta_pp):
    """Apply the preregistered rule to a quality interval."""
    if quality is None:
        return "inconclusive"
    low, high = quality["ci_low"], quality["ci_high"]
    if low > 0:
        return "improved"
    if low > -delta_pp and high < delta_pp:
        return "non_inferior"
    if high < -delta_pp:
        return "regressed"
    return "inconclusive"


def undefined_ratios(deltas):
    return [row["task"] for row in deltas if row.get("token_ratio") is None]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bench", required=True)
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--arm", required=True)
    parser.add_argument("--delta-pp", type=float, default=5.0)
    parser.add_argument("--resamples", type=int, default=2000)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    report = json.loads(open(args.bench, encoding="utf-8").read())
    deltas = cluster_deltas(report.get("results", []), args.baseline, args.arm)
    if deltas is None:
        print(f"no tasks shared by {args.baseline} and {args.arm}", file=sys.stderr)
        return 2

    quality = bootstrap(deltas, "quality_pp", args.resamples)
    tokens = bootstrap(deltas, "token_delta", args.resamples)
    ratios = [row["token_ratio"] for row in deltas if row["token_ratio"] is not None]
    token_ratio = (sum(ratios) / len(ratios)) if ratios else None
    outcome = verdict(quality, args.delta_pp)

    result = {
        "baseline": args.baseline,
        "arm": args.arm,
        "tasks": len(deltas),
        "delta_pp_preregistered": args.delta_pp,
        "quality_pp": quality,
        "tokens": tokens,
        "token_ratio": token_ratio,
        "token_ratio_undefined_for": undefined_ratios(deltas),
        "verdict": outcome,
        # Cost is only interpretable when quality did not get worse.
        "cost_claim_allowed": outcome in ("improved", "non_inferior"),
        "rule": "improved requires CI low > 0; non_inferior requires CI inside (-delta, +delta); "
        "zero denominator is undefined, not zero; cost is only claimed after quality holds",
    }
    print(json.dumps(result, ensure_ascii=False, indent=2) if args.json else
          f"{outcome}: quality {quality} tokens {tokens} ratio {token_ratio} on {len(deltas)} tasks")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

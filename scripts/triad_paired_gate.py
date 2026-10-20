#!/usr/bin/env python3
"""Fail-closed, offline cache/tools/compaction paired A/B gate.

Consumes an operator-produced *measured* report, not synthetic responses. Task is the
bootstrap cluster. Prices are estimates from a versioned operator table, NOT invoices.
This script does not run models or attest to the authenticity of supplied measurements.

Report: {"experiment_id":"...", "rows":[{ "task_id":"...", "repetition":0,
 "arm":"baseline|candidate", "resolved":true, "reported_model":"...",
 "usage_basis":"provider_reported", "input_tokens":1000,
 "cached_input_tokens":600, "cache_write_tokens":0, "output_tokens":40,
 "prompt_bytes":2000, "tool_schema_bytes":1000, "compacted_evidence_bytes":0,
 "wall_ms":1000 }]}
Prices: {"version":"...", "source":"...", "models":{"model-id":{
 "uncached_input_nano_usd":100, "cached_input_nano_usd":10,
 "cache_creation_input_nano_usd":125, "output_nano_usd":400}}}

Unknown cache write/read split is never priced using guessed zeros. No dollars claimed
until quality is noninferior on task-cluster CI, cost has complete usage, and cost CI
actually improves. The minimal sample size is intentionally conservative.
"""
import argparse
import json
import math
import random
import sys
from collections import defaultdict
from pathlib import Path


class GateError(ValueError):
    pass


def need(ok, reason):
    if not ok:
        raise GateError(reason)


def number(value, name):
    need(type(value) is int and value >= 0 and value <= 2**53 - 1, f'{name} must be a nonnegative safe integer')
    return value


def estimate(row, prices):
    if row.get('usage_basis') != 'provider_reported':
        return None, 'usage is not provider reported'
    model = row.get('reported_model')
    rates = prices['models'].get(model) if isinstance(model, str) else None
    if rates is None:
        return None, 'reported model has no exact rate'
    input_n = number(row.get('input_tokens'), 'input_tokens')
    output_n = number(row.get('output_tokens'), 'output_tokens')
    cached = row.get('cached_input_tokens')
    writes = row.get('cache_write_tokens')
    if cached is None and rates['cached_input_nano_usd'] != rates['uncached_input_nano_usd']:
        return None, 'cached input split unknown'
    if writes is None and rates.get('cache_creation_input_nano_usd') is not None:
        return None, 'cache write split unknown'
    cached = 0 if cached is None else number(cached, 'cached_input_tokens')
    writes = 0 if writes is None else number(writes, 'cache_write_tokens')
    need(cached + writes <= input_n, 'cached + written input exceeds input_tokens')
    if writes and rates.get('cache_creation_input_nano_usd') is None:
        return None, 'cache write pricing unknown'
    cost = ((input_n - cached - writes) * rates['uncached_input_nano_usd']
            + cached * rates['cached_input_nano_usd']
            + writes * rates.get('cache_creation_input_nano_usd', 0)
            + output_n * rates['output_nano_usd'])
    return cost, None


def ci(values, resamples, seed):
    rng = random.Random(seed)
    means = sorted(sum(values[rng.randrange(len(values))] for _ in values) / len(values)
                   for _ in range(resamples))
    return {'mean': sum(values) / len(values),
            'low': means[int(0.025 * (len(means) - 1))],
            'high': means[int(0.975 * (len(means) - 1))]}


def analyze(report, prices, *, min_tasks=20, delta_pp=5, resamples=2000, seed=20261020):
    need(isinstance(report, dict) and isinstance(report.get('experiment_id'), str)
         and bool(report['experiment_id'].strip()), 'nonempty experiment_id required')
    need(isinstance(prices, dict) and isinstance(prices.get('version'), str)
         and bool(prices['version'].strip()) and isinstance(prices.get('source'), str)
         and bool(prices['source'].strip()) and isinstance(prices.get('models'), dict),
         'versioned operator prices required')
    for model, rates in prices['models'].items():
        need(bool(model) and isinstance(rates, dict), 'invalid model rate table')
        for k in ['uncached_input_nano_usd', 'cached_input_nano_usd', 'output_nano_usd']:
            number(rates.get(k), f'rates.{model}.{k}')
        if rates.get('cache_creation_input_nano_usd') is not None:
            number(rates['cache_creation_input_nano_usd'], 'cache write rate')
    need(type(min_tasks) is int and min_tasks >= 2, 'min_tasks >= 2 required')
    need(math.isfinite(delta_pp) and 0 <= delta_pp < 100, 'invalid noninferiority margin')
    need(type(resamples) is int and resamples >= 200, 'at least 200 bootstrap resamples')
    rows = report.get('rows')
    need(isinstance(rows, list) and bool(rows), 'nonempty rows required')
    indexed = {}
    reps_by_task = defaultdict(lambda: {'baseline': set(), 'candidate': set()})
    unknown = []
    for i, row in enumerate(rows):
        need(isinstance(row, dict), f'row {i} must be an object')
        task, arm, rep = row.get('task_id'), row.get('arm'), row.get('repetition')
        need(isinstance(task, str) and task and len(task) <= 256, f'row {i} bad task_id')
        need(arm in ('baseline', 'candidate'), f'row {i} invalid arm')
        number(rep, 'repetition')
        need(type(row.get('resolved')) is bool, f'row {i} missing boolean resolved')
        for field in ['prompt_bytes', 'tool_schema_bytes', 'compacted_evidence_bytes', 'wall_ms']:
            number(row.get(field), f'row {i}.{field}')
        key = (task, rep, arm)
        need(key not in indexed, f'duplicate task/repetition/arm: {key}')
        cost, why = estimate(row, prices)
        indexed[key] = (row, cost)
        reps_by_task[task][arm].add(rep)
        if why:
            unknown.append({'task_id': task, 'repetition': rep, 'arm': arm, 'reason': why})
    need(all(reps['baseline'] == reps['candidate'] and bool(reps['baseline'])
             for reps in reps_by_task.values()), 'unpaired repetitions: baseline and candidate must exactly match')
    tasks = sorted(reps_by_task)
    task_quality, task_cost, task_cache, task_prompt, task_tool_schema, task_compacted = [], [], [], [], [], []
    for task in tasks:
        qualities, costs, cache_delta, prompt_delta, schema_delta, compact_delta = [], [], [], [], [], []
        for rep in sorted(reps_by_task[task]['baseline']):
            base, bc = indexed[(task, rep, 'baseline')]
            cand, cc = indexed[(task, rep, 'candidate')]
            need(base.get('reported_model') == cand.get('reported_model'),
                 f'{task} repetition {rep} has different provider models; not a same-model A/B')
            qualities.append((int(cand['resolved']) - int(base['resolved'])) * 100)
            if bc is not None and cc is not None:
                costs.append(cc - bc)
            if base.get('cached_input_tokens') is not None and cand.get('cached_input_tokens') is not None:
                cache_delta.append(cand['cached_input_tokens'] - base['cached_input_tokens'])
            prompt_delta.append(cand['prompt_bytes'] - base['prompt_bytes'])
            schema_delta.append(cand['tool_schema_bytes'] - base['tool_schema_bytes'])
            compact_delta.append(cand['compacted_evidence_bytes'] - base['compacted_evidence_bytes'])
        task_quality.append(sum(qualities) / len(qualities))
        if len(costs) == len(qualities):
            task_cost.append(sum(costs) / len(costs))
        if len(cache_delta) == len(qualities):
            task_cache.append(sum(cache_delta) / len(cache_delta))
        task_prompt.append(sum(prompt_delta) / len(prompt_delta))
        task_tool_schema.append(sum(schema_delta) / len(schema_delta))
        task_compacted.append(sum(compact_delta) / len(compact_delta))
    quality_ci = ci(task_quality, resamples, seed)
    quality_gate = ('insufficient_tasks' if len(tasks) < min_tasks else
                    'non_inferior' if quality_ci['low'] > -delta_pp else
                    'regressed' if quality_ci['high'] < -delta_pp else 'inconclusive')
    cost_ci = ci(task_cost, resamples, seed + 1) if len(task_cost) == len(tasks) else None
    if quality_gate != 'non_inferior':
        outcome = 'no_cost_saving_claim_quality_not_established'
    elif cost_ci is None:
        outcome = 'no_cost_saving_claim_unknown_billing'
    elif cost_ci['high'] >= 0:
        outcome = 'no_cost_saving_claim_not_statistically_cheaper'
    else:
        outcome = 'candidate_estimated_cheaper_with_quality_noninferior'
    return {
        'experiment_id': report['experiment_id'], 'tasks': len(tasks),
        'observations': len(rows), 'operator_price_version': prices['version'],
        'quality_margin_pp': delta_pp, 'quality_delta_pp': quality_ci,
        'quality_gate': quality_gate, 'estimated_cost_delta_nano_usd': cost_ci,
        'unknown_billing_count': len(unknown), 'unknown_billing_examples': unknown[:10],
        'cache_read_token_delta_mean': sum(task_cache) / len(task_cache) if len(task_cache) == len(tasks) else None,
        'prompt_bytes_delta_mean': sum(task_prompt) / len(tasks),
        'tool_schema_bytes_delta_mean': sum(task_tool_schema) / len(tasks),
        'compacted_evidence_bytes_delta_mean': sum(task_compacted) / len(tasks),
        'outcome': outcome, 'cost_saving_claim_allowed': outcome == 'candidate_estimated_cheaper_with_quality_noninferior',
        'disclaimer': 'offline task-cluster bootstrap and operator-rate estimate; not a provider invoice or proof that rows came from live calls',
    }


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--report', type=Path, required=True)
    p.add_argument('--prices', type=Path, required=True)
    p.add_argument('--min-tasks', type=int, default=20)
    p.add_argument('--noninferiority-pp', type=float, default=5)
    p.add_argument('--resamples', type=int, default=2000)
    args = p.parse_args(argv)
    try:
        report = json.loads(args.report.read_text('utf8'))
        prices = json.loads(args.prices.read_text('utf8'))
        result = analyze(report, prices, min_tasks=args.min_tasks,
                         delta_pp=args.noninferiority_pp, resamples=args.resamples)
    except (GateError, OSError, ValueError, TypeError, KeyError) as e:
        print(f'FAIL-CLOSED: {e}', file=sys.stderr)
        return 2
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 0


if __name__ == '__main__':
    sys.exit(main())

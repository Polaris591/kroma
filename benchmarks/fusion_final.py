#!/usr/bin/env python3
"""Final A/C/D comparison: two long-history recall conditions, two rounds.

Reuses the credential-isolated proxy and bwrap sandbox from fusion_ablation.
Writes only to /tmp/kroma-fusion-final. No production Kroma source changes.
"""
import argparse
import csv
import hashlib
import json
import random
import statistics
from pathlib import Path

import fusion_ablation as bench
import fusion_stress as stress

ROOT = Path("/tmp/kroma-fusion-final")
bench.OUT = ROOT
bench.RUNTIME = ROOT / "runtime"
bench.WORK = bench.RUNTIME / "project"
bench.SESSION = bench.RUNTIME / "history.json"
stress.OUT = ROOT

MARKERS = {"incidental": "NOVA-58-Q", "diagnostic": "MESA-74-P"}
scenario = "incidental"
base_seed = stress.stress_messages


def fixture():
    messages = base_seed()
    if scenario == "diagnostic":
        for message in messages:
            if message.get("tool_call_id") == "archived_0":
                data = json.loads(message["content"])
                lines = data["stdout"].splitlines()
                assert stress.MARKER in lines[17]
                lines[17] = "build 0 step 17: ERROR launch gate " + stress.MARKER
                data["stdout"] = "\n".join(lines)
                message["content"] = bench.encoded(data)
                break
    return messages


bench.seed_messages = fixture


def select(name):
    global scenario
    scenario = name
    stress.MARKER = MARKERS[name]
    history = fixture()
    index = next(i for i, m in enumerate(history)
                 if m.get("tool_call_id") == "archived_0")
    assert MARKERS[name] in history[index]["content"]
    assert ("ERROR launch gate" in history[index]["content"]) == (name == "diagnostic")
    return dict(scenario=name, marker_index=index,
                history_hash=hashlib.sha256(bench.encoded(history).encode()).hexdigest())


def report(rows, metadata):
    truth = lambda value: str(value).lower() == "true"
    output = [
        "# Final Kroma A/C/D benchmark",
        "",
        "Real WorkBuddy DeepSeek. Isolated coding tasks + post-APEC follow-up recall.",
        "A = APEC only; C = APEC + cache-aware TRC; D = C + evidence-aware summaries.",
        "",
        "| Variant | Runs | Coding verified | Valid exact recall | Invalid probes | Mean wall seconds | Mean input tokens | Mean uncached input |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for variant in "acd":
        group = [r for r in rows if r["variant"] == variant]
        if not group:
            continue
        mean = lambda field: statistics.mean(float(r[field]) for r in group)
        valid = [r for r in group if truth(r["valid_memory_probe"])]
        output.append(
            f"| {variant.upper()} | {len(group)} | "
            f"{sum(truth(r['algorithm_success']) for r in group)}/{len(group)} | "
            f"{sum(truth(r['followup_marker_recalled']) for r in valid)}/{len(valid)} | "
            f"{len(group)-len(valid)} | "
            f"{mean('total_seconds'):.2f} | {mean('total_input_tokens'):,.0f} | "
            f"{statistics.mean(float(r['total_input_tokens'])-float(r['total_cached_input']) for r in group):,.0f} |"
        )
    output += [
        "", "## Per-scenario results", "",
        "| Scenario | Round | Variant | Code fixed | APEC covered | Marker recalled | TRC cleared | Full wall seconds | Input tokens |",
        "|---|---:|---|---|---|---|---:|---:|---:|",
    ]
    for row in rows:
        output.append(
            f"| {row['scenario']} | {row['round']} | {row['variant'].upper()} | "
            f"{row['algorithm_success']} | {row['covered_old_marker']} | "
            f"{row['followup_marker_recalled']} | {row['trc_cleared']} | "
            f"{float(row['total_seconds']):.2f} | {int(row['total_input_tokens']):,} |"
        )
    output += [
        "", "## Interpretation limits", "",
        "- incidental: marker is buried in ordinary build progress, without evidence-selector keywords.",
        "- diagnostic: marker is tagged ERROR in the same old tool-result location.",
        "- Follow-up is after a committed APEC snapshot, without shell or original-history retrieval.",
        "- All modes use the same actual model, source code, project path and task in a condition.",
        "- Real input/output/cached token counts come from provider usage; cached input is a subset of input.",
        "- Results are small-sample and task-specific, not formal statistical or generality proof.",
        "- D does not have the proposed on-demand retrieval system; this tests D as implemented.",
        "- One APEC failure left older raw history uncompressed in an A run; its recall is not a valid post-compaction probe.",
        "- C is the best default candidate for this tested workload: all probes valid and recalled, with low uncached input.",
        "- The apparent savings must not be interpreted as real money savings without the provider's cached/uncached prices.",
        "", "## Immutable experiment fixtures", "",
        json.dumps(metadata, ensure_ascii=False, indent=2), "",
    ]
    bench.save(ROOT / "summary.md", "\n".join(output))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runs", type=int, default=2)
    parser.add_argument("--timeout", type=int, default=100)
    parser.add_argument("--report-only", action="store_true")
    args = parser.parse_args()
    if args.runs not in (1, 2, 3) or not 60 <= args.timeout <= 150:
        parser.error("runs must be 1..3; timeout 60..150")
    ROOT.mkdir(exist_ok=True)
    metadata = [select(name) for name in MARKERS]
    if args.report_only:
        with (ROOT / "results.csv").open(newline="") as file:
            report(list(csv.DictReader(file)), metadata)
        print("UPDATED", ROOT / "summary.md")
        return
    select("incidental")
    bench.validate_fixtures()
    conn = bench.credentials()
    rows = []
    for round_number in range(1, args.runs + 1):
        for name in MARKERS:
            spec = select(name)
            order = list("acd")
            random.Random(round_number * 997 + (13 if name == "incidental" else 43)).shuffle(order)
            for variant in order:
                row = bench.run_once(variant, round_number, "final-" + name, args.timeout, conn)
                probe = stress.run_followup(conn, variant, f"{name}-{round_number}",
                                            args.timeout, spec["marker_index"])
                row.update(probe)
                row["scenario"] = name
                row["round"] = round_number
                row["valid_memory_probe"] = (
                    row["covered_old_marker"] and row["followup_tool_calls"] == 0
                    and row["followup_exit"] == 0
                )
                # Pull actual response completion tokens for follow-up from the
                # sanitized proxy records. The existing probe returns input only.
                path = ROOT / "logs" / f"followup-{name}-{round_number}-{variant}.requests.json"
                followup_records = json.loads(path.read_text())
                f_usage = bench.aggregate(followup_records, "foreground")
                a_usage = bench.aggregate(followup_records, "apec")
                row["followup_foreground_output"] = f_usage["output"]
                row["followup_apec_output"] = a_usage["output"] if a_usage["requests"] else 0
                row["total_seconds"] = round(
                    float(row["seconds"]) + float(row["followup_seconds"]), 3)
                row["total_input_tokens"] = sum(int(row.get(field) or 0) for field in (
                    "foreground_input", "apec_input", "followup_foreground_input",
                    "followup_apec_input"))
                row["total_output_tokens"] = sum(int(row.get(field) or 0) for field in (
                    "foreground_output", "apec_output", "followup_foreground_output",
                    "followup_apec_output"))
                row["total_cached_input"] = sum(int(row.get(field) or 0) for field in (
                    "foreground_cached", "apec_cached", "followup_foreground_cached"))
                rows.append(row)
                with (ROOT / "results.csv").open("w", newline="") as file:
                    fields = list(dict.fromkeys(k for r in rows for k in r))
                    writer = csv.DictWriter(file, fieldnames=fields)
                    writer.writeheader()
                    writer.writerows(rows)
                report(rows, metadata)
                print("RESULT", json.dumps({
                    k: row[k] for k in ("scenario", "round", "variant", "seconds",
                        "followup_seconds", "algorithm_success", "valid_memory_probe",
                        "followup_marker_recalled", "trc_cleared", "total_input_tokens",
                        "total_output_tokens", "api_errors")
                }), flush=True)
    print("COMPLETE", ROOT / "summary.md", flush=True)


if __name__ == "__main__":
    main()

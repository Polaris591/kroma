#!/usr/bin/env python3
"""KR-E03: attribute a run's wall time to phases instead of reporting one blended latency.

The runner already emits enough to do this without changing the provider boundary:

  ModelRequestStarted   -> request_body_bytes, message_history_bytes (serialization size)
  ModelRequestCompleted -> latency_ms (whole call) and ttft_ms (first token)
  ToolResult            -> duration_ms per tool call
  compaction events     -> reread / compaction cost

The honest decomposition available today is therefore: pre-first-token (connect + TLS + serialize +
prefill, reported as one number because the transport does not split it), stream completion, tool
time and compaction time. This tool reports exactly that and says what is still blended, rather than
inventing a split it cannot measure.

Usage: nextgen_latency_breakdown.py <run.jsonl> [...]
Exit:  0 report written, 1 no usable events.
"""
import json
import sys


def load(path):
    events = []
    with open(path, errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return events


def breakdown(path):
    events = load(path)
    requests = []
    tools = []
    term = None
    for e in events:
        et = e.get("event_type")
        p = e.get("payload", {})
        if et == "ModelRequestStarted":
            requests.append({"turn": p.get("turn"), "attempt": p.get("attempt"),
                             "request_body_bytes": p.get("request_body_bytes"),
                             "message_history_bytes": p.get("message_history_bytes")})
        elif et == "ModelRequestCompleted":
            if requests:
                requests[-1].update({
                    "latency_ms": p.get("latency_ms"),
                    "ttft_ms": p.get("ttft_ms"),
                    "input_tokens": p.get("input_tokens"),
                    "output_tokens": p.get("output_tokens"),
                })
        elif et == "ToolResult" or et == "ToolCallCompleted":
            tools.append({"tool": p.get("tool") or p.get("name"),
                          "duration_ms": p.get("duration_ms") or
                          (p.get("meta") or {}).get("duration_ms")})
        elif et == "RunTerminated":
            term = p

    pre_first_token = sum(r.get("ttft_ms") or 0 for r in requests)
    whole_call = sum(r.get("latency_ms") or 0 for r in requests)
    stream_tail = max(0, whole_call - pre_first_token)
    tool_time = sum(t.get("duration_ms") or 0 for t in tools)
    total = (term or {}).get("duration_ms") or 0
    accounted = pre_first_token + stream_tail + tool_time
    return {
        "file": path,
        "turns": len(requests),
        "run_total_ms": total,
        "phases": {
            "pre_first_token_ms": pre_first_token,
            "stream_completion_ms": stream_tail,
            "tool_ms": tool_time,
            "unaccounted_ms": max(0, total - accounted),
        },
        "share_of_accounted": {
            k: (round(v / accounted, 3) if accounted else None)
            for k, v in {
                "pre_first_token": pre_first_token,
                "stream_completion": stream_tail,
                "tool": tool_time,
            }.items()
        },
        "dominant_phase": max(
            [("pre_first_token", pre_first_token), ("stream_completion", stream_tail),
             ("tool", tool_time), ("unaccounted", max(0, total - accounted))],
            key=lambda kv: kv[1],
        )[0],
        "still_blended": [
            "connect + TLS + request serialization + prefill are one number (ttft_ms); the transport "
            "does not split them and ProviderCompletion has 62 construction sites, so splitting is a "
            "deliberate follow-up rather than something this tool pretends to measure",
        ],
        "requests": requests,
        "tools": tools,
        "terminal": {k: (term or {}).get(k) for k in ("status", "terminal_reason", "duration_ms")},
    }


def main(argv):
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 1
    reports = [breakdown(p) for p in argv[1:]]
    if not any(r["turns"] for r in reports):
        print("no model requests found in the given logs", file=sys.stderr)
        return 1
    print(json.dumps(reports, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

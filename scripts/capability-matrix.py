#!/usr/bin/env python3
"""M4-03: render the capability support matrix from the manifest, not from intentions.

The manifest (`docs/capabilities.json`) records five booleans per capability. The blueprint asks for
six orthogonal axes, so this script derives the two the manifest lacks and refuses to invent them:

* `platform_support` is `unverified` for every capability, because nothing in this tree tests a
  capability on a second platform.
* `authorization` is `default_on` only when the manifest says so; otherwise `requires_decision`.

Anything the manifest does not prove is printed as `unmeasured`, never as `supported`.
"""
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "docs" / "capabilities.json"


def evidence(record):
    if record.get("live_tested"):
        return "live_tested"
    if record.get("contract_tested"):
        return "contract_tested"
    if record.get("wired"):
        return "wired"
    if record.get("implemented"):
        return "implemented"
    return "absent"


def main():
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    rows = []
    for record in manifest["capabilities"]:
        rows.append(
            {
                "id": record["id"],
                "delivery": "implemented" if record.get("implemented") else "absent",
                "platform_support": "unverified",
                "authorization": "default_on" if record.get("default_on") else "requires_decision",
                "lifecycle": "not_managed",
                "evidence": evidence(record),
                "default": "on" if record.get("default_on") else "off_or_conditional",
                "wired": ",".join(record.get("wired") or []) or "none",
            }
        )

    lines = [
        "# 能力支持矩阵（M4-03，由 docs/capabilities.json 生成）",
        "",
        "由 `scripts/capability-matrix.py` 生成，不手工维护。六轴含义见蓝图 §5.3。",
        "",
        "| capability | delivery | platform | authorization | lifecycle | evidence | default | wired |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for row in rows:
        lines.append(
            "| {id} | {delivery} | {platform_support} | {authorization} | {lifecycle} | "
            "{evidence} | {default} | {wired} |".format(**row)
        )
    lines += [
        "",
        "## 读法与边界",
        "",
        f"- 共 {len(rows)} 项，其中 default_on "
        f"{sum(1 for row in rows if row['default'] == 'on')} 项、contract_tested "
        f"{sum(1 for row in rows if row['evidence'] in ('contract_tested', 'live_tested'))} 项。",
        "- `platform_support` 全部是 `unverified`：本树没有任何第二平台的能力级验证。",
        "- `lifecycle` 全部是 `not_managed`：激活租约（C00-04/05）尚未接进运行路径。",
        "- `live_tested` 全部为 false：没有真实 Provider 或真实沙箱的能力级实测。",
        "- 发布成熟度**不**由本矩阵给出；正式发布需要所有者单独授权（本目标边界内不做）。",
        "",
    ]
    out = ROOT / "docs" / "CAPABILITY_MATRIX.md"
    out.write_text("\n".join(lines), encoding="utf-8")
    print(out)
    print(f"{len(rows)} capabilities")


if __name__ == "__main__":
    main()

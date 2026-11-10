#!/usr/bin/env python3
"""Compare exact manifest-derived goal schema and ask/goal prompt token estimates.
These are conservative model-token estimates, not actual provider usage.
Old manifests come from pinned HEAD, new from current isolated checkout.
"""
import json
import subprocess
import tomllib
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]

def estimate(text):
    ascii_count=cjk=other=0
    for ch in text:
        cp=ord(ch)
        if cp<128:ascii_count+=1
        elif (0x3040<=cp<=0x30ff or 0x3400<=cp<=0x4dbf
              or 0x4e00<=cp<=0x9fff or 0xf900<=cp<=0xfaff
              or 0xff00<=cp<=0xffef or 0x20000<=cp<=0x3ffff):
            cjk+=1
        else:other+=1
    return (ascii_count*2+6)//7+(cjk*13+9)//10+other

def get(stage,name):
    rel=f"examples/plugins/{name}/plugin.toml"
    if stage=="old":
        raw=subprocess.check_output(["git","show","HEAD:"+rel],cwd=ROOT)
    else:
        raw=(ROOT/rel).read_bytes()
    return tomllib.loads(raw.decode("utf-8"))

def schema(tool):
    return {"type":"function","function":{"name":tool["name"],
        "description":tool["description"],"parameters":tool["parameters"]}}

def compare(name):
    before=get("old",name);after=get("new",name)
    result={}
    for label,manifest in (("before",before),("after",after)):
        prompt=manifest.get("prompt","")
        arr=[schema(t) for t in manifest.get("tools",[]) if "parameters" in t]
        wire=json.dumps(arr,ensure_ascii=False,separators=(",",":"))
        result[label]={"prompt_chars":len(prompt),"prompt_estimated_tokens":estimate(prompt),
            "tool_schema_bytes":len(wire.encode()),"tool_schema_estimated_tokens":estimate(wire)}
    result["delta_estimated_tokens"]=(
        result["after"]["prompt_estimated_tokens"]+result["after"]["tool_schema_estimated_tokens"]-
        result["before"]["prompt_estimated_tokens"]-result["before"]["tool_schema_estimated_tokens"])
    return result

if __name__=="__main__":
    print(json.dumps({"goal":compare("goal"),"ask":compare("ask")},ensure_ascii=False,indent=2))

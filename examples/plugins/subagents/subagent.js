"use strict";
const {spawn} = require("node:child_process");
const os=require("node:os");
const path=require("node:path");
const crypto=require("node:crypto");

function bounded(v,def,min,max) {
  const n=Number(v);
  return Number.isFinite(n) ? Math.min(max,Math.max(min,Math.floor(n))) : def;
}
async function runOne(task,request={},spawnProcess=spawn) {
  if(typeof task!=="string" || !task.trim()) throw Error("subagent task must be nonempty");
  const config=request.config ?? {};
  const depth=Number(process.env.KROMA_SUBAGENT_DEPTH ?? 0);
  const maxDepth=bounded(config.max_depth,2,1,100);
  if(depth>=maxDepth) throw Error("Subagent nesting depth reached; configure max_depth to change");
  const executable=String(config.kroma_bin || process.env.KROMA_BIN || "kroma");
  const workdir=String(request.workdir || process.cwd());
  const session=path.join(os.tmpdir(),"kroma-subagent-"+process.pid+"-"+crypto.randomUUID()+".json");
  const maxChars=bounded(config.max_output_chars,16000,1000,100000);
  const env={...process.env,KROMA_WORKDIR:workdir,KROMA_SUBAGENT_DEPTH:String(depth+1),KROMA_SESSION_FILE:session};
  const timeout=bounded(config.timeout_ms,0,0,600000);
  return new Promise((resolve,reject)=>{
    const child=spawnProcess(executable,["--print",task],{
      cwd:workdir,env,windowsHide:true,stdio:["ignore","pipe","pipe"]
    });
    let stdout="",stderr="",finished=false;
    function append(target,chunk){return (target+String(chunk)).slice(-maxChars);}
    child.stdout?.on("data",chunk=>stdout=append(stdout,chunk));
    child.stderr?.on("data",chunk=>stderr=append(stderr,chunk));
    child.once("error",error=>{if(!finished){finished=true;reject(error);}});
    child.once("close",code=>{
      if(finished) return;
      finished=true;
      resolve({ok:code===0,code,output:stdout.trim(),error:stderr.trim()});
    });
    if(timeout>0){
      const timer=setTimeout(()=>{if(!finished)child.kill();},timeout);
      child.once("close",()=>clearTimeout(timer));
      child.once("error",()=>clearTimeout(timer));
    }
  });
}
async function dispatch(request,spawnProcess=spawn) {
  if(request.type!=="tool") throw Error("Only tools are supported");
  if(request.name==="subagent_run") return JSON.stringify(
    await runOne(request.arguments?.task,request,spawnProcess),null,2
  );
  if(request.name==="subagent_parallel") {
    const tasks=request.arguments?.tasks;
    if(!Array.isArray(tasks)||!tasks.length)throw Error("tasks must be a nonempty array");
    const limit=bounded(request.config?.max_parallel,3,1,8);
    const results=new Array(tasks.length);
    let index=0;
    await Promise.all(Array.from({length:Math.min(tasks.length,limit)},async()=>{
      while(index<tasks.length){
        const at=index++;
        try{results[at]=await runOne(tasks[at],request,spawnProcess);}
        catch(error){results[at]={ok:false,error:String(error.message||error)};}
      }
    }));
    return JSON.stringify({results},null,2);
  }
  throw Error("Unknown tool "+request.name);
}
module.exports={dispatch,runOne};

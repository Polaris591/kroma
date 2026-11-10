// Forensic local mock HTTP provider; prints captured wire request JSON only.
// No external HTTP requests, credentials or model billing.
const http = require("node:http");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {spawn} = require("node:child_process");
const crypto = require("node:crypto");
const mode = process.argv[2] || "baseline";
const repo = path.resolve(__dirname, "..");
const baseHome = process.env.USERPROFILE || process.env.HOME;
const temp = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-audit-v2-"));
const scenarios = {
  success: "printf KROMA_AUDIT_OK",
  truncated: "for i in $(seq 1 9000); do printf 'AUDIT%06d-ABCDEFGHIJKLMN-0123456789\\n' \"$i\"; done",
  timeout: "sleep 3",
  spawn_error: "Write-Output ignored",
};
const requestLog = [];
const finalText = "Audit mock finished.";
function contentTokens(text) {
  let a=0,c=0,o=0;
  for (const char of text) {
    const cp = char.codePointAt(0);
    if (cp < 128) a++;
    else if ((cp>=0x3040&&cp<=0x30ff)||(cp>=0x3400&&cp<=0x4dbf)||
      (cp>=0x4e00&&cp<=0x9fff)||(cp>=0xf900&&cp<=0xfaff)||
      (cp>=0xff00&&cp<=0xffef)||(cp>=0x20000&&cp<=0x3ffff)) c++;
    else o++;
  }
  return Math.ceil(a*2/7)+Math.ceil(c*13/10)+o;
}
const server = http.createServer(async (req,res) => {
  if (req.method!=="POST"||req.url!=="/v1/chat/completions") {
    res.writeHead(404);res.end();return;
  }
  let chunks=[];
  for await (const c of req) chunks.push(c);
  let data=JSON.parse(Buffer.concat(chunks).toString("utf8"));
  requestLog.push(data);
  const isTool = Object.hasOwn(scenarios,mode) && requestLog.length===1;
  const event = isTool
    ? {choices:[{index:0,delta:{tool_calls:[{index:0,id:"audit-call-1",type:"function",function:{name:"shell",arguments:JSON.stringify({command:scenarios[mode]})}}]},finish_reason:"tool_calls"}]}
    : {choices:[{index:0,delta:{content:finalText},finish_reason:"stop"}]};
  const body = "data: "+JSON.stringify(event)+"\n\n"+"data: [DONE]\n\n";
  res.writeHead(200,{"content-type":"text/event-stream","connection":"close"});
  res.end(body);
});
server.listen(0,"127.0.0.1",async()=>{
  try {
    const port=server.address().port;
    const env={...process.env};
    for (const k of ["KROMA_PROVIDER_PROFILE","KROMA_OFFICIAL_PLUGIN_DIR","KROMA_PLUGIN_TRY","KROMA_CHILD_POLICY_FILE","KROMA_DEEPSEEK_THINKING","KROMA_SHELL_TIMEOUT_SECS","KROMA_SHELL","KROMA_MCP_EAGER","KROMA_REASONING_EFFORT"]) delete env[k];
    env.HOME=temp;env.USERPROFILE=temp;
    env.KROMA_MODEL="audit-fixture-model";
    env.KROMA_BASE_URL="http://127.0.0.1:"+port+"/v1";
    env.KROMA_API_KEY="audit-fake-key-only";
    env.KROMA_API_PROTOCOL="chat";
    env.KROMA_WORKDIR=temp;
    env.KROMA_SESSION_FILE=path.join(temp,"audit-session.json");
    env.KROMA_CONTEXT_WINDOW="128000";
    env.KROMA_MAX_OUTPUT_TOKENS="4096";
    env.KROMA_TOOL_IDLE_SECS="0";
    env.KROMA_NO_CHECKPOINTS="1";
    env.KROMA_NO_MCP="1";
    env.KROMA_PLUGIN_DIR=mode==="all"?
      path.join(baseHome,".kroma","plugins"):path.join(temp,"plugins");
    if(mode==="timeout")env.KROMA_SHELL_TIMEOUT_SECS="1";
    if(mode==="spawn_error") env.KROMA_SHELL=path.join(temp,"missing","powershell.exe");
    const child=spawn(path.join(repo,"target","debug","kroma.exe"),["--print","Audit offline fixture."],{env,cwd:temp,windowsHide:true,stdio:["ignore","pipe","pipe"]});
    let stdout="",stderr="";
    child.stdout.on("data",v=>stdout+=v.toString());
    child.stderr.on("data",v=>stderr+=v.toString());
    const outcome=await new Promise((resolve,reject)=>{
      child.once("error",reject);
      child.once("close",(code,signal)=>resolve({code,signal}));
      setTimeout(()=>{child.kill();reject(new Error("fixture process deadline 30s"))},30000).unref();
    });
    const initial=requestLog[0]||{};
    const tools=initial.tools||[];
    const prompt=(initial.messages||[]).filter(m=>m.role==="system").map(m=>m.content??"").join("\n");
    const second=requestLog[1]||{};
    const resultMessages=(second.messages||[]).filter(m=>m.role==="tool");
    const toolRaw=resultMessages.at(-1)?.content;
    let toolJSON=null;try{toolJSON=JSON.parse(toolRaw);}catch(e){}
    const perTool=tools.map(t=>({name:t.function?.name||t.name,bytes:Buffer.byteLength(JSON.stringify(t)),estimated_tokens:contentTokens(JSON.stringify(t))}));
    const summary={
      mode,commit:"f64ab03dccf6bc0df1765d86bb66d3c4f2be6e93",cwd:temp,
      exit:outcome,requestCount:requestLog.length,toolCount:tools.length,
      tools_per_name:perTool,
      promptBytes:Buffer.byteLength(prompt),promptEstimatedTokens:contentTokens(prompt),
      toolsBytes:Buffer.byteLength(JSON.stringify(tools)),toolsEstimatedTokens:contentTokens(JSON.stringify(tools)),
      wholeRequestEstimatedTokens:contentTokens(JSON.stringify(initial))+256,
      stderr:stderr.slice(0,3000),stdout:stdout.slice(0,400),
      toolResultParsed:toolJSON,
    };
    process.stdout.write(JSON.stringify({summary,wire:{tools,system_prompt:prompt}},null,2));
  }catch(error){
    process.stdout.write(JSON.stringify({mode,error:String(error),requests:requestLog.length},null,2));
    process.exitCode=1;
  }finally {server.close();}
});

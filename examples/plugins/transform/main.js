"use strict";
// Demonstration hooks. Only the replaced `shell` tool is touched: enabling an
// example must never rewrite arguments or results of unrelated tools.
const isShell = req => req.arguments?.name === "shell";
require("node:readline").createInterface({input:process.stdin}).on("line",line=>{
  try {
    const req=JSON.parse(line);
    let content;
    if(req.type==="hook" && req.name==="before_tool_call") {
      content=JSON.stringify(isShell(req)
        ? {arguments:{command:"modified:" + req.arguments.arguments.command}}
        : {});
    } else if(req.type==="tool" && req.name==="shell") {
      content="replaced-shell:" + req.arguments.command;
    } else if(req.type==="hook" && req.name==="after_tool_result") {
      content=JSON.stringify(isShell(req)
        ? {content:req.arguments.content + ":after-hook",is_error:false}
        : {});
    } else {throw Error("unknown request");}
    process.stdout.write(JSON.stringify({content})+"\n");
  } catch(error) {
    process.stdout.write(JSON.stringify({content:String(error.message),is_error:true})+"\n");
  }
});

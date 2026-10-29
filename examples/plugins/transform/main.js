"use strict";
require("node:readline").createInterface({input:process.stdin}).on("line",line=>{
  try {
    const req=JSON.parse(line);
    let content;
    if(req.type==="hook" && req.name==="before_tool_call") {
      content=JSON.stringify({arguments:{command:"modified:" + req.arguments.arguments.command}});
    } else if(req.type==="tool" && req.name==="shell") {
      content="replaced-shell:" + req.arguments.command;
    } else if(req.type==="hook" && req.name==="after_tool_result") {
      content=JSON.stringify({content:req.arguments.content + ":after-hook",is_error:false});
    } else {throw Error("unknown request");}
    process.stdout.write(JSON.stringify({content})+"\n");
  } catch(error) {
    process.stdout.write(JSON.stringify({content:String(error.message),is_error:true})+"\n");
  }
});

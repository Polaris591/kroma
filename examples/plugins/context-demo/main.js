"use strict";
// Demonstrates the contract; intentionally does NOT compress history.
require("node:readline").createInterface({input:process.stdin}).on("line",line=>{
  try{
    const req=JSON.parse(line);
    let content;
    if(req.type==="context_prepare"){
      const state={revision:(req.arguments.state?.revision ?? 0)+1};
      content=JSON.stringify({messages:req.arguments.messages,state});
    } else if(req.type==="context_branch"){
      content=JSON.stringify({state:{revision:0}});
    } else throw Error("Unknown context operation "+req.type);
    process.stdout.write(JSON.stringify({content})+"\n");
  }catch(error){process.stdout.write(JSON.stringify({content:String(error.message),is_error:true})+"\n");}
});

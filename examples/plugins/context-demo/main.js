"use strict";
// Demonstrates the contract; intentionally does NOT compress history.
require("node:readline").createInterface({input:process.stdin}).on("line",line=>{
  try{
    const req=JSON.parse(line);
    let content;
    if(req.type==="context_prepare"){
      const state={revision:(req.arguments.state?.revision ?? 0)+1,
        received:(req.arguments.lifecycle??[]).map(event=>event.type)};
      content=JSON.stringify({messages:req.arguments.messages,state});
    } else if(req.type==="context_branch"){
      content=JSON.stringify({state:{revision:0}});
    } else if(req.type==="context_new_turn" || req.type==="context_mark_consumed" || req.type==="context_observe") {
      content=JSON.stringify({state:req.arguments.state ?? {revision:0}});
    } else if(req.type==="context_finish") {
      content=JSON.stringify({state:{...(req.arguments.state??{}),
        finished:(req.arguments.lifecycle??[]).map(event=>event.type)}});
    } else if(req.type==="context_overflow") {
      content=JSON.stringify({recovered:false});
    } else throw Error("Unknown context operation "+req.type);
    process.stdout.write(JSON.stringify({content})+"\n");
  }catch(error){process.stdout.write(JSON.stringify({content:String(error.message),is_error:true})+"\n");}
});

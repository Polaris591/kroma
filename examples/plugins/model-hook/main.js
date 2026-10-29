"use strict";
require("node:readline").createInterface({input:process.stdin}).on("line",line=>{
 try{
   const req=JSON.parse(line);
   if(req.type!=="hook"||req.name!=="before_model_request")throw Error("unknown hook");
   const messages=req.arguments.messages;
   const user=messages.findLast(m=>m.role==="user");
   if(user)user.content=(user.content ?? "")+" [model-hook]";
   process.stdout.write(JSON.stringify({content:JSON.stringify({messages})})+"\n");
 }catch(error){process.stdout.write(JSON.stringify({content:String(error.message),is_error:true})+"\n");}
});

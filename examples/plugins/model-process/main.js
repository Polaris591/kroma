"use strict";
// Replace the deterministic reply with calls to any external model protocol.
require("node:readline").createInterface({input:process.stdin,crlfDelay:Infinity}).on("line",line=>{
  try {
    const req=JSON.parse(line);
    let content;
    if(req.type==="model_complete"){
      content=JSON.stringify({message:{role:"assistant",content:"Model process replied"},
        usage:{input:100,output:5,cached:30}});
    } else if(req.type==="model_generate"){
      content=JSON.stringify({text:"Summary",requests:1,usage:{input:10,output:2,cached:0}});
    } else throw Error("Unknown model request: "+req.type);
    process.stdout.write(JSON.stringify({
      protocol_version:req.protocol_version,request_id:req.request_id,content
    })+"\n");
  } catch(error) {
    process.stdout.write(JSON.stringify({content:String(error.message),is_error:true})+"\n");
  }
});

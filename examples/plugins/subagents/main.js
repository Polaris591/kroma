"use strict";
const {dispatch}=require("./subagent");
require("node:readline").createInterface({input:process.stdin}).on("line",async line=>{
  try{process.stdout.write(JSON.stringify({content:await dispatch(JSON.parse(line))})+"\n");}
  catch(error){process.stdout.write(JSON.stringify({content:String(error.message||error),is_error:true})+"\n");}
});

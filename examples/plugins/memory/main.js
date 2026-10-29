"use strict";
const {apply}=require("./memory");
require("node:readline").createInterface({input:process.stdin,crlfDelay:Infinity}).on("line",line=>{
  try {
    process.stdout.write(JSON.stringify({content:apply(JSON.parse(line))})+"\n");
  }catch(error){
    process.stdout.write(JSON.stringify({content:String(error.message||error),is_error:true})+"\n");
  }
});

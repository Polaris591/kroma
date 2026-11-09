"use strict";
const fs=require("node:fs");
const path=require("node:path");
const {apply}=require("./memory");
// Notes live in the host-provided per-plugin data directory so uninstalling or
// reinstalling the plugin code never deletes user-authored notes.
const LEGACY=path.join(__dirname,"memory.json");
function notesPath(message){
  const root=typeof message?.data_dir==="string"&&message.data_dir?message.data_dir:null;
  if(!root)return LEGACY;
  fs.mkdirSync(root,{recursive:true});
  const file=path.join(root,"memory.json");
  if(!fs.existsSync(file)&&fs.existsSync(LEGACY))fs.copyFileSync(LEGACY,file);
  return file;
}
require("node:readline").createInterface({input:process.stdin,crlfDelay:Infinity}).on("line",line=>{
  try {
    const message=JSON.parse(line);
    process.stdout.write(JSON.stringify({content:apply(message,notesPath(message))})+"\n");
  }catch(error){
    process.stdout.write(JSON.stringify({content:String(error.message||error),is_error:true})+"\n");
  }
});

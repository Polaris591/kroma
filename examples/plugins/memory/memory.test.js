"use strict";
const {test}=require("node:test");
const assert=require("node:assert/strict");
const fs=require("node:fs");
const os=require("node:os");
const path=require("node:path");
const {apply,read}=require("./memory");
test("manual note memory persists and injects into request view only",()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),"kroma-memory-"));
  const file=path.join(dir,"memory.json");
  try{
    assert.equal(apply({type:"command",name:"/remember",arguments:"Prefer Rust"},file),"Stored note #1");
    assert.equal(read(file)[0],"Prefer Rust");
    assert.match(apply({type:"command",name:"/memories",arguments:""},file),/Prefer Rust/);
    const original=[{role:"system",content:"Original"},{role:"user",content:"Hello"}];
    const result=JSON.parse(apply({type:"hook",name:"before_model_request",arguments:{messages:original}},file));
    assert.match(result.messages[0].content,/Prefer Rust/);
    assert.equal(original[0].content,"Original");
    assert.equal(apply({type:"command",name:"/forgetnote",arguments:"1"},file),"Note deleted");
    assert.deepEqual(read(file),[]);
  }finally{fs.rmSync(dir,{recursive:true,force:true});}
});

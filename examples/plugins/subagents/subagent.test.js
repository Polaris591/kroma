"use strict";
const {test} = require("node:test");
const assert=require("node:assert/strict");
const {EventEmitter}=require("node:events");
const {dispatch}=require("./subagent");
function fakeSpawn(command,args,options){
  assert.ok(args[0]==="--print");
  assert.ok(options.env.KROMA_SESSION_FILE.includes("kroma-subagent-"));
  const child=new EventEmitter();
  child.stdout=new EventEmitter();
  child.stderr=new EventEmitter();
  setImmediate(()=>{
    child.stdout.emit("data",args[1]);
    child.emit("close",0);
  });
  return child;
}
test("subagent_run isolates session and executes a task",async()=>{
  const result=JSON.parse(await dispatch({
    type:"tool",name:"subagent_run",arguments:{task:"hello"},workdir:process.cwd(),config:{max_depth:3}
  },fakeSpawn));
  assert.equal(result.ok,true);
  assert.equal(result.output,"hello");
});
test("subagent_parallel preserves tasks result ordering",async()=>{
  const value=JSON.parse(await dispatch({
    type:"tool",name:"subagent_parallel",arguments:{tasks:["a","b","c"]},
    workdir:process.cwd(),config:{max_parallel:2}
  },fakeSpawn));
  assert.deepEqual(value.results.map(r=>r.output),["a","b","c"]);
});

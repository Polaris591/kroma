"use strict";
const {dispatch} = require("./web");
require("node:readline").createInterface({input:process.stdin,crlfDelay:Infinity}).on("line",async line=>{
  try {
    const content = await dispatch(JSON.parse(line));
    process.stdout.write(JSON.stringify({content})+"\n");
  } catch(error) {
    process.stdout.write(JSON.stringify({content:[error.message || String(error),error.cause?.message].filter(Boolean).join(": "),is_error:true})+"\n");
  }
});

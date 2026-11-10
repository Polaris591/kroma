// Offline aggregate of current-repository user session tool-call names only.
// Does not print user prompts, commands, results, or any raw session records.
// NOT a benchmark trace; this is a local transcript corpus.
const fs = require("node:fs");
const path = require("node:path");
const base = path.join(process.env.USERPROFILE, ".kroma", "sessions");
const list = fs.readdirSync(base);
const normalize = s => String(s||"").replaceAll(String.fromCharCode(92), "/")
 .toLowerCase().replace(/^([a-z]):/, "/$1").replace(/\/+$/, "");
const root = normalize(process.cwd());
const stats = {totalSessionSnapshots:0,parseFailures:0,matchedSessions:0,
 matchedWalFiles:0,malformedWalRecords:0,rawToolCallOccurrences:0};
const byName = new Map();
const ids = new Set();
function add(m, session) {
 if (m?.role !== "assistant") return;
 for (const tool of m.tool_calls || []) {
  const name = tool.function?.name;
  if (!name) continue;
  stats.rawToolCallOccurrences++;
  const id = session + "|" + (tool.id || "");
  if (ids.has(id)) continue;
  ids.add(id);
  byName.set(name, (byName.get(name)||0)+1);
 }
}
for (const f of list) {
 if (!f.endsWith(".json") || f.includes(".catalog") || f.endsWith(".last.json")) continue;
 const file = path.join(base,f);
 let session;
 try {session=JSON.parse(fs.readFileSync(file,"utf8"));stats.totalSessionSnapshots++;}
 catch {stats.parseFailures++;continue;}
 if(normalize(session.workdir)!==root)continue;
 stats.matchedSessions++;
 for(const msg of session.messages||[])add(msg,f);
 if (!fs.existsSync(file+".wal"))continue;
 stats.matchedWalFiles++;
 for (const line of fs.readFileSync(file+".wal","utf8").split("\n")) {
  if(!line)continue;
  let item;
  try{item=JSON.parse(line.split("\t",1)[0]);}
  catch {stats.malformedWalRecords++;continue;}
  if(item.generation !== session.generation)continue;
  add(item.message,f);
 }
}
stats.uniqueSessionToolIds=ids.size;
stats.byName=Array.from(byName).sort((a,b)=>b[1]-a[1]).map(([name,calls])=>({name,calls}));
console.log(JSON.stringify(stats,null,2));

// Audit only: execute the real App.svelte function bodies, not a reimplementation.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require('../../desktop/node_modules/typescript');
const source = fs.readFileSync(path.join(__dirname, '../../desktop/src/App.svelte'), 'utf8').match(/<script lang="ts">([\s\S]*?)<\/script>/)[1];
const ast = ts.createSourceFile('App.ts', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
const wanted = new Set(['syncHistoryState', 'loadOlderHistory', 'switchSession']);
const functions = ast.statements.filter(n => ts.isFunctionDeclaration(n) && wanted.has(n.name?.text)).map(n => n.getText(ast)).join('\n');
function harness(initial, api) {
  const sandbox = {initial, api};
  vm.createContext(sandbox);
  const code = `let data=initial,busy=false,running=false,historyBusy=false,olderMessages=[],historyHasMore=true,historyCount=150,taskGeneration=0,error='',stream='',thinking='',activeTools=[],tab='chat';
  function showError(e){throw e;} async function scrollDown(){}
  function isActiveTask(generation,current){return generation===current;}
  function historyPageBelongsTo(pageSession,visibleSession,pageGeneration,currentGeneration){return pageGeneration===currentGeneration&&pageSession===visibleSession;}
  ${functions}
  globalThis.audit={loadOlderHistory,switchSession,get:()=>({data,olderMessages,historyHasMore,historyCount,visible:[...olderMessages,...data.snapshot.messages].filter(m=>m.role!=='system').slice(-historyCount)})};`;
  vm.runInContext(ts.transpileModule(code, {compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.None}}).outputText, sandbox);
  return sandbox.audit;
}
(async () => {
  const recent = Array.from({length:1000}, (_,n)=>({role:'user',content:'recent-'+n}));
  const old = Array.from({length:200}, (_,n)=>({role:'user',content:'older-'+n}));
  const a = {snapshot:{session_id:'A',total_messages:1200,messages:recent,history_truncated:true}};
  const ui = harness(a, {historyPage:async()=>({session_id:'A',messages:old,next_before:0})});
  const before = JSON.stringify(ui.get().visible);
  await ui.loadOlderHistory();
  console.log('PAGINATION', JSON.stringify({loadedOlder:ui.get().olderMessages.length,historyCount:ui.get().historyCount,visibleUnchanged:before===JSON.stringify(ui.get().visible)}));
  let resolvePage;
  const page = new Promise(resolve=>{resolvePage=resolve});
  const b = {snapshot:{session_id:'B',total_messages:1,messages:[{role:'user',content:'B recent'}],history_truncated:false}};
  const raced = harness(a, {historyPage:()=>page,switchSession:async()=>b});
  const pending = raced.loadOlderHistory();
  await raced.switchSession('B');
  resolvePage({session_id:'A',messages:[{role:'user',content:'A PRIVATE older history'}],next_before:800});
  await pending;
  console.log('HISTORY_RACE', JSON.stringify({visibleSession:raced.get().data.snapshot.session_id,wrongSessionVisible:raced.get().visible.some(m=>m.content==='A PRIVATE older history')}));
})().catch(error=>{console.error(error);process.exitCode=1});

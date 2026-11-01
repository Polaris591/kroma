// Audit only: execute the real App.svelte function bodies without a WebView.
const fs = require('node:fs');
const vm = require('node:vm');
const ts = require('../../desktop/node_modules/typescript');
const path = require('node:path');
const source = fs.readFileSync(path.join(__dirname, '../../desktop/src/App.svelte'), 'utf8').match(/<script lang="ts">([\s\S]*?)<\/script>/)[1];
const ast = ts.createSourceFile('App.ts', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
const wanted = new Set(['syncHistoryState', 'loadOlderHistory', 'switchSession']);
const functions = ast.statements.filter(n => ts.isFunctionDeclaration(n) && wanted.has(n.name?.text)).map(n => n.getText(ast)).join('\n');
let resolvePage;
const deferred = new Promise(resolve => { resolvePage = resolve; });
const a = {project:'project-A', ready:true, snapshot:{session_id:'A', total_messages:1200, messages:[{role:'user',content:'A recent'}], history_truncated:true}};
const b = {project:'project-A', ready:true, snapshot:{session_id:'B', total_messages:2, messages:[{role:'user',content:'B recent'}], history_truncated:false}};
const sandbox = {api:{historyPage:()=>deferred,switchSession:async()=>b},initial:a,console};
vm.createContext(sandbox);
const code = `let data=initial,busy=false,running=false,historyBusy=false,olderMessages=[],historyHasMore=true,historyCount=150,taskGeneration=0,error='',stream='',thinking='',activeTools=[],tab='chat';
function showError(e){throw e;} async function scrollDown(){}
${functions}
globalThis.audit={loadOlderHistory,switchSession,get:()=>({data,olderMessages,historyHasMore,taskGeneration})};`;
vm.runInContext(ts.transpileModule(code,{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.None}}).outputText,sandbox);
(async()=>{
 const pending=sandbox.audit.loadOlderHistory();
 await sandbox.audit.switchSession('B');
 resolvePage({session_id:'A',messages:[{role:'user',content:'A PRIVATE older history'}],next_before:800});
 await pending;
 const state=sandbox.audit.get();
 console.log(JSON.stringify({visibleSession:state.data.snapshot.session_id,olderMessages:state.olderMessages,wrongSessionMerged:state.olderMessages.some(m=>m.content.includes('A PRIVATE'))},null,2));
})();

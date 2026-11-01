const fs=require('node:fs'),os=require('node:os'),path=require('node:path');
const {spawn}=require('node:child_process');
const root=fs.mkdtempSync(path.join(os.tmpdir(),'kroma-disabled-scan-'));
const project=path.join(root,'project'), plugin=path.join(project,'.kroma','plugins','cycle');
fs.mkdirSync(plugin,{recursive:true});
fs.writeFileSync(path.join(plugin,'plugin.toml'),"id='cycle'\nname='Cycle'\nprompt='disabled audit plugin'\n");
fs.writeFileSync(path.join(plugin,'main.js'),'// fingerprint fixture');
const env={...process.env,HOME:path.join(root,'home'),KROMA_WORKDIR:project,KROMA_PLUGIN_DIR:path.join(root,'user','plugins')};
function run(){return new Promise(resolve=>{const started=Date.now();const child=spawn(path.resolve(__dirname,'../../target/debug/kroma.exe'),['plugins','list'],{cwd:project,env,stdio:'ignore'});let timedOut=false;const timer=setTimeout(()=>{timedOut=true;child.kill();},2000);child.on('exit',(code)=>{clearTimeout(timer);resolve({code,timedOut,elapsedMs:Date.now()-started})});child.on('error',error=>{clearTimeout(timer);resolve({error:String(error)})});});}
(async()=>{try{const baseline=await run();fs.symlinkSync(plugin,path.join(plugin,'loop1'),'junction');fs.symlinkSync(plugin,path.join(plugin,'loop2'),'junction');const cyclic=await run();console.log(JSON.stringify({baseline,cyclic,pluginEnabled:false},null,2));}finally{for(const name of ['loop1','loop2'])try{fs.unlinkSync(path.join(plugin,name))}catch{}fs.rmSync(root,{recursive:true,force:true});}})();

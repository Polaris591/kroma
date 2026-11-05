//! Read-only production audit: isolated filesystem fixtures and loopback HTTP only.
use anyhow::Result;
use kroma::{
    context::{BuiltinContextFactory, CompactionState, Compression, ContextConfig, ContextFactory},
    context_state::PersistedContext,
    desktop_host::DesktopEvents,
    events::{EventSink, RuntimeEvent},
    model::{Completion, ModelBackend, ModelFuture, StreamFuture, TextResult},
    plugins::PluginCatalog,
    ports::ContextCommit,
    profiles::{Profiles, ProviderProfile},
    protocol::Message,
    provider::ModelClient,
    sessions::SessionCatalog,
    tools::ToolRegistry,
    prompt::PromptComposer,
    agent::Agent,
};
use serde_json::Value;
use std::{fs, io::{Read, Write}, net::TcpListener, path::Path,
    sync::{Arc, atomic::{AtomicUsize, Ordering}}, thread, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
struct NoCommit;
impl ContextCommit for NoCommit {
    fn commit(&self, _: &PersistedContext) -> Result<()> { Ok(()) }
}
struct Backend(Arc<AtomicUsize>);
impl ModelBackend for Backend {
    fn complete<'a>(&'a self, _: &'a [Message], _: &'a [Value], _: &'a mut (dyn FnMut(&str) + Send)) -> StreamFuture<'a, Completion> {
        Box::pin(async { Ok(Completion { message: Message::new("assistant", "ok"), usage: None, elapsed: Duration::ZERO, first_token: None }) })
    }
    fn generate_text<'a>(&'a self, _: &'a str, _: &'a str, _: usize) -> ModelFuture<'a, TextResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { anyhow::bail!("audit checkpoint reached backend") })
    }
}

struct CheckpointBackend(Arc<AtomicUsize>);
impl ModelBackend for CheckpointBackend {
    fn complete<'a>(&'a self, _: &'a [Message], _: &'a [Value], _: &'a mut (dyn FnMut(&str) + Send)) -> StreamFuture<'a, Completion> {
        Box::pin(async { anyhow::bail!("control must not generate a coding response") })
    }
    fn generate_text<'a>(&'a self, _: &'a str, _: &'a str, _: usize) -> ModelFuture<'a, TextResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(TextResult {
            text: "## OBJECTIVE\nContinue the isolated audit of a completed conversation.\n## CONSTRAINTS\nDo not rerun completed tools or change the production source.\n## PROGRESS\nSeveral independent conversation turns completed successfully.\n## CODE STATE\nOnly isolated temporary fixtures were created for this audit.\n## UNRESOLVED\nThe branch must retain the confirmed consumed-history boundary.\n## NEXT ACTION\nContinue from the retained transcript after validating the projected context.\n".into(),
            usage: None, elapsed: Duration::ZERO, requests: 1,
        }) })
    }
    fn checkpoint_output_ceiling(&self, _: usize, _: usize) -> usize {128}
}
async fn branch_control() -> Result<()> {
    let mut source=vec![Message::new("system","isolated control")];
    for _ in 0..12 {source.push(Message::new("user","word ".repeat(250)));source.push(Message::new("assistant","completed"));}
    let kept=source.len(); source.push(Message::new("user","abandoned future"));
    let mut original=CompactionState::default();original.consumed_until=kept-1;
    let state=BuiltinContextFactory.branch_state(&PersistedContext::Builtin(original.clone()),&source,&source[..kept],Path::new("source.json"),Path::new("branch.json"))?;
    let PersistedContext::Builtin(branch)=state else {unreachable!()};
    assert_eq!(branch.consumed_until,original.consumed_until,"the confirmed prefix must survive the rewind");
    let calls=Arc::new(AtomicUsize::new(0));let backend:Arc<dyn ModelBackend>=Arc::new(CheckpointBackend(calls.clone()));
    let mut context=Compression::new(branch,ContextConfig::with_output_reserve(4096,512)?,Path::new("branch.json"));
    let prepared=context.prepare(&source[..kept],&backend,&NoCommit).await;
    println!("SHORTENED_BRANCH_CONTROL rewound_succeeded={} checkpoint_calls={}",prepared.is_ok(),calls.load(Ordering::SeqCst));
    prepared?;assert_eq!(calls.load(Ordering::SeqCst),1);Ok(())
}

fn read_request(stream: &mut std::net::TcpStream) {
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut bytes = Vec::new(); let mut one = [0];
    while stream.read_exact(&mut one).is_ok() { bytes.push(one[0]); if bytes.ends_with(b"\r\n\r\n") { break; } }
    let headers = String::from_utf8_lossy(&bytes);
    let n: usize = headers.lines().filter_map(|s|s.split_once(':')).find(|(k,_)|k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_,v)|v.trim().parse().ok()).unwrap_or(0);
    stream.read_exact(&mut vec![0; n]).unwrap();
}
async fn early_disconnect(body: &'static str) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut count = 0;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream,_)) => {
                    count += 1; read_request(&mut stream);
                    write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n{body}").unwrap();
                    stream.flush().unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("{error}"),
            }
        }
        count
    });
    let error = ModelClient::new(&url,None,"audit".into())?.complete_with_tools(&[Message::new("user","test")],&[],&mut |_|{}).await.err().unwrap();
    let attempts = handle.join().unwrap();
    println!("EARLY_BODY_CLOSE heartbeat={} requests={} error={error:#}", !body.is_empty(), attempts);
    assert_eq!(attempts, 4, "a body truncated before any event is retried up to the budget");
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let root = std::env::temp_dir().join(format!("kroma-continuation-audit-{}-{}",std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()));
    let home = root.join("home"); let project = root.join("project");
    fs::create_dir_all(&home)?; fs::create_dir_all(&project)?;
    for key in ["KROMA_PLUGIN_TRY","KROMA_SESSION_FILE","KROMA_OUTPUT_RESERVE_TOKENS","KROMA_CONTEXT_WINDOW","KROMA_TRUST_PROJECT_MCP"] { std::env::remove_var(key); }
    std::env::set_var("HOME",&home); std::env::set_var("USERPROFILE",&home);
    branch_control().await?;
    early_disconnect("").await?;
    early_disconnect(": heartbeat\n\n").await?;
    let counter=Arc::new(AtomicUsize::new(0)); let model:Arc<dyn ModelBackend>=Arc::new(Backend(counter.clone()));
    let messages=vec![Message::new("system","audit"),Message::new("user","word ".repeat(4000)),Message::new("assistant","done"),Message::new("user","later")];
    let mut state=CompactionState::default(); state.consumed_until=3; let state=PersistedContext::Builtin(state);
    let branched=BuiltinContextFactory.branch_state(&state,&messages,&messages[..3],Path::new("source.json"),Path::new("fork.json"))?;
    let PersistedContext::Builtin(b)=branched else {unreachable!()};
    let consumed=b.consumed_until;
    let mut compression=Compression::new(b,ContextConfig::with_output_reserve(4096,512)?,Path::new("audit.json"));
    let error=compression.prepare(&messages[..3],&model,&NoCommit).await.err().unwrap();
    println!("SHORTENED_BRANCH consumed_until={consumed} checkpoint_calls={} error={error:#}",counter.load(Ordering::SeqCst));
    assert_eq!(consumed,3); assert!(error.to_string().contains("near hard limit"), "a single oversized message still fails honestly: {error:#}");
    let plugin_dir=project.join(".kroma/plugins/python-audit"); fs::create_dir_all(&plugin_dir)?;
    fs::write(plugin_dir.join("plugin.toml"),"id='python-audit'\nname='Python audit'\ncommand='python'\nargs=['main.py']\n[[tools]]\nname='audit_tool'\ndescription='audit only'\n")?;
    fs::write(plugin_dir.join("main.py"),"print(1)\n")?;
    let plugins=home.join("plugins"); let plugin_state=home.join("plugins.toml");
    let catalog=PluginCatalog::discover_at(plugins.clone(),plugin_state.clone(),&project)?;
    catalog.set_enabled("python-audit",true)?;
    fs::write(plugin_dir.join("main.py"),"print(2)\n")?;
    let catalog=PluginCatalog::discover_at(plugins.clone(),plugin_state.clone(),&project)?;
    let mut tools=ToolRegistry::new(); let warnings=catalog.contribute(&mut tools,&mut PromptComposer::default());
    println!("PROJECT_PYTHON_SAME_LENGTH_EDIT active_tool_count={} warnings={warnings:?}",tools.definitions().len());
    assert_eq!(tools.definitions().len(),0);
    fs::write(plugin_dir.join("plugin.toml"),"id='python-audit'\nname='Changed manifest'\ncommand='python'\nargs=['main.py']\n[[tools]]\nname='audit_tool'\ndescription='audit only'\n")?;
    let catalog=PluginCatalog::discover_at(plugins,plugin_state,&project)?;
    let mut tools=ToolRegistry::new(); catalog.contribute(&mut tools,&mut PromptComposer::default());
    let enabled=catalog.summaries()?[0]["enabled"].as_bool().unwrap();
    println!("PROJECT_CHANGED_MANIFEST summary_enabled={enabled} actual_tool_count={}",tools.definitions().len());
    assert!(!enabled); assert!(tools.definitions().is_empty());
    let events=DesktopEvents::default();
    for n in 0..600 { events.on_event(RuntimeEvent::ModelStarted{request:n}); }
    println!("DESKTOP_QUEUE status_events={}",events.drain().len());
    let backend=Arc::new(Backend(Arc::new(AtomicUsize::new(0))));
    let agent=Agent::with_runtime_budget(backend,ToolRegistry::new(),Arc::new(BuiltinContextFactory),PromptComposer::default(),None,project.clone(),ContextConfig::with_output_reserve(128000,4096)?)?;
    let catalog=SessionCatalog::new(&project)?;
    let default=catalog.default_path(); let ledger=default.with_extension("usage.json");
    fs::write(&ledger,b"invalid-json")?;
    events.on_event(RuntimeEvent::ModelFinished { request:1,tools:0,reasoning:None,input_tokens:Some(123),output_tokens:Some(4),cached_tokens:Some(0),reasoning_tokens:None });
    assert!(events.persist_usage(&catalog,agent.session_id()).is_err());
    fs::write(&ledger,b"{}")?;
    let saved=events.persist_usage(&catalog,agent.session_id())?;
    println!("DESKTOP_USAGE_AFTER_STORAGE_RECOVERY calls={} input={}",saved.coding.calls,saved.coding.input);
    assert_eq!(saved.coding.calls,1);
    let profile_file=home.join("profiles.json");
    let profile=ProviderProfile {base_url:"http://127.0.0.1/v1".into(),model:"audit".into(),..Default::default()};
    Profiles::update_at(&profile_file,|profiles|{profiles.profiles.insert("a".into(),profile.clone());Ok(())})?;
    Profiles::update_at(&profile_file,|profiles|{profiles.profiles.insert("b".into(),profile.clone());Ok(())})?;
    let retained=Profiles::load_at(&profile_file)?.profiles.keys().cloned().collect::<Vec<_>>();
    println!("PROFILE_TWO_WRITERS retained_profiles={retained:?}");
    assert_eq!(retained,vec!["a".to_string(),"b".to_string()],"each transaction keeps the other writer's update");

    // Hashing an entry file does not cover imports after the scan prefix.
    let mcpdir=project.join(".kroma");
    fs::write(mcpdir.join("mcp.json"),r#"{"mcpServers":{"audit":{"command":"node","args":["entry.js"]}}}"#)?;
    let mut entry=String::from("/*"); entry.push_str(&" ".repeat(2*1024*1024)); entry.push_str("*/\nrequire('./helper.js');\n");
    fs::write(project.join("entry.js"),entry)?;
    fs::write(project.join("helper.js"),"module.exports=1;\n")?;
    kroma::mcp::trust_project_mcp(&project)?;
    fs::write(project.join("helper.js"),"module.exports=2;\n")?;
    let trusted=kroma::mcp::project_mcp_trusted(&project)?;
    println!("MCP_LATE_RELATIVE_IMPORT changed_dependency_still_trusted={trusted}");
    assert!(!trusted);
    // A successful-but-truncated attempt reports usage; a later HTTP failure loses it.
    let listener=TcpListener::bind("127.0.0.1:0")?;
    let url=format!("http://{}/v1",listener.local_addr()?);
    listener.set_nonblocking(true)?;
    let server=thread::spawn(move||{
        let deadline=Instant::now()+Duration::from_secs(8);
        for first in [true,false] {
            let (mut stream,_)=loop {match listener.accept() {Ok(pair)=>break pair,Err(e) if e.kind()==std::io::ErrorKind::WouldBlock && Instant::now()<deadline=>thread::sleep(Duration::from_millis(5)),Err(e)=>panic!("bounded audit server accept: {e}")}}; read_request(&mut stream);
            let (status,body)=if first {("200 OK",r#"{"choices":[{"message":{"role":"assistant","content":"partial"},"finish_reason":"length"}],"usage":{"prompt_tokens":100,"completion_tokens":10}}"#)}
                else {("401 Unauthorized",r#"{"error":{"message":"audit rejected"}}"#)};
            write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    let err=ModelClient::new(&url,None,"audit".into())?.generate_checkpoint(&[Message::new("system","audit"),Message::new("user","text")],&[],"checkpoint",128).await.err().unwrap();
    server.join().unwrap();
    let preserved=format!("{err:#}").contains("measured attempt(s)");
    println!("CHECKPOINT_FINAL_FAILURE earlier_reported_usage_preserved={preserved} error={err:#}");
    assert!(preserved,"the failed summary must report that it used measured attempts");

    println!("FIXTURE_ROOT={}",root.display());
    Ok(())
}

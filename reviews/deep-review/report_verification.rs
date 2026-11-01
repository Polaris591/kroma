//! Audit-only probe. Run from examples/, then archive under reviews/deep-review/.
//! Uses loopback HTTP, dummy credentials and an isolated temporary HOME.
use anyhow::Result;
use kroma::{
    app, config::Config,
    context::{CompactionState, Compression, ContextConfig, ContextPolicy},
    context_state::PersistedContext,
    model::ModelBackend,
    plugins::PluginCatalog,
    ports::ContextCommit,
    profiles::ProviderProfile,
    protocol::Message,
    provider::ModelClient,
};
use serde_json::{json, Value};
use std::{
    fs, io::{Read, Write}, net::{TcpListener, TcpStream}, path::Path,
    sync::Arc, thread, time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
struct NoCommit;
impl ContextCommit for NoCommit {
    fn commit(&self, _: &PersistedContext) -> Result<()> { Ok(()) }
}
fn request(stream: &mut TcpStream) -> Value {
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut headers = Vec::new();
    let mut byte = [0];
    while stream.read_exact(&mut byte).is_ok() {
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") { break; }
    }
    let text = String::from_utf8_lossy(&headers);
    let length = text.lines().filter_map(|s| s.split_once(':'))
        .find(|(k,_)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok()).unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}
fn server(status: &str, body: Value) -> (String, thread::JoinHandle<Value>) {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", socket.local_addr().unwrap());
    let status = status.to_owned();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            match socket.accept() {
                Ok((stream, _)) => break Some(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break None,
            }
        };
        let Some(mut stream) = stream.take() else { return Value::Null; };
        let payload = request(&mut stream);
        let body = body.to_string();
        write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        payload
    });
    (url, handle)
}
fn lock_paths(pid: u32) -> Vec<std::path::PathBuf> {
    fs::read_dir(std::env::temp_dir()).unwrap().flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&format!(".kroma-unsaved-{pid}-")))
        .map(|entry| entry.path()).collect()
}
#[tokio::main]
async fn main() -> Result<()> {
    let root = std::env::temp_dir().join(format!("kroma-report-check-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()));
    let home = root.join("home");
    let project = home.join("project");
    let plugins = home.join(".kroma/plugins");
    fs::create_dir_all(&project)?;
    fs::create_dir_all(&plugins)?;
    // Never read real profile/plugin settings or send real credentials.
    for key in ["KROMA_PLUGIN_TRY", "KROMA_PROVIDER_PROFILE", "KROMA_CONTEXT_WINDOW", "KROMA_OUTPUT_RESERVE_TOKENS", "KROMA_MAX_OUTPUT_TOKENS", "KROMA_CATALOG_MODEL", "KROMA_MCP_CONFIG", "KROMA_SESSION_FILE", "KROMA_DEEPSEEK_THINKING", "KROMA_REASONING_EFFORT", "KROMA_CONFIG_FILE"] { std::env::remove_var(key); }
    std::env::set_var("HOME", &home);
    std::env::set_var("USERPROFILE", &home);
    std::env::set_var("KROMA_PLUGIN_DIR", &plugins);
    std::env::set_var("KROMA_WORKDIR", &project);
    std::env::set_var("KROMA_MODEL", "audit-unknown");
    std::env::set_var("KROMA_BASE_URL", "http://127.0.0.1:9/v1");
    std::env::set_var("KROMA_API_KEY", "AUDIT-DUMMY-KEY");
    std::env::set_var("OPENAI_API_KEY", "AUDIT-DUMMY-OPENAI");
    std::env::set_var("KROMA_API_PROTOCOL", "chat");
    // Demonstrate the opt-in minimum-privilege plugin environment.
    std::env::set_var("KROMA_PLUGIN_ISOLATE_ENV", "1");

    // A ~100k source plus the 32k first-attempt cap cannot fit 128k; admission
    // must reject it before sending a request. This is not a tokenizer test.
    let (url, handle) = server("400 Bad Request", json!({"error":{"type":"invalid_request_error","message":"context_length_exceeded (audit mock)"}}));
    let model: Arc<dyn ModelBackend> = Arc::new(ModelClient::new(&url, None, "audit".into())?.with_protocol("anthropic")?.with_output_tokens(Some(32768)));
    let messages = vec![Message::new("system", "audit system"), Message::new("user", "word ".repeat(60_000)), Message::new("assistant", "previously consumed")];
    let budget = ContextConfig::with_output_reserve(128_000, 32_768)?;
    let mut compression = Compression::new(CompactionState::default(), budget, Path::new("audit.json"));
    compression.set_request_context(&model, &[]);
    compression.mark_consumed(messages.len());
    let result = ContextPolicy::compact(&mut compression, &messages, &model, &NoCommit, 0).await;
    let payload = handle.join().unwrap();
    let wire = payload.get("max_tokens").and_then(Value::as_u64);
    println!("CHECKPOINT request_sent={} wire_max_tokens={wire:?} window=128000 result_error={} soft_limit={}", payload.is_object(), result.is_err(), budget.soft_limit());

    // The shared text generation path ignores the catalog cap.
    let (url, handle) = server("200 OK", json!({"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}));
    let client = ModelClient::new(&url, None, "audit".into())?.with_catalog_defaults(Some(512), None);
    client.text_completion("system", "input", 4096).await?;
    let payload = handle.join().unwrap();
    println!("CATALOG effective_turn_cap={:?} text_generation_wire_max={}", client.effective_output_tokens(), payload["max_tokens"]);

    // Smoke-check code really runs and receives dummy host credentials.
    let fixture = root.join("environment-plugin");
    fs::create_dir_all(&fixture)?;
    fs::write(fixture.join("plugin.toml"), "id='environment'\nname='Environment probe'\nprompt='audit probe'\ncommand='node'\nargs=['main.js']\n")?;
    fs::write(fixture.join("main.js"), r#"const fs=require('node:fs');const rl=require('node:readline').createInterface({input:process.stdin});rl.on('line',line=>{const q=JSON.parse(line);const flags={kroma_dummy_seen:process.env.KROMA_API_KEY==='AUDIT-DUMMY-KEY',openai_dummy_seen:process.env.OPENAI_API_KEY==='AUDIT-DUMMY-OPENAI'};fs.writeFileSync('seen.json',JSON.stringify(flags));console.log(JSON.stringify({content:JSON.stringify(flags),request_id:q.request_id,protocol_version:q.protocol_version}));});"#)?;
    let state = home.join(".kroma/plugins.toml");
    let catalog = PluginCatalog::discover_at(plugins.clone(), state.clone(), &project)?;
    catalog.check(&fixture)?;
    println!("PLUGIN_CHECK_ENV {}", fs::read_to_string(fixture.join("seen.json"))?);

    let fixture = root.join("event-plugin");
    fs::create_dir_all(&fixture)?;
    fs::write(fixture.join("plugin.toml"), "id='event'\nname='Event probe'\ncommand='node'\nargs=['main.js']\nevents=['compaction_started']\n")?;
    fs::write(fixture.join("main.js"), r#"const rl=require("node:readline").createInterface({input:process.stdin});rl.on("line",line=>{const q=JSON.parse(line);console.log(JSON.stringify({content:"ok",request_id:q.request_id,protocol_version:q.protocol_version}));});"#)?;
    let accepted = catalog.check(&fixture);
    println!("EVENT_WHITELIST accepted={} error={:?}", accepted.is_ok(), accepted.err().map(|error| error.to_string()));

    let secret = plugins.join("empty-secret");
    fs::create_dir_all(&secret)?;
    fs::write(secret.join("plugin.toml"), "id='empty-secret'\nname='Empty secret'\nprompt='audit'\n[settings]\napi_key=''\n")?;
    let summaries = catalog.summaries()?;
    println!("EMPTY_SECRET {}", serde_json::to_string(&summaries)?);

    // Project activation survives a code change (path-bound trust, not content-bound).
    let local = project.join(".kroma/plugins/local");
    fs::create_dir_all(&local)?;
    fs::write(local.join("plugin.toml"), "id='local'\nname='Local'\nprompt='original'\n")?;
    let catalog = PluginCatalog::discover_at(plugins.clone(), state.clone(), &project)?;
    catalog.set_enabled("local", true)?;
    fs::write(local.join("plugin.toml"), "id='local'\nname='Local'\nprompt='changed after approval'\n")?;
    let summaries = catalog.summaries()?;
    println!("PROJECT_TRUST changed_manifest_still_enabled={}", summaries.iter().find(|p|p["id"]=="local").unwrap()["enabled"]);
    catalog.set_enabled("local", false)?;

    // Native no-probe budget differs from runtime; runtime metadata is only loopback.
    let profile = ProviderProfile { base_url:"http://127.0.0.1:9/v1".into(), model:"audit-unknown".into(), api_protocol:Some("responses".into()), context_window:Some(8192), ..Default::default() };
    let config = Config::from_profile(&profile, project.clone(), None)?;
    let (offline_budget, offline_report) = config.resolved_limits_without_probe()?;
    let mut runtime = app::default_runtime_with(config, |c| { c.defer_initial_session = true; Ok(()) }).await?;
    println!("NATIVE_LIMIT offline_reserve={} runtime_reserve={} offline_effective={:?} runtime_effective={:?}", offline_budget.output_reserve(), runtime.agent.context_window_tokens()-runtime.agent.available_context_tokens(), offline_report.effective_turn_output_tokens.value, runtime.agent.model_report().unwrap().effective_turn_output_tokens.value);
    let initial = lock_paths(std::process::id()).len();
    runtime.agent.enter_welcome()?;
    runtime.agent.enter_welcome()?;
    runtime.agent.enter_welcome()?;
    drop(runtime);
    let locks = lock_paths(std::process::id());
    println!("TRANSIENT_LOCKS initial={initial} after_three_new_and_drop={}", locks.len());

    // Diagnostics canonicalizes Windows paths, then fails to strip ordinary HOME.
    let bundle = root.join("bundle");
    kroma::diagnostics_bundle::generate(Some(bundle.clone()))?;
    let report: Value = serde_json::from_slice(&fs::read(bundle.join("summary.json"))?)?;
    println!("WINDOWS_DIAGNOSTICS home_redacted={} verbatim_prefix={}", report["workdir"].as_str().unwrap().starts_with('~'), report["workdir"].as_str().unwrap().starts_with(r"\\?\"));
    #[cfg(windows)] {
        std::env::set_var("KROMA_SHELL", "bash");
        let verbatim = project.canonicalize()?.to_string_lossy().into_owned();
        let normalized = verbatim.strip_prefix(r"\\?\").unwrap_or(&verbatim).replace('\\', "/");
        let raw_output = kroma::shell::execute_for_session(&format!("ls -d -- '{verbatim}'"), &project, &project.join("raw-path.json")).await?;
        let normal_output = kroma::shell::execute_for_session(&format!("ls -d -- '{normalized}'"), &project, &project.join("normal-path.json")).await?;
        println!("GIT_BASH_PATH quoted_verbatim_exit={} normalized_exit={}", serde_json::to_value(raw_output)?["exit_code"], serde_json::to_value(normal_output)?["exit_code"]);
        // A descendant must outlive its successful parent shell, but Job close kills it.
        std::env::set_var("KROMA_SHELL", "powershell");
        let script = project.join("background.ps1");
        fs::write(&script, "Start-Sleep -Milliseconds 800; Set-Content -LiteralPath 'background-done.txt' -Value 'alive'")?;
        let command = "Start-Process -FilePath powershell -ArgumentList '-NoProfile','-NonInteractive','-File','background.ps1' -WindowStyle Hidden; Start-Sleep -Milliseconds 200";
        let baseline = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", command]).current_dir(&project).status()?;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let baseline_completed = project.join("background-done.txt").exists();
        if baseline_completed { fs::remove_file(project.join("background-done.txt"))?; }
        println!("WINDOWS_BACKGROUND_CONTROL parent_success={} descendant_completed={baseline_completed}", baseline.success());
        let output = kroma::shell::execute_for_session(command, &project, &project.join("shell-audit.json")).await?;
        tokio::time::sleep(Duration::from_millis(1600)).await;
        println!("WINDOWS_BACKGROUND shell_exit={:?} descendant_completed={}", serde_json::to_value(output)?["exit_code"], project.join("background-done.txt").exists());
    }
    // A non-.json default uses a different WAL than SessionCatalog::list expects.
    let custom = root.join("foo.session");
    let other = root.join("foo.other");
    fs::write(&custom, b"{}")?;
    fs::write(&other, b"{}")?;
    let wal = root.join("foo.session.wal");
    fs::write(&wal, b"audit metadata fixture")?;
    let old_time = UNIX_EPOCH + Duration::from_secs(100_000);
    let new_time = UNIX_EPOCH + Duration::from_secs(200_000);
    fs::OpenOptions::new().write(true).open(&custom)?.set_times(fs::FileTimes::new().set_modified(old_time))?;
    fs::OpenOptions::new().write(true).open(&wal)?.set_times(fs::FileTimes::new().set_modified(new_time))?;
    std::env::set_var("KROMA_SESSION_FILE", &custom);
    let first = kroma::sessions::SessionCatalog::new(&project)?;
    let listed = first.list()?.into_iter().find(|entry| entry.id == "default").unwrap();
    first.name_session("default", "FIRST SESSION TITLE")?;
    std::env::set_var("KROMA_SESSION_FILE", &other);
    let second = kroma::sessions::SessionCatalog::new(&project)?;
    let second_entry = second.list()?.into_iter().find(|entry| entry.id == "default").unwrap();
    println!("CUSTOM_SUFFIX wal_mtime_ignored={} unrelated_default_inherits_title={}", listed.modified == Some(old_time), second_entry.name.as_deref() == Some("FIRST SESSION TITLE"));
    for path in locks { fs::remove_file(path)?; }
    fs::remove_dir_all(root)?;
    Ok(())
}

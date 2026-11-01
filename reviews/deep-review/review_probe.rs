//! Temporary audit-only executable; no production code changes.
use anyhow::Result;
use kroma::{
    app,
    config::Config,
    mcp,
    profiles::ProviderProfile,
    protocol::{FunctionCall, Message, ToolCall},
    provider::{reported_error_usage, ModelClient},
    tools::{ToolContext, ToolRegistry},
};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
struct TrackedAllocator;
unsafe impl GlobalAlloc for TrackedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        System.dealloc(ptr, layout);
    }
}
#[global_allocator]
static ALLOCATOR: TrackedAllocator = TrackedAllocator;

fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut one = [0u8];
    while stream.read_exact(&mut one).is_ok() {
        bytes.push(one[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let headers = String::from_utf8_lossy(&bytes).into_owned();
    let length = headers
        .lines()
        .filter_map(|s| s.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    let _ = stream.read_exact(&mut body);
    headers
}
fn serve_body(body: String) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    (url, handle)
}
#[tokio::main]
async fn main() -> Result<()> {
    let root = std::env::temp_dir().join(format!(
        "kroma-deep-review-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(&root)?;
    let home = root.join("home");
    fs::create_dir_all(&home)?;
    std::env::set_var("HOME", &home);
    std::env::remove_var("KROMA_TRUST_PROJECT_MCP");
    // A redirect between two loopback ports is cross-origin, just like an external host.
    let target = TcpListener::bind("127.0.0.1:0")?;
    target.set_nonblocking(true)?;
    let target_url = format!("http://{}/collect", target.local_addr()?);
    let collector = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut stream = loop {
            match target.accept() {
                Ok((stream, _)) => break Some(stream),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break None,
            }
        };
        match stream.take() {
            Some(mut stream) => {
                let request = read_request(&mut stream);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                request
            }
            None => String::new(),
        }
    });
    let source = TcpListener::bind("127.0.0.1:0")?;
    let source_url = format!("http://{}/v1", source.local_addr()?);
    let redirect = thread::spawn(move || {
        let (mut stream, _) = source.accept().unwrap();
        read_request(&mut stream);
        write!(stream,"HTTP/1.1 307 Temporary Redirect\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let client = ModelClient::new(&source_url, Some("AUDIT-DUMMY-KEY".into()), "mock".into())?
        .with_protocol("anthropic")?;
    let _ = client
        .complete_with_tools(&[Message::new("user", "hello")], &[], &mut |_| {})
        .await;
    redirect.join().unwrap();
    let captured = collector.join().unwrap();
    println!(
        "REDIRECT credential_forwarded={}",
        captured
            .to_ascii_lowercase()
            .contains("x-api-key: audit-dummy-key")
    );
    // Input usage is present in a valid initial Anthropic event before a server-side failure.
    for error_event in [true, false] {
        let mut body="data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":123,\"output_tokens\":5}}}\n\n".to_owned();
        if error_event {
            body.push_str("data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"audit failure\"}}\n\n");
        }
        let (url, server) = serve_body(body);
        let model = ModelClient::new(&url, None, "mock".into())?.with_protocol("anthropic")?;
        let error = model
            .complete_with_tools(&[Message::new("user", "hello")], &[], &mut |_| {})
            .await
            .err()
            .unwrap();
        println!(
            "FAILED_USAGE error_event={} usage={:?} error={:#}",
            error_event,
            reported_error_usage(&error),
            error
        );
        server.join().unwrap();
    }
    let archive = root.join("unicode.json");
    fs::write(
        &archive,
        serde_json::to_vec(
            &json!({"version":3,"workdir":root,"messages":[Message::new("system","system"),Message::new("user",format!("{}needle", "İ".repeat(300)))]}),
        )?,
    )?;
    let call = ToolCall {
        id: "probe".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "history_search".into(),
            arguments: json!({"query":"needle"}).to_string(),
        },
    };
    let mut registry = ToolRegistry::new();
    let profile = ProviderProfile {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "audit".into(),
        context_window: Some(128_000),
        max_output_tokens: Some(4096),
        ..Default::default()
    };
    let runtime =
        app::default_runtime_with(Config::from_profile(&profile, root.clone(), None)?, |c| {
            registry = c.tools.clone();
            c.defer_initial_session = true;
            Ok(())
        })
        .await?;
    let result = registry
        .execute(
            &call,
            ToolContext {
                workdir: &root,
                archive: &archive,
                on_output: None,
            },
        )
        .await;
    println!("UNICODE_SEARCH {}", result.content);
    drop(runtime);
    let n = 24;
    let mut projects = Vec::<PathBuf>::new();
    for i in 0..n {
        let project = root.join(format!("p{i}"));
        fs::create_dir_all(project.join(".kroma"))?;
        fs::write(project.join(".kroma/mcp.json"), "{\"mcpServers\":{}}")?;
        projects.push(project);
    }
    let barrier = Arc::new(Barrier::new(n));
    let workers: Vec<_> = projects
        .iter()
        .map(|path| {
            let path = path.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                mcp::trust_project_mcp(&path).map(|_| ())
            })
        })
        .collect();
    let mut errors = 0;
    for worker in workers {
        if worker.join().unwrap().is_err() {
            errors += 1;
        }
    }
    let trusted = projects
        .iter()
        .filter(|path| mcp::project_mcp_trusted(path).unwrap_or(false))
        .count();
    println!("TRUST_RACE attempts={n} errors={errors} persisted={trusted}");
    let slow = root.join("slow-approval");
    fs::create_dir_all(slow.join(".kroma"))?;
    let mut large = vec![b' '; 1024 * 1024 - 32];
    large.extend_from_slice(b"{\"mcpServers\":{}}");
    fs::write(slow.join(".kroma/mcp.json"), large)?;
    for wait in [1, 5, 10, 20] {
        mcp::revoke_project_mcp(&slow)?;
        mcp::trust_project_mcp(&projects[0])?;
        let path = slow.clone();
        let approving = thread::spawn(move || mcp::trust_project_mcp(&path));
        thread::sleep(Duration::from_millis(wait));
        mcp::revoke_project_mcp(&projects[0])?;
        approving.join().unwrap()?;
        println!(
            "TRUST_REVOKE_RACE delay_ms={wait} revoked_project_trusted_again={}",
            mcp::project_mcp_trusted(&projects[0])?
        );
    }
    let large_project = root.join("oversized-untrusted");
    fs::create_dir_all(large_project.join(".kroma"))?;
    fs::File::create(large_project.join(".kroma/mcp.json"))?.set_len(32 * 1024 * 1024)?;
    let baseline = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    let configured = mcp::configured_servers(&large_project)?;
    println!("MCP_CONFIG_LIMIT declared_limit_mib=1 input_mib=32 additional_peak_alloc_bytes={} configured_servers={}",PEAK.load(Ordering::SeqCst).saturating_sub(baseline),configured.len());
    fs::remove_dir_all(root)?;
    Ok(())
}

//! Two non-interactive runs must not share a conversation or its lock.
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kroma-cli-oneshot-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn mock_model(posts: Arc<AtomicUsize>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        while posts.load(Ordering::SeqCst) < 2 {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let post = request.starts_with("POST ");
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header == "\n" || header.is_empty() {
                    break;
                }
            }
            if post {
                posts.fetch_add(1, Ordering::SeqCst);
                let body = concat!(
                    "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"mock-ok\"}}]}\n\n",
                    "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
                    "data: [DONE]\n\n"
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            } else {
                let body = "{}";
                write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            stream.flush().unwrap();
        }
    });
    (format!("http://{address}/v1"), server)
}

#[test]
fn parallel_print_runs_create_distinct_sessions() {
    let home = temp_dir("home");
    let mcp = home.join("mcp.json");
    fs::write(&mcp, r#"{"mcpServers":{}}"#).unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let (base_url, server) = mock_model(Arc::clone(&posts));
    let binary = env!("CARGO_BIN_EXE_kroma");
    fn run(
        binary: &str,
        home: &std::path::Path,
        mcp: &std::path::Path,
        base_url: &str,
        prompt: &str,
    ) -> std::io::Result<std::process::Output> {
        Command::new(binary)
            .args(["--print", prompt])
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("KROMA_MODEL", "test")
            .env("KROMA_BASE_URL", base_url)
            .env("KROMA_API_KEY", "test")
            .env("KROMA_MCP_CONFIG", mcp)
            .env("KROMA_NO_MCP", "1")
            .env_remove("KROMA_SESSION_FILE")
            .stdin(Stdio::null())
            .output()
    }
    let home_a = home.clone();
    let mcp_a = mcp.clone();
    let base_a = base_url.clone();
    let first = std::thread::spawn(move || run(binary, &home_a, &mcp_a, &base_a, "alpha"));
    let home_b = home.clone();
    let mcp_b = mcp.clone();
    let base_b = base_url.clone();
    let second = std::thread::spawn(move || run(binary, &home_b, &mcp_b, &base_b, "beta"));
    let first = first.join().unwrap().unwrap();
    let second = second.join().unwrap().unwrap();
    server.join().ok();
    assert!(first.status.success(), "first: {first:?}");
    assert!(second.status.success(), "second: {second:?}");
    assert_eq!(String::from_utf8_lossy(&first.stdout).trim(), "mock-ok");
    assert_eq!(String::from_utf8_lossy(&second.stdout).trim(), "mock-ok");
    assert_eq!(posts.load(Ordering::SeqCst), 2);
    let sessions = home.join(".kroma/sessions");
    let mut ids = fs::read_dir(&sessions)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name.ends_with(".json") && !name.ends_with(".last.json")).then_some(name)
        })
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids.len(), 2, "fresh runs must not share a session: {ids:?}");
    assert!(ids.iter().all(|name| !name.starts_with("default")));
    assert!(!sessions.join("default.json").exists());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn unknown_option_is_a_usage_error_not_prompt_text() {
    let output = Command::new(env!("CARGO_BIN_EXE_kroma"))
        .arg("--bogus")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown option"), "{stderr}");
    assert!(!stderr.contains("task"));
}

#[test]
fn a_literal_flag_prompt_is_sent_to_the_model() {
    let home = temp_dir("literal-home");
    let mcp = home.join("mcp.json");
    fs::write(&mcp, r#"{"mcpServers":{}}"#).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || loop {
        let Ok((mut stream, _)) = listener.accept() else {
            break;
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" || header == "\n" || header.is_empty() {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok();
        if request.starts_with("POST ") {
            let body = concat!(
                    "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"literal-ok\"}}]}\n\n",
                    "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
                    "data: [DONE]\n\n"
                );
            write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            stream.flush().unwrap();
            break;
        }
        write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        )
        .unwrap();
        stream.flush().unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_kroma"))
        .args(["--print", "--", "--print"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("KROMA_MODEL", "test")
        .env("KROMA_BASE_URL", format!("http://{address}/v1"))
        .env("KROMA_API_KEY", "test")
        .env("KROMA_MCP_CONFIG", &mcp)
        .env("KROMA_NO_MCP", "1")
        .env("KROMA_NO_CHECKPOINTS", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    server.join().ok();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("literal-ok"), "{stdout}");
    assert!(!stdout.contains("Usage:"), "{stdout}");
    let _ = fs::remove_dir_all(home);
}

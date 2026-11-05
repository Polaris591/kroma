//! A real one-shot run must create a shadow-git snapshot without touching the
//! project's own Git metadata.
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
        "kroma-cli-checkpoint-{label}-{}-{}",
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
                let call = posts.fetch_add(1, Ordering::SeqCst);
                let sse = if call == 0 {
                    concat!(
                        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"echo changed > note.txt\\\"}\"}}]}}]}\n\n",
                        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
                        "data: [DONE]\n\n"
                    )
                } else {
                    concat!(
                        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done\"}}]}\n\n",
                        "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
                        "data: [DONE]\n\n"
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                    sse.len()
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
fn one_shot_run_snapshots_before_tools_without_touching_project_git() {
    let home = temp_dir("home");
    let workdir = temp_dir("project");
    let init = Command::new("git")
        .args(["-C", &workdir.to_string_lossy(), "init", "-q"])
        .status()
        .unwrap();
    assert!(init.success());
    fs::write(workdir.join("README.md"), "base\n").unwrap();
    let project_index = workdir.join(".git/index");
    let before_index = fs::read(&project_index).ok();

    let mcp = home.join("mcp.json");
    fs::write(&mcp, r#"{"mcpServers":{}}"#).unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let (base_url, server) = mock_model(Arc::clone(&posts));
    let output = Command::new(env!("CARGO_BIN_EXE_kroma"))
        .current_dir(&workdir)
        .args(["--print", "change the note"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("KROMA_MODEL", "test")
        .env("KROMA_BASE_URL", &base_url)
        .env("KROMA_API_KEY", "test")
        .env("KROMA_MCP_CONFIG", &mcp)
        .env("KROMA_NO_MCP", "1")
        .env_remove("KROMA_SESSION_FILE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    server.join().ok();
    assert!(output.status.success(), "{output:?}");
    let note = fs::read_to_string(workdir.join("note.txt"));
    assert!(
        note.as_deref().is_ok_and(|text| text.contains("changed")),
        "note={note:?} stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&project_index).ok(), before_index);

    let stores = fs::read_dir(home.join(".kroma/checkpoints"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "git"))
        .collect::<Vec<_>>();
    assert_eq!(stores.len(), 1, "one shadow store expected: {stores:?}");
    let log = Command::new("git")
        .args([
            "--git-dir",
            &stores[0].to_string_lossy(),
            "log",
            "--format=%s",
        ])
        .output()
        .unwrap();
    assert!(log.status.success(), "{log:?}");
    assert!(String::from_utf8_lossy(&log.stdout).contains("turn 1"));
    let _ = fs::remove_dir_all(home);
    let _ = fs::remove_dir_all(workdir);
}

#[test]
fn snapshot_failure_emits_a_warning_but_does_not_repeat_the_tool() {
    let home = temp_dir("warning-home");
    let workdir = temp_dir("warning-project");
    assert!(Command::new("git")
        .args(["-C", &workdir.to_string_lossy(), "init", "-q"])
        .status()
        .unwrap()
        .success());
    fs::create_dir_all(home.join(".kroma")).unwrap();
    fs::write(home.join(".kroma/checkpoints"), "not a directory").unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let (base_url, server) = mock_model(posts.clone());
    let output = Command::new(env!("CARGO_BIN_EXE_kroma"))
        .current_dir(&workdir)
        .args(["--json", "change the note"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("KROMA_MODEL", "test")
        .env("KROMA_BASE_URL", base_url)
        .env("KROMA_API_KEY", "test")
        .env("KROMA_NO_MCP", "1")
        .env_remove("KROMA_NO_CHECKPOINTS")
        .env_remove("KROMA_SESSION_FILE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    server.join().unwrap();
    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events.iter().any(|event| event["type"] == "warning"
            && event["message"]
                .as_str()
                .is_some_and(|message| message.contains("snapshot skipped"))),
        "{events:?}"
    );
    assert_eq!(posts.load(Ordering::SeqCst), 2);
    assert!(fs::read_to_string(workdir.join("note.txt"))
        .unwrap()
        .contains("changed"));
    fs::remove_dir_all(home).unwrap();
    fs::remove_dir_all(workdir).unwrap();
}

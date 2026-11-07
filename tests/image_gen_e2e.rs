use anyhow::{Context, Result};
use kroma::{
    plugins::PluginCatalog,
    prompt::PromptComposer,
    protocol::{FunctionCall, ToolCall},
    tools::{ToolContext, ToolRegistry},
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScL/nwAAAABJRU5ErkJggg==";

#[tokio::test]
async fn plugin_receives_opt_in_image_attachment_and_saves_a_durable_result() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root = std::env::temp_dir().join(format!("kroma-image-e2e-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&root)?;
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let workdir = root.join("project");
    fs::create_dir_all(&workdir)?;
    let image = workdir.join("reference.png");
    fs::write(
        &image,
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, PNG_BASE64)?,
    )?;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let mock = std::thread::spawn(move || -> Result<Value> {
        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(15)))?;
        let mut reader = BufReader::new(&mut stream);
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let mut size = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 || line == "\r\n" {
                break;
            }
            if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                size = n.trim().parse()?;
            }
        }
        let mut raw = vec![0; size];
        reader.read_exact(&mut raw)?;
        let request: Value = serde_json::from_slice(&raw)?;
        drop(reader);
        let payload = json!({"data":[{"b64_json":PNG_BASE64}]}).to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload)?;
        stream.flush()?;
        assert!(header.contains("/images/edits"), "{header}");
        Ok(request)
    });

    let installed = root.join("plugins");
    let state = root.join("plugins.toml");
    let discover = || PluginCatalog::discover_at(installed.clone(), state.clone(), &workdir);
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/image-gen");
    let catalog = discover()?;
    catalog.install(&source)?;
    catalog.set_enabled("image-gen", true)?;
    catalog.set_config("image-gen", "provider", Some("openai-images"))?;
    catalog.set_config(
        "image-gen",
        "base_url",
        Some(&format!("http://{address}/v1")),
    )?;
    // PATH exists already and is not a secret. Avoid changing the test process's
    // global environment or making a real paid-provider request.
    catalog.set_config("image-gen", "api_key_env", Some("PATH"))?;
    let mut tools = ToolRegistry::builtin();
    let mut prompt = PromptComposer::default();
    let warnings = discover()?.contribute(&mut tools, &mut prompt);
    assert!(warnings.is_empty(), "{warnings:?}");

    // Ref numbers are the visible attachment positions, not just image order.
    let attachments = vec![
        json!({"type":"kroma_attachment","kind":"file","name":"notes.txt",
            "path":workdir.join("notes.txt"),"bytes":5}),
        json!({"type":"kroma_attachment","kind":"image",
            "name":"reference.png","path":image,"mime":"image/png","bytes":68}),
    ];
    let call = ToolCall {
        id: "image-test".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "image_generate".into(),
            arguments: json!({"prompt":"Paint it blue", "input_images":["attachment:2"],
                "output_path":"assets/blue.png"})
            .to_string(),
        },
    };
    let archive = root.join("session.json");
    let result = tools
        .execute(
            &call,
            ToolContext {
                model_connection: None,
                user_message: None,
                user_turn: Some(1),
                user_images: Some(&attachments),
                previous_user_images: None,
                workdir: &workdir,
                archive: &archive,
                on_output: None,
            },
        )
        .await;
    assert!(!result.is_error, "{}", result.content);
    let answer: Value = serde_json::from_str(&result.content)?;
    let output = answer["images"][0]["path"]
        .as_str()
        .context("missing output path")?;
    assert_eq!(PathBuf::from(output), workdir.join("assets/blue.png"));
    assert!(fs::metadata(output)?.len() > 0);
    assert_eq!(answer["images"][0]["width"], 1);
    assert!(answer["images"][0]["id"]
        .as_str()
        .unwrap_or("")
        .starts_with("image:"));
    let body = mock.join().expect("mock thread panicked")?;
    let supplied = body["images"][0]["image_url"]
        .as_str()
        .context("missing edit image")?;
    assert!(supplied.starts_with("data:image/png;base64,"));
    Ok(())
}

//! Offline regression checks for JSON arguments -> native Bash -> exact bytes.
use anyhow::Result;
use kroma::{
    protocol::ToolCall,
    tools::{ToolContext, ToolRegistry, ToolResult},
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn bash_available() -> bool {
    let available = kroma::shell::tool_description().starts_with("Run a Bash command");
    if !available {
        eprintln!("skipping Bash-specific round-trip test: selected shell is not Bash");
    }
    available
}

fn workdir(label: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "kroma shell 中文 {label} {}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

async fn run_command(dir: &Path, command: &str) -> Result<(ToolResult, Value)> {
    let wire = json!({
        "id": "roundtrip",
        "type": "function",
        "function": {
            "name": "shell",
            "arguments": json!({"command": command}).to_string()
        }
    })
    .to_string();
    let call: ToolCall = serde_json::from_str(&wire)?;
    let arguments = call.function.arguments.clone();
    let archive = dir.join("session.json");
    let outcome = ToolRegistry::builtin()
        .execute(
            &call,
            ToolContext {
                model_connection: None,
                user_message: None,
                user_turn: None,
                user_images: None,
                previous_user_images: None,
                workdir: dir,
                archive: &archive,
                on_output: None,
            },
        )
        .await;
    assert_eq!(call.function.arguments, arguments);
    let payload = serde_json::from_str(&outcome.content)?;
    Ok((outcome, payload))
}

#[tokio::test]
async fn quoted_heredocs_preserve_python_source_and_literal_shell_syntax() -> Result<()> {
    if !bash_available() {
        return Ok(());
    }
    let dir = workdir("source")?;
    let source = r###"import json
text = "first\nsecond\tquote: \" slash: \\"
windows = r"C:\Kroma\new\test"
literal = r'$(printf unexpected) `printf unexpected` >nul 2>NUL'
print("中文 🦀", text, windows, literal)
"###;
    for delimiter in ["'KROMA_PAYLOAD'", "\"KROMA_PAYLOAD\""] {
        let command = format!(
            "cat > '输出 内容.py' <<{delimiter}\n{source}KROMA_PAYLOAD\ncat '输出 内容.py'"
        );
        let (outcome, payload) = run_command(&dir, &command).await?;
        assert!(!outcome.is_error, "{payload}");
        assert_eq!(payload["exit_code"], 0);
        assert_eq!(fs::read(dir.join("输出 内容.py"))?, source.as_bytes());
        assert_eq!(payload["stdout"], source);
        assert_eq!(payload["stderr"], "");
    }
    fs::remove_dir_all(dir)?;
    eprintln!("exercised quoted heredocs, Unicode paths, quotes and literal backslashes");
    Ok(())
}

#[tokio::test]
async fn shell_result_preserves_explicit_crlf_bytes() -> Result<()> {
    if !bash_available() {
        return Ok(());
    }
    let dir = workdir("crlf")?;
    let source = "actual first\r\nactual second\r\n";
    // Git Bash normalizes raw CRLF in command text; printf emits explicit bytes.
    let encoded = r"actual first\r\nactual second\r\n";
    let command = format!("printf '%b' '{encoded}' > 'crlf payload.txt'\nprintf '%b' '{encoded}'");
    let (outcome, payload) = run_command(&dir, &command).await?;
    assert!(!outcome.is_error, "{payload}");
    assert_eq!(payload["exit_code"], 0);
    assert_eq!(fs::read(dir.join("crlf payload.txt"))?, source.as_bytes());
    assert_eq!(payload["stdout"], source);
    fs::remove_dir_all(dir)?;
    eprintln!("exercised exact CRLF file and output bytes");
    Ok(())
}

#[tokio::test]
async fn failed_command_keeps_exact_stdout_stderr_and_exit_code() -> Result<()> {
    if !bash_available() {
        return Ok(());
    }
    let dir = workdir("failure")?;
    let stdout = "输出 'single' \"double\" \\n \\path\n";
    let stderr = "错误 $HOME $(printf unexpected) 2>nul\n";
    let command = format!(
        "cat <<'KROMA_STDOUT'\n{stdout}KROMA_STDOUT\ncat >&2 <<'KROMA_STDERR'\n{stderr}KROMA_STDERR\nexit 7"
    );
    let (outcome, payload) = run_command(&dir, &command).await?;
    assert!(outcome.is_error, "nonzero exit must not become success");
    assert_eq!(payload["exit_code"], 7);
    assert_eq!(payload["stdout"], stdout);
    assert_eq!(payload["stderr"], stderr);
    fs::remove_dir_all(dir)?;
    eprintln!("exercised exact stdout/stderr and nonzero-exit propagation");
    Ok(())
}

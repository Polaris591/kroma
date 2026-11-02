//! Regression: a `/steer` line that is already buffered on stdin must reach
//! the running task instead of being rejected before the task is activated.
//! Runs the real CLI binary against an external model-process plugin.
use anyhow::{bail, Context, Result};
use kroma::plugins::PluginCatalog;
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const STEER_TEXT: &str = "KEEP_THIS_INSTRUCTION";

const PLUGIN_MANIFEST: &str = r#"id = "steer-probe"
name = "Steering probe model"
version = "0.1.0"
command = "node"
args = ["main.js"]

[model]
model = "steer-probe"
protocol = "plugin"
context_window = 32768
output_reserve = 2048
"#;

// The first turn is held open so the CLI has time to read the buffered
// `/steer` line before the task can finish. A later turn proves that the
// instruction was projected into the model history.
const PLUGIN_SOURCE: &str = r#""use strict";
require("node:readline").createInterface({input:process.stdin,crlfDelay:Infinity}).on("line",line=>{
  const request=JSON.parse(line);
  const send=content=>process.stdout.write(JSON.stringify({
    protocol_version:request.protocol_version,request_id:request.request_id,content
  })+"\n");
  if(request.type!=="model_complete"){
    send(JSON.stringify({text:"summary",requests:1}));
    return;
  }
  const messages=request.arguments.messages||[];
  const steered=messages.some(message=>
    typeof message.content==="string"&&message.content.includes("KEEP_THIS_INSTRUCTION"));
  const assistant=text=>JSON.stringify({message:{role:"assistant",content:text}});
  if(steered){
    send(assistant("STEERING_APPLIED"));
    return;
  }
  setTimeout(()=>send(assistant("first answer")),1500);
});
"#;

fn isolated_dir(label: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "kroma-cli-steering-{label}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[test]
fn piped_steer_reaches_the_task_instead_of_being_dropped_before_activation() -> Result<()> {
    if Command::new("node").arg("--version").output().is_err() {
        return Ok(());
    }
    let temp = isolated_dir("piped")?;
    let project = temp.join("project");
    let plugins = temp.join("plugins");
    let plugin = plugins.join("steer-probe");
    fs::create_dir_all(&project)?;
    fs::create_dir_all(&plugin)?;
    fs::write(plugin.join("plugin.toml"), PLUGIN_MANIFEST)?;
    fs::write(plugin.join("main.js"), PLUGIN_SOURCE)?;
    PluginCatalog::discover_at(plugins.clone(), temp.join("plugins.toml"), &project)?
        .set_enabled("steer-probe", true)?;

    let mut child = Command::new(env!("CARGO_BIN_EXE_kroma"))
        .env("KROMA_WORKDIR", &project)
        .env("KROMA_PLUGIN_DIR", &plugins)
        .env("KROMA_CONFIG_FILE", temp.join("profiles.json"))
        .env("KROMA_SESSION_FILE", temp.join("session.json"))
        .env_remove("KROMA_MODEL")
        .env_remove("KROMA_BASE_URL")
        .env_remove("KROMA_API_KEY")
        .env_remove("KROMA_PROVIDER_PROFILE")
        .env_remove("KROMA_PLUGIN_TRY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning the kroma binary")?;
    {
        // Both lines exist in the pipe before the CLI reads any of them.
        let stdin = child.stdin.as_mut().context("child stdin")?;
        writeln!(stdin, "original task")?;
        writeln!(stdin, "/steer {STEER_TEXT}")?;
        stdin.flush()?;
    }
    drop(child.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("kroma did not exit within the test deadline");
        }
        sleep(Duration::from_millis(25));
    };
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .context("child stdout")?
        .read_to_string(&mut stdout)?;
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .context("child stderr")?
        .read_to_string(&mut stderr)?;

    assert!(
        stdout.contains("STEERING_APPLIED"),
        "the piped /steer instruction never reached the model; it must be applied or exit non-zero, never dropped silently (exit status {status})\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[steer] Accepted"),
        "the CLI did not accept the piped instruction while the task was running\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    fs::remove_dir_all(&temp)?;
    Ok(())
}

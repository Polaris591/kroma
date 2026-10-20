//! `codemode`: one tool that runs a short program instead of a dozen tool calls.
//!
//! The program is Python with a pre-bound, workspace-confined API (`read`, `write`, `grep`, `ls`).
//! Three properties are deliberate:
//!
//! * the API resolves every path under the workspace root and refuses escapes, so a snippet cannot
//!   read `/etc/passwd` by asking nicely;
//! * the tool is `SideEffect`, exactly like `bash`, so the same policy decision gates it — a
//!   different exposure must not become a different authorization;
//! * the program runs through the supervised bash executor (process group, deadline, output caps),
//!   so there is one execution path to reason about, not two.
//!
//! The program itself is handed to the interpreter on **stdin** (a quoted here-document), never as a
//! host path. That is what makes the same tool work in host and isolated mode: the sandbox only
//! maps the workspace, so a script staged in the trusted state directory does not exist in the guest
//! view — and mapping the whole state directory in to make a path work would hand the agent the
//! runtime's private files. The guest root is supplied by
//! [`crate::sandbox::ExecutionPolicy::guest_workspace_root`].

use serde::Deserialize;

/// Where the generated program reads its root from. Set by the dispatch arm, never by the model.
pub const ROOT_ENV: &str = "KROMA_CODEMODE_ROOT";

/// The prelude the model's code runs on top of.
///
/// Deliberately small: four functions that mirror the file tools. Anything else the model wants it
/// can write itself, but it cannot reach outside the root without tripping `_confine`.
const PRELUDE: &str = r#"import os, pathlib, re, sys
ROOT = pathlib.Path(os.environ["KROMA_CODEMODE_ROOT"]).resolve()

def _confine(path):
    candidate = (ROOT / path).resolve()
    if candidate != ROOT and ROOT not in candidate.parents:
        raise PermissionError(f"path escapes the workspace: {path}")
    return candidate

def read(path):
    return _confine(path).read_text()

def write(path, text):
    target = _confine(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text)
    return len(text)

def ls(path="."):
    return sorted(p.name for p in _confine(path).iterdir())

def grep(pattern, path="."):
    hits = []
    for entry in sorted(_confine(path).rglob("*")):
        if not entry.is_file():
            continue
        for number, line in enumerate(entry.read_text(errors="replace").splitlines(), 1):
            if re.search(pattern, line):
                hits.append(f"{entry.relative_to(ROOT)}:{number}:{line}")
    return hits
"#;

/// The `codemode` arguments, strictly: exactly the properties the tool schema declares.
///
/// Strict on purpose. A lenient parse (`value.get("code")`) silently ignores a caller's
/// `timeout_sec` and any unknown field, which is how a declared per-call limit became decorative and
/// how a malformed call could still execute. Rejecting unknown fields keeps the parser and the
/// model-visible schema the same contract.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeModeArgs {
    pub code: String,
    /// Per-call wall-clock limit in seconds (default 120), handed to the same supervised executor
    /// `bash` uses. `None` means the schema default, not "unbounded".
    pub timeout_sec: Option<u64>,
}

impl CodeModeArgs {
    /// The schema's declared default (`timeout_sec` default 120).
    pub const DEFAULT_TIMEOUT_SEC: u64 = 120;

    pub fn parse(raw_arguments: &str) -> Result<Self, String> {
        let args: Self =
            serde_json::from_str(raw_arguments).map_err(|e| format!("invalid JSON: {e}"))?;
        if args.code.trim().is_empty() {
            return Err("`code` must not be empty".to_string());
        }
        if args.timeout_sec == Some(0) {
            return Err("`timeout_sec` must be > 0".to_string());
        }
        Ok(args)
    }

    pub fn timeout_sec(&self) -> u64 {
        self.timeout_sec.unwrap_or(Self::DEFAULT_TIMEOUT_SEC)
    }
}

/// The command that runs one program under the kernel limits the run declared.
///
/// `ulimit` is used rather than a second spawn path: the program already runs through the supervised
/// bash executor, and the limits are a property of that child, not of a new executor. The program is
/// fed on stdin by a quoted here-document, so no host script path and no trusted state directory is
/// ever named to the guest, and the model's source cannot be reinterpreted by the shell.
pub fn program_command(root: &str, code: &str, cpu_seconds: u64, memory_bytes: u64) -> String {
    let memory_kib = (memory_bytes / 1024).max(1024);
    let body = program(code);
    let delimiter = heredoc_delimiter(&body);
    format!(
        "ulimit -t {cpu_seconds} && ulimit -v {memory_kib} && {}={} python3 - <<'{delimiter}'\n{body}{delimiter}",
        ROOT_ENV,
        shell_quote(root),
    )
}

/// A here-document delimiter no line of `body` equals, so a program that happens to contain the
/// default marker cannot terminate its own here-document early. Deterministic (no random source) so
/// the produced command stays reproducible in logs.
fn heredoc_delimiter(body: &str) -> String {
    let mut delimiter = "KROMA_CODEMODE_EOF".to_string();
    let mut suffix = 0u32;
    while body.lines().any(|line| line == delimiter) {
        suffix += 1;
        delimiter = format!("KROMA_CODEMODE_EOF_{suffix}");
    }
    delimiter
}

/// Single-quote a value for the shell. Only internal paths go through this, but a path with a
/// space or a quote must still arrive as one argument.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The complete program for one `codemode` call: prelude, then the model's code verbatim.
pub fn program(code: &str) -> String {
    format!("{PRELUDE}\n# --- model code ---\n{}\n", code.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn run(code: &str, root: &std::path::Path) -> std::process::Output {
        let script = root.join("program.py");
        fs::write(&script, program(code)).expect("write program");
        Command::new("python3")
            .arg(&script)
            .env(ROOT_ENV, root)
            .output()
            .expect("run program")
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kroma-codemode-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn a_program_runs_against_the_workspace_api() {
        let root = scratch("run");
        fs::write(root.join("data.txt"), "alpha\nbeta\n").unwrap();
        let out = run(
            "write('out/copy.txt', read('data.txt').upper())\nprint(ls('out'))\nprint(grep('beta'))",
            &root,
        );
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("copy.txt"), "{stdout}");
        assert!(stdout.contains("beta"), "{stdout}");
        assert_eq!(
            fs::read_to_string(root.join("out/copy.txt")).unwrap(),
            "ALPHA\nBETA\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_program_cannot_reach_outside_the_workspace() {
        let root = scratch("escape");
        let out = run("print(read('../../etc/passwd'))", &root);
        assert!(
            !out.status.success(),
            "an escaping read must fail, not silently pass"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("escapes the workspace"), "{stderr}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_tool_is_a_side_effect_like_bash_so_one_policy_gates_both() {
        let tools = crate::tool::definition::builtin_tool_registrations();
        let codemode = tools
            .iter()
            .find(|tool| tool.definition.name == "codemode")
            .expect("codemode is registered");
        assert_eq!(
            codemode.metadata.effect_class,
            crate::types::EffectClass::SideEffect,
            "exposure must not change authorization: codemode is gated exactly like bash"
        );
    }

    /// The produced command must run the program through stdin without staging a host path, and a
    /// program that contains the default here-document marker must not truncate itself.
    #[test]
    fn the_program_is_fed_on_stdin_and_cannot_end_its_own_heredoc() {
        let root = scratch("stdin");
        fs::write(root.join("data.txt"), "payload").unwrap();
        // The marker line is real program text but must not execute as a Python name: it sits in a
        // triple-quoted string, which is still a line the here-document scanner has to account for.
        let code = format!(
            "print(read('data.txt'))\ntext = \"\"\"\n{}\n\"\"\"\nprint('after-marker')",
            "KROMA_CODEMODE_EOF"
        );
        let command = program_command(&root.display().to_string(), &code, 5, 256 * 1024 * 1024);
        assert!(
            command.contains("python3 - <<"),
            "the program must be read from stdin, not from a host file: {command}"
        );
        assert!(
            !command.contains(&format!(
                "python3 {}",
                shell_quote(&root.display().to_string())
            )),
            "no host script path may be passed to the interpreter"
        );
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .output()
            .expect("run command");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("payload"), "{stdout}");
        assert!(stdout.contains("after-marker"), "{stdout}");
        let _ = fs::remove_dir_all(&root);
    }

    /// Strict parsing is the contract between the schema and the executor: an unknown field or a
    /// non-positive timeout is refused instead of being silently ignored.
    #[test]
    fn arguments_are_parsed_strictly() {
        assert!(CodeModeArgs::parse(r#"{"code":"print(1)"}"#).is_ok());
        assert_eq!(
            CodeModeArgs::parse(r#"{"code":"print(1)","timeout_sec":3}"#)
                .unwrap()
                .timeout_sec(),
            3
        );
        assert_eq!(
            CodeModeArgs::parse(r#"{"code":"print(1)"}"#)
                .unwrap()
                .timeout_sec(),
            CodeModeArgs::DEFAULT_TIMEOUT_SEC
        );
        assert!(
            CodeModeArgs::parse(r#"{"code":"print(1)","not_in_schema":true}"#).is_err(),
            "an unknown field must be refused, not ignored"
        );
        assert!(CodeModeArgs::parse(r#"{"code":"print(1)","timeout_sec":0}"#).is_err());
        assert!(CodeModeArgs::parse(r#"{"code":"print(1)","timeout_sec":-1}"#).is_err());
        assert!(CodeModeArgs::parse(r#"{"code":""}"#).is_err());
    }
}

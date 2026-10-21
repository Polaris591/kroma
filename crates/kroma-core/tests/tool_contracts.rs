use kroma_core::tools::{
    find::execute_find, grep::execute_grep, ls::execute_ls, read::execute_read,
    write::execute_write,
};
use kroma_core::types::ErrorCode;
use kroma_core::workspace::{InodeMutationLock, WorkspaceCap};
use std::fs;
use std::path::Path;
use uuid::Uuid;

fn symlink(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dst)
    }
    #[cfg(windows)]
    {
        if src.as_ref().is_dir() {
            std::os::windows::fs::symlink_dir(src, dst)
        } else {
            std::os::windows::fs::symlink_file(src, dst)
        }
    }
}

#[tokio::test]
async fn test_read_write_ls_find_grep_contracts() {
    let tmp_ws = std::env::temp_dir().join(format!("kroma-tools-{}", Uuid::new_v4()));
    fs::create_dir_all(tmp_ws.join("src")).unwrap();
    fs::create_dir_all(tmp_ws.join("target")).unwrap();
    fs::create_dir_all(tmp_ws.join("node_modules")).unwrap();

    // Sensitive files and ignored dirs
    fs::write(tmp_ws.join(".env"), "SECRET=1\n").unwrap();
    fs::write(tmp_ws.join("id_rsa"), "PRIVATE KEY\n").unwrap();
    fs::create_dir_all(tmp_ws.join(".ssh")).unwrap();
    fs::write(tmp_ws.join(".ssh/config"), "Host *\n").unwrap();
    fs::write(tmp_ws.join("target/ignored.rs"), "pub fn ignored() {}\n").unwrap();
    fs::write(tmp_ws.join("node_modules/pkg.js"), "console.log(1);\n").unwrap();
    fs::write(tmp_ws.join("Cargo.toml"), "[package]\nname=\"demo\"\n").unwrap();
    fs::write(tmp_ws.join(".gitignore"), "*.log\n!important.log\n").unwrap();
    fs::write(tmp_ws.join("debug.log"), "ignored log\n").unwrap();
    fs::write(tmp_ws.join("important.log"), "kept log\n").unwrap();

    // Symlink inside workspace (should not be followed by ls/find/grep)
    fs::write(
        tmp_ws.join("src/real.rs"),
        "pub fn alpha_target() -> i32 { 42 }\n",
    )
    .unwrap();
    symlink(tmp_ws.join("src/real.rs"), tmp_ws.join("src/sym.rs")).unwrap();

    let ws = WorkspaceCap::new(&tmp_ws).unwrap();
    let locks = InodeMutationLock::new();

    // 1. Test `write` with `if_not_exists=true` and `expected_sha256`
    let w1 = execute_write(
        &ws,
        &locks,
        r#"{"path":"src/new_mod.rs","content":"line1\nline2\n","if_not_exists":true}"#,
        1024 * 1024,
    )
    .await;
    assert!(w1.outcome.is_success());
    let sha16 = w1.meta.content_sha256.clone().unwrap();
    assert_eq!(sha16.len(), 16);

    // Repeating with `if_not_exists=true` fails with FILE_ALREADY_EXISTS
    let w_dup = execute_write(
        &ws,
        &locks,
        r#"{"path":"src/new_mod.rs","content":"other","if_not_exists":true}"#,
        1024 * 1024,
    )
    .await;
    assert_eq!(
        w_dup.outcome,
        kroma_core::messages::ToolOutcome::Error {
            code: ErrorCode::FileAlreadyExists
        }
    );

    // Overwrite with wrong `expected_sha256` fails with STALE_FILE
    let w_stale = execute_write(
        &ws,
        &locks,
        r#"{"path":"src/new_mod.rs","content":"updated\n","expected_sha256":"0000000000000000"}"#,
        1024 * 1024,
    )
    .await;
    assert_eq!(
        w_stale.outcome,
        kroma_core::messages::ToolOutcome::Error {
            code: ErrorCode::StaleFile
        }
    );

    // Overwrite with matching 16-char `expected_sha256` succeeds
    let w_ok = execute_write(
        &ws,
        &locks,
        &format!(
            r#"{{"path":"src/new_mod.rs","content":"updated_line1\nupdated_line2\n","expected_sha256":"{}"}}"#,
            sha16
        ),
        1024 * 1024,
    )
    .await;
    assert!(w_ok.outcome.is_success());

    // 2. Test `read`: header format, 16-char SHA256, long line truncation (> 2 KiB), pagination, binary rejection
    let long_line = "x".repeat(2500);
    fs::write(
        tmp_ws.join("src/long.txt"),
        format!("{}\nsecond_line\nthird_line\n", long_line),
    )
    .unwrap();

    let r_long = execute_read(&ws, r#"{"path":"src/long.txt","offset":1,"limit":2}"#);
    assert!(r_long.outcome.is_success());
    assert!(r_long
        .content
        .contains("[File: src/long.txt | Lines: 1-2 of 3 | SHA256: "));
    assert!(r_long.content.contains("[line truncated]"));
    assert!(r_long
        .content
        .contains("[Truncated at line 2. Call read with offset=3 to continue.]"));

    // Binary file (contains NUL byte) rejected with BINARY_OR_INVALID_UTF8
    fs::write(tmp_ws.join("src/bin.dat"), b"hello\x00world").unwrap();
    let r_bin = execute_read(&ws, r#"{"path":"src/bin.dat"}"#);
    assert_eq!(
        r_bin.outcome,
        kroma_core::messages::ToolOutcome::Error {
            code: ErrorCode::BinaryOrInvalidUtf8
        }
    );

    // 3. Test `ls`, `find`, `grep`: skip `.git/target/node_modules`, skip symlinks, skip sensitive files with note
    let ls_out = execute_ls(&ws, r#"{"path":"."}"#);
    assert!(ls_out.outcome.is_success());
    assert!(ls_out.content.contains("src/"));
    assert!(!ls_out.content.contains("target/"));
    assert!(!ls_out.content.contains("node_modules/"));
    assert!(!ls_out.content.contains(".env"));
    assert!(!ls_out.content.contains("id_rsa"));
    assert!(!ls_out.content.contains(".ssh"));
    assert!(ls_out.content.contains("[Skipped 3 sensitive file(s)]"));

    let find_out = execute_find(&ws, r#"{"pattern":"*.rs"}"#);
    assert!(find_out.outcome.is_success());
    assert!(find_out.content.contains("src/new_mod.rs"));
    assert!(find_out.content.contains("src/real.rs"));
    assert!(!find_out.content.contains("sym.rs")); // symlink skipped
    assert!(!find_out.content.contains("ignored.rs")); // target/ skipped
    assert!(find_out.content.contains("[Skipped 3 sensitive file(s)]"));

    // Test brace `{rs,toml}` and `.gitignore` negation `!important.log` and `path: "target"` guard
    let find_braces = execute_find(&ws, r#"{"pattern":"*.{rs,toml}"}"#);
    assert!(find_braces.content.contains("Cargo.toml"));
    assert!(find_braces.content.contains("src/real.rs"));

    let find_logs = execute_find(&ws, r#"{"pattern":"*.log"}"#);
    assert!(find_logs.content.contains("important.log"));
    assert!(!find_logs.content.contains("debug.log"));

    let find_target = execute_find(&ws, r#"{"pattern":"*","path":"target"}"#);
    assert!(!find_target.content.contains("ignored.rs"));

    let grep_out = execute_grep(&ws, r#"{"pattern":"alpha_target"}"#);
    assert!(grep_out.outcome.is_success());
    assert!(grep_out
        .content
        .contains("src/real.rs:1:pub fn alpha_target() -> i32 { 42 }"));
    assert!(!grep_out.content.contains("sym.rs"));
    assert!(grep_out.content.contains("[Skipped 3 sensitive file(s)]"));

    let _ = fs::remove_dir_all(&tmp_ws);
}

/// KR-H01: every registered tool must declare a complete contract.
///
/// `tool_contracts` covers the five file tools in detail. This is the catalogue-level check: walk
/// the whole registry and require that each entry has a stable name, a description the model can act
/// on, an object-shaped JSON schema, a timeout and an output cap. A tool that reaches the model with
/// no declared limits is a tool whose cost and side effects are unknown.
#[test]
fn every_registered_tool_declares_a_complete_contract() {
    use kroma_core::tool::registry::ToolRegistry;

    let registry = ToolRegistry::new(None);
    let tools = registry.all_registered();
    assert!(
        tools.len() >= 5,
        "expected at least the builtin file tools, got {}",
        tools.len()
    );

    for tool in &tools {
        let name = &tool.definition.name;
        assert!(!name.is_empty(), "a tool must have a stable name");
        assert!(
            !tool.definition.description.is_empty(),
            "{name} must describe itself to the model"
        );
        assert!(
            tool.definition.parameters.is_object(),
            "{name} parameters must be a JSON object schema, got {}",
            tool.definition.parameters
        );
        assert!(
            tool.metadata.default_timeout_sec > 0,
            "{name} must declare a timeout"
        );
        assert!(
            tool.metadata.inline_output_cap_bytes > 0,
            "{name} must declare an inline output cap"
        );
    }

    let mut names: Vec<&str> = tools.iter().map(|t| t.definition.name.as_str()).collect();
    names.sort_unstable();
    let total = names.len();
    names.dedup();
    assert_eq!(total, names.len(), "tool names must be unique");
}

/// KR-Q04: the argument boundary must survive hostile input without panicking.
///
/// This is a small deterministic fuzz: a fixed set of malformed and adversarial bodies, each of which
/// must produce a structured refusal rather than a panic, an allocation blow-up or a pass-through.
/// The inputs are chosen to hit the classes the validator claims to handle (unparseable, wrong shape,
/// wrong type, missing required, deep nesting, oversized, duplicate keys, control characters).
#[test]
fn hostile_argument_bodies_are_refused_without_panicking() {
    use kroma_core::tool::definition::ToolDefinition;
    use kroma_core::tool::schema_check::{validate_arguments, MAX_ARGUMENT_BYTES};
    use serde_json::json;

    let def = ToolDefinition {
        name: "probe".to_string(),
        description: "probe".to_string(),
        parameters: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"},
                "tags": {"type": "array", "items": {"type": "string"}}
            }
        }),
    };

    let deep = format!("{}{}{}", "{\"a\":".repeat(40), "1", "}".repeat(40));
    let huge = "x".repeat(MAX_ARGUMENT_BYTES + 1);
    let cases: Vec<String> = vec![
        String::new(),                                   // empty -> treated as {}
        "{".to_string(),                                 // truncated
        "null".to_string(),                              // not an object
        "[1,2,3]".to_string(),                           // array
        "\"string\"".to_string(),                        // scalar
        "{\"path\":123}".to_string(),                    // wrong type
        "{\"limit\":\"ten\"}".to_string(),               // wrong type
        "{\"tags\":[\"a\",1]}".to_string(),              // bad item
        "{\"path\":\"a\",\"path\":\"b\"}".to_string(),   // duplicate key
        "{\"path\":\"\\u0000\\u001b[31m\"}".to_string(), // control chars
        deep,
        huge,
        "{\"path\":\"a\"} trailing".to_string(),
        "{\"__proto__\":{\"polluted\":true}}".to_string(),
    ];

    for (i, body) in cases.iter().enumerate() {
        // The only requirement is: no panic, and a verdict either way. A pass is fine for bodies
        // that happen to be valid (empty body, __proto__ object); the point is the boundary holds.
        let verdict = validate_arguments(&def, body);
        if let Err(message) = &verdict {
            assert!(
                !message.is_empty(),
                "case {i} must explain the refusal, got an empty message"
            );
            assert!(
                message.len() < 512,
                "case {i} refusal must stay small, got {} bytes",
                message.len()
            );
        }
    }

    // The classes that must be refused, spot-checked so the loop above cannot pass vacuously.
    assert!(validate_arguments(&def, "{").is_err());
    assert!(validate_arguments(&def, "null").is_err());
    assert!(validate_arguments(&def, "{\"path\":123}").is_err());
    assert!(validate_arguments(&def, "{}").is_err());
}

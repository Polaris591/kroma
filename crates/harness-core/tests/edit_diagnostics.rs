use harness_core::messages::ToolOutcome;
use harness_core::tools::edit::execute_edit;
use harness_core::tools::read::execute_read;
use harness_core::types::{EditDiagKind, EditMatchTier, ErrorCode};
use harness_core::workspace::{InodeMutationLock, WorkspaceCap};
use std::fs;
use uuid::Uuid;

#[tokio::test]
async fn test_edit_tier1_tier2_bom_crlf_and_unchanged_lines_preservation() {
    let tmp_ws = std::env::temp_dir().join(format!("harness-edit-t12-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    // File with UTF-8 BOM, CRLF line endings, and trailing spaces on lines 1, 2, and 3
    let original = "\u{FEFF}fn keep_top() {   \r\n    let target = 1;   \r\n    let second = 2;\r\n    let keep_bottom = 3;\t\r\n}\r\n";
    let file_path = tmp_ws.join("lib.rs");
    fs::write(&file_path, original.as_bytes()).unwrap();

    let ws = WorkspaceCap::new(&tmp_ws).unwrap();
    let locks = InodeMutationLock::new();

    // Read to get 16-char SHA256
    let read_res = execute_read(&ws, r#"{"path":"lib.rs"}"#);
    let sha16 = read_res.meta.content_sha256.unwrap();

    // Edit lines 2-3 using `old_text` WITHOUT the trailing 3 spaces on line 2 -> Tier 1 fails, must match via Tier 2!
    let edit_args = serde_json::json!({
        "path": "lib.rs",
        "expected_sha256": sha16,
        "edits": [
            {
                "old_text": "    let target = 1;\n    let second = 2;",
                "new_text": "    let target = 42;\n    let second = 200;"
            }
        ]
    })
    .to_string();

    let edit_res = execute_edit(&ws, &locks, &edit_args, 2 * 1024 * 1024, 512 * 1024).await;
    assert_eq!(edit_res.outcome, ToolOutcome::Success);
    assert_eq!(
        edit_res.meta.edit_match_tier,
        Some(EditMatchTier::Tier2TrailingWhitespace)
    );

    let updated_bytes = fs::read(&file_path).unwrap();
    let updated_str = std::str::from_utf8(&updated_bytes).unwrap();

    // Verify UTF-8 BOM, CRLF line endings, and untouched lines' trailing spaces/tabs are 100% preserved!
    assert_eq!(
        updated_str,
        "\u{FEFF}fn keep_top() {   \r\n    let target = 42;\r\n    let second = 200;\r\n    let keep_bottom = 3;\t\r\n}\r\n"
    );

    // Verify 2-line CRLF file without trailing newline preserves CRLF dominant_eol
    let two_line_crlf = tmp_ws.join("two_line.txt");
    fs::write(&two_line_crlf, b"line1\r\nline2").unwrap();
    let edit_two_args = serde_json::json!({
        "path": "two_line.txt",
        "edits": [{
            "old_text": "line1\nline2",
            "new_text": "alpha\nbeta"
        }]
    })
    .to_string();
    let edit_two_res = execute_edit(&ws, &locks, &edit_two_args, 2 * 1024 * 1024, 512 * 1024).await;
    assert_eq!(edit_two_res.outcome, ToolOutcome::Success);
    assert_eq!(fs::read(&two_line_crlf).unwrap(), b"alpha\r\nbeta");

    let _ = fs::remove_dir_all(&tmp_ws);
}

#[tokio::test]
async fn test_edit_three_mismatch_diagnostics_and_all_or_none_rollback() {
    let tmp_ws = std::env::temp_dir().join(format!("harness-edit-diag-{}", Uuid::new_v4()));
    fs::create_dir_all(&tmp_ws).unwrap();

    let src = "\
pub struct Config {
    pub host: String,
    pub port: u16,
}

impl Config {
    pub fn new(host: String, port: u16) -> Self {
        let validated_port = if port == 0 { 8080 } else { port };
        Self {
            host,
            port: validated_port,
        }
    }
}
";
    let file_path = tmp_ws.join("config.rs");
    fs::write(&file_path, src).unwrap();

    let ws = WorkspaceCap::new(&tmp_ws).unwrap();
    let locks = InodeMutationLock::new();

    // 1. Diagnostic 1: Line number prefix pollution (`   7\t...`)
    let diag1_args = serde_json::json!({
        "path": "config.rs",
        "edits": [{
            "old_text": "    7\t    pub fn new(host: String, port: u16) -> Self {\n    8\t        let validated_port = if port == 0 { 8080 } else { port };",
            "new_text": "    pub fn new() {}"
        }]
    })
    .to_string();
    let res1 = execute_edit(&ws, &locks, &diag1_args, 2 * 1024 * 1024, 512 * 1024).await;
    assert_eq!(
        res1.outcome,
        ToolOutcome::Error {
            code: ErrorCode::MatchNotFound
        }
    );
    assert_eq!(res1.meta.diag_kind, Some(EditDiagKind::LineNumberPrefix));
    assert!(res1
        .content
        .contains("old_text appears to include line number prefixes from read output"));

    // 2. Diagnostic 2: Leading indentation mismatch (2-space indent in old_text vs 4/8-space indent in file)
    let diag2_args = serde_json::json!({
        "path": "config.rs",
        "edits": [{
            "old_text": "  pub fn new(host: String, port: u16) -> Self {\n    let validated_port = if port == 0 { 8080 } else { port };",
            "new_text": "    pub fn new(host: String, port: u16) -> Self {"
        }]
    })
    .to_string();
    let res2 = execute_edit(&ws, &locks, &diag2_args, 2 * 1024 * 1024, 512 * 1024).await;
    assert_eq!(
        res2.outcome,
        ToolOutcome::Error {
            code: ErrorCode::MatchNotFound
        }
    );
    assert_eq!(res2.meta.diag_kind, Some(EditDiagKind::IndentationMismatch));
    assert!(res2.content.contains("Indentation mismatch at lines 7-8"));
    assert!(res2
        .content
        .contains("file uses 4-space indent, whereas old_text used 2-space indent"));
    assert!(res2.content.contains("let validated_port = if port == 0"));

    // 3. Diagnostic 3: Closest candidate window recommendation (>= 50% similarity) + All-or-None rollback
    let diag3_args = serde_json::json!({
        "path": "config.rs",
        "edits": [
            {
                "old_text": "    pub host: String,",
                "new_text": "    pub host_name: String,"
            },
            {
                "old_text": "        let validated_port = if port == 0 { 9999 } else { port };",
                "new_text": "        let validated_port = port;"
            }
        ]
    })
    .to_string();
    let res3 = execute_edit(&ws, &locks, &diag3_args, 2 * 1024 * 1024, 512 * 1024).await;
    assert_eq!(
        res3.outcome,
        ToolOutcome::Error {
            code: ErrorCode::MatchNotFound
        }
    );
    assert_eq!(res3.meta.diag_kind, Some(EditDiagKind::ClosestCandidate));
    // Verify per-item indexed status: edits[0] is OK, edits[1] is MATCH_NOT_FOUND with closest candidate at line 8
    assert!(res3.content.contains("edits[0]: OK (lines 2-2)"));
    assert!(res3.content.contains("edits[1]: MATCH_NOT_FOUND"));
    assert!(res3.content.contains("Closest candidate at lines 8-8"));
    // Verify All-or-None: file on disk was NOT modified by edits[0]!
    assert_eq!(fs::read_to_string(&file_path).unwrap(), src);

    // 4. Stale file check returns current 16-char SHA256 and snippet
    let stale_args = serde_json::json!({
        "path": "config.rs",
        "expected_sha256": "ffffffffffffffff",
        "edits": [{
            "old_text": "    pub host: String,",
            "new_text": "    pub host: String,"
        }]
    })
    .to_string();
    let res_stale = execute_edit(&ws, &locks, &stale_args, 2 * 1024 * 1024, 512 * 1024).await;
    assert_eq!(
        res_stale.outcome,
        ToolOutcome::Error {
            code: ErrorCode::StaleFile
        }
    );
    assert!(res_stale.content.contains("Current file snippet"));

    // 5. `[line truncated]` marker guard
    let trunc_args = serde_json::json!({
        "path": "config.rs",
        "edits": [{
            "old_text": "    pub host: String,[line truncated]",
            "new_text": "    pub host: String,"
        }]
    })
    .to_string();
    let res_trunc = execute_edit(&ws, &locks, &trunc_args, 2 * 1024 * 1024, 512 * 1024).await;
    assert_eq!(
        res_trunc.outcome,
        ToolOutcome::Error {
            code: ErrorCode::LineTruncatedMarker
        }
    );

    let _ = fs::remove_dir_all(&tmp_ws);
}

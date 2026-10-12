use crate::types::EffectClass;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Model-visible tool schema definition (physically separated from host execution metadata).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Host-only execution metadata (never serialized into the model's tool schema).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolMetadata {
    pub effect_class: EffectClass,
    pub default_timeout_sec: u64,
    pub inline_output_cap_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub definition: ToolDefinition,
    pub metadata: ToolMetadata,
}

pub fn builtin_tool_registrations() -> Vec<RegisteredTool> {
    vec![
        RegisteredTool {
            definition: ToolDefinition {
                name: "bash".to_string(),
                description: "Execute a command in the workspace root using a supervised process group with 16 KiB Head + 48 KiB Rolling Tail capture.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to execute in workspace root."
                        },
                        "timeout_sec": {
                            "type": "integer",
                            "description": "Optional timeout in seconds (default 120)."
                        }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            },
            metadata: ToolMetadata {
                effect_class: EffectClass::SideEffect,
                default_timeout_sec: 120,
                inline_output_cap_bytes: 64 * 1024,
            },
        },
        RegisteredTool {
            definition: ToolDefinition {
                name: "edit".to_string(),
                description: "Apply 1-16 deterministic replacements to a workspace file against a single snapshot with Tier 1/2 matching and actionable mismatch diagnostics.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative file path."
                        },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_text": { "type": "string" },
                                    "new_text": { "type": "string" }
                                },
                                "required": ["old_text", "new_text"],
                                "additionalProperties": false
                            },
                            "minItems": 1,
                            "maxItems": 16
                        },
                        "expected_sha256": {
                            "type": "string",
                            "description": "Optional 16-char (or 64-char) hex SHA256 fingerprint from read."
                        }
                    },
                    "required": ["path", "edits"],
                    "additionalProperties": false
                }),
            },
            metadata: ToolMetadata {
                effect_class: EffectClass::Mutating,
                default_timeout_sec: 30,
                inline_output_cap_bytes: 64 * 1024,
            },
        },
        RegisteredTool {
            definition: ToolDefinition {
                name: "find".to_string(),
                description: "Find files in the workspace matching a glob pattern without following symlinks.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match (e.g. '*.rs', 'src/**/*.rs')."
                        },
                        "path": {
                            "type": "string",
                            "description": "Optional workspace-relative subdirectory (default '.')."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max entries to return (default 200, max 200)."
                        }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
            },
            metadata: ToolMetadata {
                effect_class: EffectClass::ReadOnly,
                default_timeout_sec: 30,
                inline_output_cap_bytes: 32 * 1024,
            },
        },
        RegisteredTool {
            definition: ToolDefinition {
                name: "grep".to_string(),
                description: "Search file contents in the workspace using a bounded regular expression without following symlinks.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regular expression pattern."
                        },
                        "path": {
                            "type": "string",
                            "description": "Optional workspace-relative file or directory path (default '.')."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max matches to return (default 200, max 200)."
                        }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
            },
            metadata: ToolMetadata {
                effect_class: EffectClass::ReadOnly,
                default_timeout_sec: 30,
                inline_output_cap_bytes: 32 * 1024,
            },
        },
        RegisteredTool {
            definition: ToolDefinition {
                name: "ls".to_string(),
                description: "List directory entries in deterministic lexicographical order without following symlinks.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative directory path (default '.')."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max entries to return (default 200, max 200)."
                        }
                    },
                    "additionalProperties": false
                }),
            },
            metadata: ToolMetadata {
                effect_class: EffectClass::ReadOnly,
                default_timeout_sec: 30,
                inline_output_cap_bytes: 32 * 1024,
            },
        },
        RegisteredTool {
            definition: ToolDefinition {
                name: "read".to_string(),
                description: "Read a workspace text file with bounded streaming, line numbers, and 16-char SHA256 fingerprint.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative file path."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "1-indexed starting line number (default 1)."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum lines to read (default 2000, max 2000)."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
            metadata: ToolMetadata {
                effect_class: EffectClass::ReadOnly,
                default_timeout_sec: 30,
                inline_output_cap_bytes: 64 * 1024,
            },
        },
        RegisteredTool {
            definition: ToolDefinition {
                name: "recall_evidence".to_string(),
                description: "Recall the raw tool output behind a compacted Outline Stub by its source_entry_id (or the original call_id), optionally filtered to a line range or a case-insensitive pattern.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "source_entry_id": {
                            "type": "string",
                            "description": "The source_entry_id shown in a [Compacted: ...] stub header, or the original tool call_id."
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "Optional 1-based first line of the raw output to return (default 1)."
                        },
                        "end_line": {
                            "type": "integer",
                            "description": "Optional 1-based last line to return (capped at start_line + 199)."
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Optional case-insensitive substring filter; matching lines are returned with 2 lines of context."
                        }
                    },
                    "required": ["source_entry_id"],
                    "additionalProperties": false
                }),
            },
            metadata: ToolMetadata {
                effect_class: EffectClass::ReadOnly,
                default_timeout_sec: 30,
                inline_output_cap_bytes: 64 * 1024,
            },
        },
        RegisteredTool {
            definition: ToolDefinition {
                name: "write".to_string(),
                description: "Atomically write or create a workspace file via same-directory random temporary file.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative file path."
                        },
                        "content": {
                            "type": "string",
                            "description": "UTF-8 file content to write (max 1 MiB)."
                        },
                        "expected_sha256": {
                            "type": "string",
                            "description": "Optional 16-char (or 64-char) hex SHA256 check when overwriting."
                        },
                        "if_not_exists": {
                            "type": "boolean",
                            "description": "If true, atomically fail with FILE_ALREADY_EXISTS if file already exists."
                        }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            },
            metadata: ToolMetadata {
                effect_class: EffectClass::Mutating,
                default_timeout_sec: 30,
                inline_output_cap_bytes: 64 * 1024,
            },
        },
    ]
}

pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod recall_evidence;
pub mod write;

use crate::messages::{ToolOutcome, ToolResultMeta};
use crate::types::ErrorCode;

#[derive(Debug, Clone)]
pub struct ToolExecutionOutput {
    pub outcome: ToolOutcome,
    pub content: String,
    pub meta: ToolResultMeta,
}

impl ToolExecutionOutput {
    pub fn success(content: String, meta: ToolResultMeta) -> Self {
        Self {
            outcome: ToolOutcome::Success,
            content,
            meta,
        }
    }

    pub fn error(code: ErrorCode, content: String, meta: ToolResultMeta) -> Self {
        Self {
            outcome: ToolOutcome::Error { code },
            content: if content.starts_with(code.as_str()) {
                content
            } else {
                format!("{}: {}", code.as_str(), content)
            },
            meta,
        }
    }
}

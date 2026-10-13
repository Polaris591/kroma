pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod recall_evidence;
pub mod write;

use crate::messages::{ImageAttachment, ToolOutcome, ToolResultMeta};
use crate::types::ErrorCode;

#[derive(Debug, Clone)]
pub struct ToolExecutionOutput {
    pub outcome: ToolOutcome,
    pub content: String,
    pub meta: ToolResultMeta,
    /// Gate 7 (`v1.2.0`): binary images produced by this tool call (multimodal `read`).
    pub image_attachments: Vec<ImageAttachment>,
}

impl ToolExecutionOutput {
    pub fn success(content: String, meta: ToolResultMeta) -> Self {
        Self {
            outcome: ToolOutcome::Success,
            content,
            meta,
            image_attachments: Vec::new(),
        }
    }

    /// Gate 7: a successful result that also carries multimodal image payloads.
    pub fn success_with_images(
        content: String,
        meta: ToolResultMeta,
        images: Vec<ImageAttachment>,
    ) -> Self {
        Self {
            outcome: ToolOutcome::Success,
            content,
            meta,
            image_attachments: images,
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
            image_attachments: Vec::new(),
        }
    }
}

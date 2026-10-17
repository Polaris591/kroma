use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub Uuid);

impl RunId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ToolCallId(pub String);

impl ToolCallId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn is_valid(&self) -> bool {
        !self.0.trim().is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventSeq(pub u64);

impl EventSeq {
    pub fn new(seq: u64) -> Self {
        Self(seq)
    }

    /// Advance the counter and return the new value. Not named `next`: this mutates and returns a
    /// counter directly rather than yielding iterator items.
    pub fn advance(&mut self) -> Self {
        self.0 += 1;
        *self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Created,
    Running,
    WaitingForApproval,
    CompletionGateCheck,
    Completed,
    Failed,
    Cancelled,
    BudgetExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalReason {
    ModelStop,
    MaxTurns,
    MaxToolCalls,
    MaxRequestAttempts,
    ContextBudgetExceeded,
    RunDeadlineExceeded,
    /// F08 (`v1.4.0`): the run stopped because the *token* budget (`--max-tokens`) was exhausted.
    /// Distinct from [`TerminalReason::MaxTurns`] so callers can tell a turn-count stop from a
    /// spend stop.
    TokenBudgetExhausted,
    ProviderProtocolError(String),
    UserCancelled,
    StrictLogFailure(String),
}

impl TerminalReason {
    pub fn to_status(&self) -> RunStatus {
        match self {
            TerminalReason::ModelStop => RunStatus::Completed,
            TerminalReason::MaxTurns
            | TerminalReason::MaxToolCalls
            | TerminalReason::MaxRequestAttempts
            | TerminalReason::ContextBudgetExceeded
            | TerminalReason::RunDeadlineExceeded
            | TerminalReason::TokenBudgetExhausted => RunStatus::BudgetExhausted,
            TerminalReason::ProviderProtocolError(_) | TerminalReason::StrictLogFailure(_) => {
                RunStatus::Failed
            }
            TerminalReason::UserCancelled => RunStatus::Cancelled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    TruncatedToolCall,
    InvalidToolBatch,
    PolicyDenied,
    /// T09: the run required a sandbox for this spawn and the backend is unavailable. Fail-closed:
    /// the command was NOT run on the host instead.
    SandboxUnavailable,
    SensitivePathBlocked,
    PathEscapeBlocked,
    SymlinkBlocked,
    BinaryOrInvalidUtf8,
    StaleFile,
    FileAlreadyExists,
    FileNotFound,
    MatchNotFound,
    MatchNotUnique,
    OverlappingEdits,
    LineTruncatedMarker,
    RepeatedCallBlocked,
    PingPongCycleBlocked,
    EditFailureStreakBlocked,
    OutputFloodCap,
    ToolTimeout,
    NotExecutedCancelled,
    NotExecutedBudgetExhausted,
    InvalidArguments,
    UnknownTool,
    IoError,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::TruncatedToolCall => "TRUNCATED_TOOL_CALL",
            ErrorCode::InvalidToolBatch => "INVALID_TOOL_BATCH",
            ErrorCode::PolicyDenied => "POLICY_DENIED",
            ErrorCode::SandboxUnavailable => "SANDBOX_UNAVAILABLE",
            ErrorCode::SensitivePathBlocked => "SENSITIVE_PATH_BLOCKED",
            ErrorCode::PathEscapeBlocked => "PATH_ESCAPE_BLOCKED",
            ErrorCode::SymlinkBlocked => "SYMLINK_BLOCKED",
            ErrorCode::BinaryOrInvalidUtf8 => "BINARY_OR_INVALID_UTF8",
            ErrorCode::StaleFile => "STALE_FILE",
            ErrorCode::FileAlreadyExists => "FILE_ALREADY_EXISTS",
            ErrorCode::FileNotFound => "FILE_NOT_FOUND",
            ErrorCode::MatchNotFound => "MATCH_NOT_FOUND",
            ErrorCode::MatchNotUnique => "MATCH_NOT_UNIQUE",
            ErrorCode::OverlappingEdits => "OVERLAPPING_EDITS",
            ErrorCode::LineTruncatedMarker => "LINE_TRUNCATED_MARKER",
            ErrorCode::RepeatedCallBlocked => "REPEATED_CALL_BLOCKED",
            ErrorCode::PingPongCycleBlocked => "PING_PONG_CYCLE_BLOCKED",
            ErrorCode::EditFailureStreakBlocked => "EDIT_FAILURE_STREAK_BLOCKED",
            ErrorCode::OutputFloodCap => "OUTPUT_FLOOD_CAP",
            ErrorCode::ToolTimeout => "TOOL_TIMEOUT",
            ErrorCode::NotExecutedCancelled => "NOT_EXECUTED_CANCELLED",
            ErrorCode::NotExecutedBudgetExhausted => "NOT_EXECUTED_BUDGET_EXHAUSTED",
            ErrorCode::InvalidArguments => "INVALID_ARGUMENTS",
            ErrorCode::UnknownTool => "UNKNOWN_TOOL",
            ErrorCode::IoError => "IO_ERROR",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    ReadOnly,
    Mutating,
    SideEffect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditMatchTier {
    Tier1Exact,
    Tier2TrailingWhitespace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditDiagKind {
    LineNumberPrefix,
    IndentationMismatch,
    ClosestCandidate,
    MatchNotUnique,
    StaleFile,
    LineTruncatedMarker,
    OverlappingEdits,
}

use crate::types::RunId;
use crate::workspace::{InodeMutationLock, WorkspaceCap, WorkspaceError};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

/// Abstract execution environment decoupling filesystem & process spawning (§3.2 V6-07).
pub trait ExecutionEnv: Send + Sync {
    fn workspace(&self) -> &WorkspaceCap;
    fn inode_locks(&self) -> &InodeMutationLock;
    fn state_dir(&self) -> &Path;
}

/// Default local host environment backed by `WorkspaceCap` and Unix process groups.
#[derive(Debug, Clone)]
pub struct LocalHostEnv {
    workspace: WorkspaceCap,
    inode_locks: InodeMutationLock,
    state_dir: PathBuf,
}

impl LocalHostEnv {
    pub fn new(
        workspace_root: impl AsRef<Path>,
        state_dir: impl Into<PathBuf>,
    ) -> Result<Self, WorkspaceError> {
        let workspace = WorkspaceCap::new(workspace_root)?;
        Ok(Self {
            workspace,
            inode_locks: InodeMutationLock::new(),
            state_dir: state_dir.into(),
        })
    }
}

impl ExecutionEnv for LocalHostEnv {
    fn workspace(&self) -> &WorkspaceCap {
        &self.workspace
    }

    fn inode_locks(&self) -> &InodeMutationLock {
        &self.inode_locks
    }

    fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

#[derive(Debug, Clone)]
pub struct CompletionGateContext {
    pub run_id: RunId,
    pub workspace_root: PathBuf,
    pub modified_files: Vec<String>,
    pub turns_used: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    Approved,
    /// Verification was intentionally skipped because no supported command was discoverable.
    Skipped { reason: String },
    Rejected { feedback: String },
}

/// Pre-terminal completion gate executed when model stops after modifying files (§3.2 V6-07).
pub trait CompletionGate: Send + Sync {
    fn check<'a>(
        &'a self,
        ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>>;
}

#[derive(Debug, Clone, Default)]
pub struct AllowAllGate;

impl CompletionGate for AllowAllGate {
    fn check<'a>(
        &'a self,
        _ctx: &'a CompletionGateContext,
    ) -> Pin<Box<dyn Future<Output = Result<GateOutcome, String>> + Send + 'a>> {
        Box::pin(async { Ok(GateOutcome::Approved) })
    }
}

pub type SharedExecutionEnv = Arc<dyn ExecutionEnv>;
pub type SharedCompletionGate = Arc<dyn CompletionGate>;

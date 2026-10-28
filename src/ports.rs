//! A context strategy may commit its own state, not replace raw history.
//! The runtime binds the immutable authoritative messages to the committer.
use crate::context_state::PersistedContext;
use anyhow::Result;

pub trait ContextCommit: Send + Sync {
    fn commit(&self, state: &PersistedContext) -> Result<()>;
}

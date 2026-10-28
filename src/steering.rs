//! Small runtime input inbox: steer the active task or enqueue future user
//! turns. A single shared handle works while Agent::run borrows the agent.
//! No background worker, extra model request or plugin runtime.
use anyhow::{bail, Result};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Inbox {
    active: bool,
    steering: VecDeque<String>,
    followups: VecDeque<String>,
}

#[derive(Clone, Default)]
pub struct TaskInput {
    inner: Arc<Mutex<Inbox>>,
}

pub(crate) struct ActiveTask(TaskInput);

impl Drop for ActiveTask {
    fn drop(&mut self) {
        let mut inbox = self.0.inner.lock().unwrap();
        inbox.active = false;
        // A cancelled/failed task must not leak instructions into a new task.
        inbox.steering.clear();
    }
}

impl TaskInput {
    pub(crate) fn start(&self) -> Result<ActiveTask> {
        let mut inbox = self.inner.lock().unwrap();
        if inbox.active {
            bail!("another Kroma task is still active");
        }
        inbox.active = true;
        Ok(ActiveTask(self.clone()))
    }

    /// Deliver an instruction to the current task at the next safe model
    /// boundary, after all returned tool calls have been accounted for.
    pub fn steer(&self, instruction: impl Into<String>) -> Result<()> {
        let text = instruction.into();
        if text.trim().is_empty() {
            bail!("steering instruction cannot be empty");
        }
        let mut inbox = self.inner.lock().unwrap();
        if !inbox.active {
            bail!("no active task to steer");
        }
        inbox.steering.push_back(text);
        Ok(())
    }

    pub(crate) fn take_steering(&self) -> VecDeque<String> {
        std::mem::take(
            &mut self
                .inner
                .lock()
                .expect("steering inbox lock poisoned")
                .steering,
        )
    }

    pub(crate) fn restore_steering(&self, mut pending: VecDeque<String>) {
        let mut inbox = self.inner.lock().unwrap();
        pending.append(&mut inbox.steering);
        inbox.steering = pending;
    }

    /// Atomically seal a completed task. Once sealed, a late steering input
    /// is rejected rather than silently dropped during shutdown.
    pub(crate) fn finish_if_no_steering(&self) -> bool {
        let mut inbox = self.inner.lock().unwrap();
        if !inbox.steering.is_empty() {
            return false;
        }
        inbox.active = false;
        true
    }

    pub fn pending_steering(&self) -> usize {
        self.inner.lock().unwrap().steering.len()
    }

    /// Queue a new task to be run by the CLI/host after the current task.
    /// Follow-ups are not folded into the currently running model request.
    pub fn queue_followup(&self, instruction: impl Into<String>) -> Result<usize> {
        let text = instruction.into();
        if text.trim().is_empty() {
            bail!("queued task cannot be empty");
        }
        let mut inbox = self.inner.lock().unwrap();
        inbox.followups.push_back(text);
        Ok(inbox.followups.len())
    }

    pub fn queued(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .followups
            .iter()
            .cloned()
            .collect()
    }

    pub fn take_followup(&self) -> Option<String> {
        self.inner.lock().unwrap().followups.pop_front()
    }

    pub fn clear_followups(&self) -> usize {
        let mut inbox = self.inner.lock().unwrap();
        let count = inbox.followups.len();
        inbox.followups.clear();
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_clears_steering_but_preserves_queued_tasks() {
        let inbox = TaskInput::default();
        assert!(inbox.steer("orphan").is_err());
        let active = inbox.start().unwrap();
        inbox.steer("change approach").unwrap();
        assert_eq!(inbox.queue_followup("new task").unwrap(), 1);
        assert_eq!(inbox.pending_steering(), 1);
        drop(active);
        assert_eq!(inbox.pending_steering(), 0);
        assert_eq!(inbox.take_followup().as_deref(), Some("new task"));
        assert!(inbox.steer("orphan").is_err());
    }

    #[test]
    fn sealing_is_atomic_and_steering_keeps_order() {
        let inbox = TaskInput::default();
        let _active = inbox.start().unwrap();
        inbox.steer("first").unwrap();
        inbox.steer("second").unwrap();
        assert!(!inbox.finish_if_no_steering());
        let mut pending = inbox.take_steering();
        assert_eq!(pending.pop_front().as_deref(), Some("first"));
        inbox.steer("third").unwrap();
        inbox.restore_steering(pending);
        assert_eq!(
            inbox.take_steering().into_iter().collect::<Vec<_>>(),
            vec!["second", "third"]
        );
        assert!(inbox.finish_if_no_steering());
        assert!(inbox.steer("late").is_err());
    }
}

//! Bounded delivery for synchronous runtime notifications. Accounting is
//! aggregated separately; presentation overload pauses live transcript updates
//! until the next turn. The host reloads the durable transcript at turn end.
use super::runtime_events::UiEvent;
use crate::{model::Usage, usage::UsageMeter};
use std::{
    collections::VecDeque,
    mem::size_of,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, Semaphore};

const MAX_EVENTS: usize = 2048;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 8 * 1024;
const TITLE_EVENTS: usize = 32;
const TITLE_TASKS: usize = 16;
const MAX_SESSION_ID: usize = 1024;

type Stamped = (u64, UiEvent);

#[derive(Debug)]
pub(super) struct SendError;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TryRecvError {
    Empty,
    Disconnected,
}

pub(super) fn channel() -> (EventSender, EventReceiver) {
    let (titles, title_rx) = mpsc::channel(TITLE_EVENTS);
    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        ready: Notify::new(),
        closed: AtomicBool::new(false),
        senders: AtomicUsize::new(1),
        title_tasks: Arc::new(Semaphore::new(TITLE_TASKS)),
    });
    (
        EventSender {
            shared: shared.clone(),
            titles,
        },
        EventReceiver {
            shared,
            titles: title_rx,
        },
    )
}

struct Shared {
    state: Mutex<State>,
    ready: Notify,
    closed: AtomicBool,
    senders: AtomicUsize,
    title_tasks: Arc<Semaphore>,
}

pub(super) struct EventSender {
    shared: Arc<Shared>,
    titles: mpsc::Sender<UiEvent>,
}

impl Clone for EventSender {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
            titles: self.titles.clone(),
        }
    }
}

impl Drop for EventSender {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.ready.notify_one();
        }
    }
}

impl EventSender {
    /// Never wait for the consumer. Only the separate title task may use the
    /// asynchronous reliable lane, whose capacity is independent of live output.
    pub(super) fn send(&self, event: UiEvent) -> Result<(), SendError> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(SendError);
        }
        if is_title(&event) {
            if event_bytes(&event) > title_event_limit() {
                return Err(SendError);
            }
            return self.titles.try_send(event).map_err(|_| SendError);
        }
        let mut state = self
            .shared
            .state
            .lock()
            .expect("TUI event queue lock poisoned");
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(SendError);
        }
        state.push(event);
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }

    pub(super) async fn send_title(&self, event: UiEvent) -> Result<(), SendError> {
        if !is_title(&event) || event_bytes(&event) > title_event_limit() {
            return Err(SendError);
        }
        self.titles.send(event).await.map_err(|_| SendError)
    }

    /// Acquire before starting any charged request, not after producing a
    /// result. Waiting title senders therefore cannot grow without a limit.
    pub(super) fn title_permit(&self, id: &str) -> Option<OwnedSemaphorePermit> {
        if self.shared.closed.load(Ordering::Acquire) || id.len() > MAX_SESSION_ID {
            return None;
        }
        self.shared.title_tasks.clone().try_acquire_owned().ok()
    }
}

pub(super) struct EventReceiver {
    shared: Arc<Shared>,
    titles: mpsc::Receiver<UiEvent>,
}

impl EventReceiver {
    pub(super) fn try_recv(&mut self) -> Result<UiEvent, TryRecvError> {
        let disconnected = self.shared.senders.load(Ordering::Acquire) == 0;
        if let Ok(event) = self.titles.try_recv() {
            return Ok(event);
        }
        if let Some(event) = self
            .shared
            .state
            .lock()
            .expect("TUI event queue lock poisoned")
            .pop()
        {
            return Ok(event);
        }
        if disconnected {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub(super) async fn recv(&mut self) -> Option<UiEvent> {
        let shared = self.shared.clone();
        loop {
            // Register before inspecting either lane: cancellation or a send
            // between the empty check and await must not lose the wakeup.
            let ready = shared.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            match self.try_recv() {
                Ok(event) => return Some(event),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {}
            }
            tokio::select! {
                _ = ready => {},
                event = self.titles.recv() => {
                    if let Some(event) = event { return Some(event); }
                }
            }
        }
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.title_tasks.close();
        self.titles.close();
        *self
            .shared
            .state
            .lock()
            .expect("TUI event queue lock poisoned") = State::default();
    }
}

#[derive(Default)]
struct State {
    queue: VecDeque<Stamped>,
    bytes: usize,
    sequence: u64,
    usage: Option<(u64, UsageMeter)>,
    apec: Option<(u64, UsageMeter)>,
    context: Option<Stamped>,
    git: Option<Stamped>,
    started: Option<Stamped>,
    phase: Option<Stamped>,
    finished: Option<Stamped>,
    notice: Option<Stamped>,
    overloaded: bool,
}

impl State {
    fn push(&mut self, event: UiEvent) {
        self.sequence = self.sequence.saturating_add(1);
        let sequence = self.sequence;
        match event {
            UiEvent::Usage {
                input,
                output,
                cached,
                reasoning,
            } => {
                let (_, meter) = self
                    .usage
                    .get_or_insert_with(|| (sequence, UsageMeter::default()));
                meter.record(Some(&Usage {
                    input,
                    output,
                    cached,
                    reasoning,
                    total: None,
                    web_searches: None,
                }));
            }
            UiEvent::ApecUsage {
                requests,
                input,
                output,
                cached,
            } => {
                let (_, meter) = self
                    .apec
                    .get_or_insert_with(|| (sequence, UsageMeter::default()));
                meter.record_batch(
                    requests,
                    Some(&Usage {
                        input,
                        output,
                        cached,
                        reasoning: None,
                        total: None,
                        web_searches: None,
                    }),
                );
            }
            UiEvent::UsageBatch(batch) => {
                let (_, meter) = self
                    .usage
                    .get_or_insert_with(|| (sequence, UsageMeter::default()));
                meter.absorb(&batch);
            }
            UiEvent::ApecUsageBatch(batch) => {
                let (_, meter) = self
                    .apec
                    .get_or_insert_with(|| (sequence, UsageMeter::default()));
                meter.absorb(&batch);
            }
            UiEvent::Context { .. } | UiEvent::ActualInput(_) => {
                self.context = Some((sequence, event))
            }
            UiEvent::GitInfo { .. } => {
                if event_bytes(&event) <= MAX_METADATA_BYTES {
                    self.git = Some((sequence, event));
                }
            }
            UiEvent::TaskStarted => {
                self.overloaded = false;
                self.started = Some((sequence, event));
            }
            UiEvent::CompactionStarted | UiEvent::ModelStarted => {
                self.phase = Some((sequence, event))
            }
            UiEvent::TaskFinished => self.finished = Some((sequence, event)),
            _ if self.overloaded => {}
            _ => {
                let bytes = event_bytes(&event);
                if self.queue.len() >= MAX_EVENTS
                    || bytes > (MAX_BYTES - MAX_METADATA_BYTES).saturating_sub(self.bytes)
                {
                    self.overloaded = true;
                    self.notice = Some((sequence, UiEvent::BacklogOverflow));
                } else {
                    self.bytes += bytes;
                    self.queue.push_back((sequence, event));
                }
            }
        }
    }

    fn pop(&mut self) -> Option<UiEvent> {
        // Fixed-size accounting/status slots join the FIFO at their original
        // position. Coalescing cannot move a task's final idle state ahead of
        // its remaining transcript prefix.
        let slots = [
            self.queue.front().map(|v| v.0),
            self.usage.as_ref().map(|v| v.0),
            self.apec.as_ref().map(|v| v.0),
            self.context.as_ref().map(|v| v.0),
            self.started.as_ref().map(|v| v.0),
            self.phase.as_ref().map(|v| v.0),
            self.finished.as_ref().map(|v| v.0),
            self.notice.as_ref().map(|v| v.0),
            self.git.as_ref().map(|v| v.0),
        ];
        let lane = slots
            .iter()
            .enumerate()
            .filter_map(|(i, seq)| seq.map(|seq| (i, seq)))
            .min_by_key(|(_, seq)| *seq)?
            .0;
        match lane {
            0 => {
                let (_, event) = self.queue.pop_front().expect("selected FIFO is nonempty");
                self.bytes -= event_bytes(&event);
                Some(event)
            }
            1 => self
                .usage
                .take()
                .map(|(_, meter)| UiEvent::UsageBatch(meter)),
            2 => self
                .apec
                .take()
                .map(|(_, meter)| UiEvent::ApecUsageBatch(meter)),
            3 => self.context.take().map(|(_, event)| event),
            4 => self.started.take().map(|(_, event)| event),
            5 => self.phase.take().map(|(_, event)| event),
            6 => self.finished.take().map(|(_, event)| event),
            7 => self.notice.take().map(|(_, event)| event),
            8 => self.git.take().map(|(_, event)| event),
            _ => unreachable!("the lane is selected from fixed slots"),
        }
    }
}

fn is_title(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::TitleUsage { .. } | UiEvent::SessionTitleUpdated(_)
    )
}

fn title_event_limit() -> usize {
    size_of::<Stamped>() + MAX_SESSION_ID
}

fn event_bytes(event: &UiEvent) -> usize {
    size_of::<Stamped>()
        + match event {
            UiEvent::Warning(s)
            | UiEvent::Text(s)
            | UiEvent::ReasoningDelta(s)
            | UiEvent::ReasoningFinal(s)
            | UiEvent::SessionTitleUpdated(s) => s.capacity(),
            UiEvent::ToolStarted { name, id, args } => {
                name.capacity() + id.capacity() + args.capacity()
            }
            UiEvent::ToolFinished { id, result, .. } => id.capacity() + result.capacity(),
            UiEvent::ToolOutput { id, stream, chunk } => {
                id.capacity() + stream.capacity() + chunk.capacity()
            }
            UiEvent::TitleUsage { session_id, .. } => session_id.capacity(),
            UiEvent::GitInfo { session, info, .. } => {
                session.capacity() + info.branch.as_ref().map_or(0, String::capacity)
            }
            _ => 0,
        }
}

#[cfg(test)]
#[path = "event_queue_tests.rs"]
mod tests;

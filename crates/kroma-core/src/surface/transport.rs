//! Bounded stream transport and lifecycle enforcement.
//!
//! Enforces bounded outbound queues, writer deadlines, authentication timeouts,
//! idle connection reclamation, and bounded task tracking across single-session
//! and multi-session daemon streams.
//!
//! # Invariants
//! - Outbound memory is bounded by item count (`max_outbound_items = 256`) and
//!   retained serialized payload bytes (`max_outbound_bytes = 4 MiB`).
//!   Note: upper bound applies to retained serialized payload bytes (`String::len() + 1`
//!   for trailing newline), not whole-process heap RSS.
//!   Individual frames are additionally bounded by `max_frame_bytes = 2 MiB`.
//! - RAII reservations guarantee that items and bytes are decremented exactly once
//!   on any exit path (normal send, queue drop, writer write, timeout, or fail-close).
//! - All subscriber and ACP approval callbacks use non-blocking synchronous sends.
//!   On queue saturation or byte overflow, the transport marks the connection as
//!   `TransportState::Aborted` sticky state and disconnects without fallback to unbounded queues.
//! - Strict linearization: state transitions (`Open -> Closing -> Aborted`) are atomically
//!   serialized under an admission lock. Calling `initiate_graceful_close` fixes a single
//!   absolute deadline and prevents subsequent sends. Calling `mark_fail_closed` immediately
//!   aborts pending writes, drains unwritten messages, and releases all RAII reservations.
//! - Persistent `FrameReader` ensures cancel-safety in `tokio::select!` reading loops
//!   when prompt tasks complete and join concurrently with partial stream reads.

use super::{JsonRpcError, JSONRPC_INVALID_PARAMS};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

/// Default upper bound on queued outbound messages (including the item currently in-flight in writer).
pub const MAX_OUTBOUND_ITEMS: usize = 256;

/// Default upper bound on serialized payload retained bytes in outbound queue (4 MiB).
pub const MAX_OUTBOUND_BYTES: usize = 4 * 1024 * 1024;

/// Default upper bound on a single outbound JSON-RPC frame (2 MiB).
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// Single absolute deadline covering write of payload, newline, and flush.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);

/// Absolute timeout from connection start to authenticate.
pub const AUTH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum consecutive failed authentication attempts before connection is terminated.
pub const MAX_AUTH_FAILURES: usize = 3;

/// Idle timeout for quiet connections without active runs.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Transport lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportState {
    /// Connection is open and accepting outbound frames.
    Open,
    /// Graceful closing has started. Inbound reading and new producers are stopped.
    /// Remaining queued messages are flushed best-effort up to `deadline`.
    Closing { deadline: tokio::time::Instant },
    /// Connection is aborted (due to queue overflow, write error/timeout, or explicit abort).
    /// Writer immediately purges all queued messages to release RAII reservations.
    Aborted,
}

/// Configurable transport limits, enabling deterministic testing with short deadlines.
#[derive(Clone, Copy, Debug)]
pub struct TransportConfig {
    pub max_outbound_items: usize,
    pub max_outbound_bytes: usize,
    pub max_frame_bytes: usize,
    pub write_deadline: Duration,
    pub auth_handshake_timeout: Duration,
    pub max_auth_failures: usize,
    pub idle_timeout: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            max_outbound_items: MAX_OUTBOUND_ITEMS,
            max_outbound_bytes: MAX_OUTBOUND_BYTES,
            max_frame_bytes: MAX_FRAME_BYTES,
            write_deadline: WRITE_DEADLINE,
            auth_handshake_timeout: AUTH_HANDSHAKE_TIMEOUT,
            max_auth_failures: MAX_AUTH_FAILURES,
            idle_timeout: IDLE_TIMEOUT,
        }
    }
}

/// Persistent, cancellation-safe frame buffer for incoming JSON-RPC streams.
///
/// Unlike local buffers allocated inside a read future, `FrameReader` is retained
/// across `tokio::select!` branch cancellations, ensuring that partially received
/// frames are not dropped when a background run completes and is reaped via `join_next`.
pub struct FrameReader {
    buf: Vec<u8>,
    max_bytes: usize,
}

impl FrameReader {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_bytes,
        }
    }

    /// Read one newline-delimited frame, preserving partial bytes across cancellations.
    pub async fn read_frame<R: AsyncBufRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> Result<Option<String>, String> {
        loop {
            let available = reader
                .fill_buf()
                .await
                .map_err(|e| format!("stream read error: {}", e))?;
            if available.is_empty() {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let line_bytes = std::mem::take(&mut self.buf);
                let s = String::from_utf8(line_bytes)
                    .map_err(|_| "frame is not valid UTF-8".to_string())?;
                return Ok(Some(s));
            }
            if let Some(newline_pos) = available.iter().position(|&b| b == b'\n') {
                let take_len = newline_pos + 1;
                if self.buf.len().saturating_add(take_len) > self.max_bytes {
                    reader.consume(take_len);
                    self.buf.clear();
                    return Err(format!(
                        "JSON-RPC frame exceeds maximum size of {} bytes",
                        self.max_bytes
                    ));
                }
                self.buf.extend_from_slice(&available[..newline_pos]);
                reader.consume(take_len);
                let line_bytes = std::mem::take(&mut self.buf);
                let s = String::from_utf8(line_bytes)
                    .map_err(|_| "frame is not valid UTF-8".to_string())?;
                return Ok(Some(s));
            } else {
                let chunk_len = available.len();
                if self.buf.len().saturating_add(chunk_len) > self.max_bytes {
                    reader.consume(chunk_len);
                    self.buf.clear();
                    return Err(format!(
                        "JSON-RPC frame exceeds maximum size of {} bytes",
                        self.max_bytes
                    ));
                }
                self.buf.extend_from_slice(available);
                reader.consume(chunk_len);
            }
        }
    }
}

#[cfg(test)]
pub struct TestPostReserveGate {
    enter_tx: std::sync::mpsc::SyncSender<()>,
    release_rx: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
impl TestPostReserveGate {
    pub fn new() -> (Arc<Self>, std::sync::mpsc::Receiver<()>, std::sync::mpsc::SyncSender<()>) {
        let (enter_tx, enter_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let gate = Arc::new(Self {
            enter_tx,
            release_rx: std::sync::Mutex::new(release_rx),
        });
        (gate, enter_rx, release_tx)
    }

    pub fn notify_and_wait(&self) {
        let _ = self.enter_tx.send(());
        if let Ok(rx) = self.release_rx.lock() {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }
}

/// Shared state tracking outbound queue bounds, admission synchronization, and lifecycle state.
pub struct OutboundState {
    current_items: AtomicUsize,
    current_bytes: AtomicUsize,
    state_tx: watch::Sender<TransportState>,
    admission_lock: std::sync::Mutex<()>,
    config: TransportConfig,
    #[cfg(test)]
    test_post_reserve_gate: std::sync::RwLock<Option<Arc<TestPostReserveGate>>>,
}

impl OutboundState {
    pub fn new(state_tx: watch::Sender<TransportState>, config: TransportConfig) -> Self {
        Self {
            current_items: AtomicUsize::new(0),
            current_bytes: AtomicUsize::new(0),
            state_tx,
            admission_lock: std::sync::Mutex::new(()),
            config,
            #[cfg(test)]
            test_post_reserve_gate: std::sync::RwLock::new(None),
        }
    }

    /// Read current lifecycle state without locking.
    #[allow(dead_code)]
    pub fn state(&self) -> TransportState {
        *self.state_tx.borrow()
    }

    /// True if the transport has entered Aborted state.
    #[allow(dead_code)]
    pub fn is_fail_closed(&self) -> bool {
        matches!(*self.state_tx.borrow(), TransportState::Aborted)
    }

    /// True if the transport is closing or aborted (rejecting new sends).
    pub fn is_closing_or_aborted(&self) -> bool {
        !matches!(*self.state_tx.borrow(), TransportState::Open)
    }

    /// Immediately mark transport as Aborted.
    /// Any state -> Aborted transition is atomic under `admission_lock`.
    pub fn mark_fail_closed(&self) {
        let _guard = self.admission_lock.lock().unwrap();
        self.state_tx.send_modify(|s| {
            *s = TransportState::Aborted;
        });
    }

    /// Initiate graceful close with a single, absolute deadline.
    /// Transition `Open -> Closing { deadline }` is atomic under `admission_lock`.
    /// Repeated calls do not extend the deadline, and `Aborted` state cannot be overwritten.
    pub fn initiate_graceful_close(&self) -> Option<tokio::time::Instant> {
        let _guard = self.admission_lock.lock().unwrap();
        let mut fixed_deadline = None;
        self.state_tx.send_modify(|s| {
            if let TransportState::Open = *s {
                let dl = tokio::time::Instant::now() + self.config.write_deadline;
                *s = TransportState::Closing { deadline: dl };
                fixed_deadline = Some(dl);
            }
        });
        fixed_deadline
    }

    #[allow(dead_code)]
    pub fn current_items(&self) -> usize {
        self.current_items.load(Ordering::SeqCst)
    }

    #[allow(dead_code)]
    pub fn current_bytes(&self) -> usize {
        self.current_bytes.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub fn set_test_post_reserve_gate(&self, gate: Arc<TestPostReserveGate>) {
        *self.test_post_reserve_gate.write().unwrap() = Some(gate);
    }
}

/// RAII reservation guard. Automatically releases item and byte accounting when dropped.
pub struct OutboundReservation {
    state: Arc<OutboundState>,
    bytes: usize,
}

impl Drop for OutboundReservation {
    fn drop(&mut self) {
        self.state.current_items.fetch_sub(1, Ordering::SeqCst);
        self.state.current_bytes.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

/// One outbound message carrying its RAII budget reservation.
pub struct OutboundMsg {
    pub payload: String,
    pub _reservation: OutboundReservation,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OutboundError {
    FailClosed,
    ItemCapacityExceeded,
    ByteCapacityExceeded,
    FrameTooLarge,
    ChannelClosed,
}

/// Bounded outbound sender cloneable across subscriber callbacks and request handlers.
#[derive(Clone)]
pub struct BoundedOutboundSender {
    tx: mpsc::Sender<OutboundMsg>,
    state: Arc<OutboundState>,
    state_rx: watch::Receiver<TransportState>,
}

impl BoundedOutboundSender {
    #[allow(dead_code)]
    pub fn is_fail_closed(&self) -> bool {
        self.state.is_fail_closed()
    }

    pub fn mark_fail_closed(&self) {
        self.state.mark_fail_closed();
    }

    /// Initiate graceful closing with an absolute deadline fixed at closing initiation.
    pub fn initiate_graceful_close(&self) -> Option<tokio::time::Instant> {
        self.state.initiate_graceful_close()
    }

    #[allow(dead_code)]
    pub fn subscribe_state(&self) -> watch::Receiver<TransportState> {
        self.state_rx.clone()
    }

    /// Synchronous, non-blocking send. Enforces item, total byte, and single frame limits.
    /// Strictly linearized: admission check and enqueue are serialized under `admission_lock`.
    pub fn try_send_sync(&self, payload: String) -> Result<(), OutboundError> {
        if self.state.is_closing_or_aborted() {
            return Err(OutboundError::FailClosed);
        }

        let line_bytes = payload.len().saturating_add(1); // include trailing newline byte
        if line_bytes > self.state.config.max_frame_bytes {
            return Err(OutboundError::FrameTooLarge);
        }

        // Atomic check & reserve for item count
        let mut curr_items = self.state.current_items.load(Ordering::Acquire);
        loop {
            if curr_items >= self.state.config.max_outbound_items {
                self.mark_fail_closed();
                return Err(OutboundError::ItemCapacityExceeded);
            }
            match self.state.current_items.compare_exchange_weak(
                curr_items,
                curr_items + 1,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => curr_items = actual,
            }
        }

        // Atomic check & reserve for byte count
        let mut curr_bytes = self.state.current_bytes.load(Ordering::Acquire);
        loop {
            if curr_bytes.saturating_add(line_bytes) > self.state.config.max_outbound_bytes {
                self.state.current_items.fetch_sub(1, Ordering::SeqCst);
                self.mark_fail_closed();
                return Err(OutboundError::ByteCapacityExceeded);
            }
            match self.state.current_bytes.compare_exchange_weak(
                curr_bytes,
                curr_bytes + line_bytes,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => curr_bytes = actual,
            }
        }

        let reservation = OutboundReservation {
            state: self.state.clone(),
            bytes: line_bytes,
        };

        #[cfg(test)]
        {
            let gate_opt = self.state.test_post_reserve_gate.read().unwrap().clone();
            if let Some(gate) = gate_opt {
                gate.notify_and_wait();
            }
        }

        // Strict admission linearization under admission_lock:
        // Ensures close initiation and message enqueue cannot race to leave an orphaned message.
        let send_res = {
            let _guard = self.state.admission_lock.lock().unwrap();
            if self.state.is_closing_or_aborted() {
                drop(reservation);
                return Err(OutboundError::FailClosed);
            }
            self.tx.try_send(OutboundMsg {
                payload,
                _reservation: reservation,
            })
        };

        match send_res {
            Ok(()) => Ok(()),
            Err(_) => {
                // Channel closed: reservation drops here, decrementing items & bytes automatically
                self.mark_fail_closed();
                Err(OutboundError::ChannelClosed)
            }
        }
    }
}

/// Create a bounded outbound queue and writer setup.
pub fn bounded_outbound_channel(
    config: TransportConfig,
) -> (
    BoundedOutboundSender,
    mpsc::Receiver<OutboundMsg>,
    watch::Receiver<TransportState>,
    Arc<OutboundState>,
) {
    let (tx, rx) = mpsc::channel(config.max_outbound_items);
    let (state_tx, state_rx) = watch::channel(TransportState::Open);
    let state = Arc::new(OutboundState::new(state_tx, config));
    let sender = BoundedOutboundSender {
        tx,
        state: state.clone(),
        state_rx: state_rx.clone(),
    };
    (sender, rx, state_rx, state)
}

/// Drive outbound stream writer with bounded deadlines and prompt failure abortion.
///
/// Distinguishes normal EOF/graceful shutdown (flushing remaining queue within an absolute closing
/// deadline fixed at closing start) from failure stop (`TransportState::Aborted`), where pending
/// messages and in-flight writes are immediately aborted and dropped to release all RAII reservations.
pub async fn run_bounded_writer<W>(
    mut rx: mpsc::Receiver<OutboundMsg>,
    mut writer: W,
    state: Arc<OutboundState>,
    mut state_rx: watch::Receiver<TransportState>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    loop {
        // Immediate failure check: if aborted, close channel, drain remaining messages, and exit
        if matches!(*state_rx.borrow(), TransportState::Aborted) {
            rx.close();
            while let Ok(msg) = rx.try_recv() {
                drop(msg);
            }
            break;
        }

        // Check if graceful close deadline has expired
        if let TransportState::Closing { deadline } = *state_rx.borrow() {
            rx.close();
            if tokio::time::Instant::now() >= deadline {
                state.mark_fail_closed();
                while let Ok(msg) = rx.try_recv() {
                    drop(msg);
                }
                break;
            }
        }

        tokio::select! {
            changed = state_rx.changed() => {
                if changed.is_err() {
                    // State sender dropped; continue loop
                }
                // Loop re-evaluates top check
            }
            msg_opt = rx.recv() => {
                let Some(msg) = msg_opt else {
                    // Normal channel close and queue drained: all accepted messages sent cleanly
                    break;
                };

                let payload = &msg.payload;
                let write_fut = async {
                    writer.write_all(payload.as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await?;
                    Ok::<(), std::io::Error>(())
                };
                tokio::pin!(write_fut);

                let frame_deadline = tokio::time::Instant::now() + state.config.write_deadline;
                let mut write_failed = false;

                loop {
                    let now = tokio::time::Instant::now();
                    let current_deadline = match *state_rx.borrow() {
                        TransportState::Aborted => {
                            write_failed = true;
                            break;
                        }
                        TransportState::Closing { deadline } => {
                            frame_deadline.min(deadline)
                        }
                        TransportState::Open => frame_deadline,
                    };

                    if now >= current_deadline {
                        state.mark_fail_closed();
                        write_failed = true;
                        break;
                    }

                    tokio::select! {
                        res = tokio::time::timeout_at(current_deadline, &mut write_fut) => {
                            match res {
                                Ok(Ok(())) => {
                                    // Successfully written and flushed
                                    break;
                                }
                                Ok(Err(_io_err)) => {
                                    state.mark_fail_closed();
                                    write_failed = true;
                                    break;
                                }
                                Err(_timeout) => {
                                    state.mark_fail_closed();
                                    write_failed = true;
                                    break;
                                }
                            }
                        }
                        changed = state_rx.changed() => {
                            if changed.is_err() {
                                // Ignore
                            }
                            // Re-evaluate deadline in loop without restarting write_fut
                        }
                    }
                }

                drop(msg);

                if write_failed {
                    rx.close();
                    while let Ok(m) = rx.try_recv() {
                        drop(m);
                    }
                    break;
                }
            }
        }
    }
}

/// Normalize JSON-RPC request parameters.
/// - If `params` is null or omitted, normalizes to an empty object `{}` and inserts `session_id`.
/// - If `params` is an object, ensures `session_id` is present (or keeps explicit session_id/sessionId).
/// - If `params` is an array or primitive scalar, rejects with `JSONRPC_INVALID_PARAMS`.
pub fn normalize_request_params(
    params: &mut Value,
    target_sid: &str,
) -> Result<(), JsonRpcError> {
    if params.is_null() {
        let mut map = serde_json::Map::new();
        map.insert("session_id".to_string(), Value::String(target_sid.to_string()));
        *params = Value::Object(map);
        Ok(())
    } else if let Value::Object(ref mut map) = params {
        if !map.contains_key("session_id") && !map.contains_key("sessionId") {
            map.insert("session_id".to_string(), Value::String(target_sid.to_string()));
        }
        Ok(())
    } else {
        Err(JsonRpcError::new(
            JSONRPC_INVALID_PARAMS,
            "params must be a JSON object or omitted",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_outbound_queue_item_limit_overflow_fail_closes() {
        let (sender, mut rx, state_rx, state) = bounded_outbound_channel(TransportConfig::default());
        assert!(!sender.is_fail_closed());
        assert_eq!(state.current_items(), 0);

        // Fill to capacity
        for i in 0..MAX_OUTBOUND_ITEMS {
            assert_eq!(
                sender.try_send_sync(format!("msg-{}", i)),
                Ok(()),
                "filling within capacity should succeed"
            );
        }
        assert_eq!(state.current_items(), MAX_OUTBOUND_ITEMS);
        assert!(!sender.is_fail_closed());

        // 257th item must fail with ItemCapacityExceeded and trigger fail-closed
        let overflow_res = sender.try_send_sync("overflow".to_string());
        assert_eq!(overflow_res, Err(OutboundError::ItemCapacityExceeded));
        assert!(sender.is_fail_closed(), "overflow must mark sender fail-closed");
        assert_eq!(*state_rx.borrow(), TransportState::Aborted);

        // Subsequent sends must return FailClosed
        assert_eq!(sender.try_send_sync("subsequent".to_string()), Err(OutboundError::FailClosed));

        // Draining/dropping messages releases capacity automatically via RAII
        let mut drained = 0;
        while let Ok(msg) = rx.try_recv() {
            drop(msg);
            drained += 1;
        }
        assert_eq!(drained, MAX_OUTBOUND_ITEMS);
        assert_eq!(state.current_items(), 0, "RAII reservations must release item count to 0");
        assert_eq!(state.current_bytes(), 0, "RAII reservations must release byte count to 0");
    }

    #[tokio::test]
    async fn test_outbound_queue_byte_limit_overflow_fail_closes() {
        let (sender, _rx, state_rx, _state) = bounded_outbound_channel(TransportConfig::default());
        assert!(!sender.is_fail_closed());

        // Payload exceeding single frame bound
        let single_huge = "x".repeat(MAX_FRAME_BYTES + 10);
        assert_eq!(sender.try_send_sync(single_huge), Err(OutboundError::FrameTooLarge));

        // Total bytes exceeding 4 MiB across multiple frames (each < 2 MiB)
        let frame_size = 1024 * 1024; // 1 MiB
        for _ in 0..3 {
            assert!(sender.try_send_sync("y".repeat(frame_size)).is_ok());
        }
        // 4th frame (another 1 MiB + 4 newline bytes total) pushes it over 4 MiB total capacity
        assert!(sender.try_send_sync("y".repeat(frame_size + 10)).is_err());
        assert!(sender.is_fail_closed(), "byte overflow must mark sender fail-closed");
        assert_eq!(*state_rx.borrow(), TransportState::Aborted);
    }

    #[tokio::test]
    async fn test_graceful_close_flushes_all_queued_items_beyond_eight() {
        // Deterministic test: >=9 (12) queued items must ALL be delivered cleanly without arbitrary 8-item cap
        let (sender, rx, state_rx, state) = bounded_outbound_channel(TransportConfig::default());

        let total_items = 12;
        for i in 0..total_items {
            sender.try_send_sync(format!("msg-{}", i)).unwrap();
        }
        assert_eq!(state.current_items(), total_items);
        assert!(state.current_bytes() > 0);

        // Initiate graceful closing: stops new producers and fixes closing deadline
        let deadline = sender.initiate_graceful_close();
        assert!(deadline.is_some());
        assert!(matches!(state.state(), TransportState::Closing { .. }));

        // Any new send must be rejected immediately
        assert_eq!(sender.try_send_sync("rejected".to_string()), Err(OutboundError::FailClosed));

        let (client_r, server_w) = tokio::io::duplex(4096);
        let writer_task = tokio::spawn(run_bounded_writer(rx, server_w, state.clone(), state_rx));

        let mut lines = tokio::io::BufReader::new(client_r).lines();
        for i in 0..total_items {
            let line = lines.next_line().await.unwrap().expect("must receive frame");
            assert_eq!(line, format!("msg-{}", i));
        }

        // After all items are flushed, stream should close cleanly (EOF)
        let eof = lines.next_line().await.unwrap();
        assert!(eof.is_none(), "stream must EOF cleanly after graceful flush");

        writer_task.await.unwrap();

        // Budget reservations must return strictly to 0
        assert_eq!(state.current_items(), 0, "all items must be accounted to 0");
        assert_eq!(state.current_bytes(), 0, "all bytes must be accounted to 0");
    }

    #[tokio::test]
    async fn test_graceful_close_repeated_does_not_extend_deadline() {
        let (sender, _rx, _state_rx, state) = bounded_outbound_channel(TransportConfig::default());

        let d1 = sender.initiate_graceful_close().expect("first call sets deadline");
        tokio::time::sleep(Duration::from_millis(20)).await;

        let d2 = sender.initiate_graceful_close();
        assert!(d2.is_none(), "repeated close must not set a new deadline");

        if let TransportState::Closing { deadline } = state.state() {
            assert_eq!(deadline, d1, "deadline must remain fixed at first initiation");
        } else {
            panic!("state must be Closing");
        }
    }

    #[tokio::test]
    async fn test_abort_cannot_be_revived_by_close() {
        let (sender, _rx, _state_rx, state) = bounded_outbound_channel(TransportConfig::default());

        sender.mark_fail_closed();
        assert_eq!(state.state(), TransportState::Aborted);

        let res = sender.initiate_graceful_close();
        assert!(res.is_none());
        assert_eq!(state.state(), TransportState::Aborted, "abort must not be overwritten by close");
    }

    #[tokio::test]
    async fn test_writer_exits_promptly_when_already_aborted() {
        let (sender, rx, state_rx, state) = bounded_outbound_channel(TransportConfig::default());

        sender.mark_fail_closed();
        let (_client_r, server_w) = tokio::io::duplex(64);
        let writer_task = tokio::spawn(run_bounded_writer(rx, server_w, state.clone(), state_rx));

        tokio::time::timeout(Duration::from_millis(200), writer_task)
            .await
            .expect("writer must exit promptly when already aborted")
            .unwrap();
    }

    #[tokio::test]
    async fn test_writer_in_flight_write_aborts_immediately_on_fail_closed() {
        let (sender, rx, state_rx, state) = bounded_outbound_channel(TransportConfig::default());

        // Queue 5 messages
        for i in 0..5 {
            sender.try_send_sync(format!("pending-{}", i)).unwrap();
        }
        assert_eq!(state.current_items(), 5);
        assert!(state.current_bytes() > 0);

        // Blackhole writer with 1-byte duplex so writer stalls on write/flush
        let (_client_r, server_w) = tokio::io::duplex(1);
        let writer_task = tokio::spawn(run_bounded_writer(rx, server_w, state.clone(), state_rx));

        // Let writer begin writing
        tokio::time::sleep(Duration::from_millis(25)).await;

        // Trigger fail_closed
        sender.mark_fail_closed();

        // Writer must abort and exit promptly
        tokio::time::timeout(Duration::from_millis(500), writer_task)
            .await
            .expect("writer must abort promptly on fail_closed")
            .unwrap();

        // All reservations must be released to 0
        assert_eq!(state.current_items(), 0, "all items must be dropped on fail_closed");
        assert_eq!(state.current_bytes(), 0, "all bytes must be dropped on fail_closed");
    }

    #[tokio::test]
    async fn test_close_deadline_includes_in_flight_writes() {
        let config = TransportConfig {
            write_deadline: Duration::from_millis(80),
            ..Default::default()
        };
        let (sender, rx, state_rx, state) = bounded_outbound_channel(config);

        sender.try_send_sync("slow-msg-1".to_string()).unwrap();
        sender.try_send_sync("slow-msg-2".to_string()).unwrap();

        // Initiate close at T0 (deadline = T0 + 80ms)
        sender.initiate_graceful_close();

        // Duplex that is never read by client
        let (_client_r, server_w) = tokio::io::duplex(1);
        let writer_task = tokio::spawn(run_bounded_writer(rx, server_w, state.clone(), state_rx));

        // Writer must timeout within 80ms + slack, marking fail-closed and dropping all
        let start = tokio::time::Instant::now();
        tokio::time::timeout(Duration::from_millis(300), writer_task)
            .await
            .expect("writer must timeout within close deadline")
            .unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(250), "must not exceed close deadline window");

        assert_eq!(state.state(), TransportState::Aborted);
        assert_eq!(state.current_items(), 0);
        assert_eq!(state.current_bytes(), 0);
    }

    #[test]
    fn test_concurrent_send_post_reserve_race_linearization() {
        // Deterministic post-reserve race validation using instance-level TestPostReserveGate
        // Sync send runs on an independent OS thread.
        let (sender, _rx, _state_rx, state) = bounded_outbound_channel(TransportConfig::default());
        let (gate, enter_rx, release_tx) = TestPostReserveGate::new();
        state.set_test_post_reserve_gate(gate);

        let sender_clone = sender.clone();
        let thread_handle = std::thread::spawn(move || {
            sender_clone.try_send_sync("concurrent-race-msg".to_string())
        });

        // 1. Wait until sender has atomically reserved item/byte budget and paused at gate
        enter_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("sender thread must enter post-reserve gate");

        // Sender holds reservation right now
        assert_eq!(state.current_items(), 1);
        assert!(state.current_bytes() > 0);

        // 2. Mark fail-closed concurrently while sender is between reservation and admission lock
        sender.mark_fail_closed();

        // 3. Release gate so sender resumes and enters admission lock
        release_tx.send(()).expect("must release gate");

        // 4. Sender thread must reject with FailClosed and decrement reservations to 0
        let send_res = thread_handle.join().expect("sender thread should not panic");
        assert_eq!(send_res, Err(OutboundError::FailClosed));

        assert_eq!(state.current_items(), 0, "reservation must be released on post-reserve abort");
        assert_eq!(state.current_bytes(), 0, "bytes must be released on post-reserve abort");
    }

    #[tokio::test]
    async fn test_frame_reader_cancellation_and_resumption() {
        // Unit test: partial frame read cancelled then resumed without byte loss
        let mut reader = FrameReader::new(1024);
        let (client_r, mut server_w) = tokio::io::duplex(256);
        let mut buf_reader = tokio::io::BufReader::new(client_r);

        // Write first half without newline
        server_w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1").await.unwrap();
        server_w.flush().await.unwrap();

        // Polling read_frame once yields pending (simulated with short timeout)
        let first_attempt = tokio::time::timeout(
            Duration::from_millis(20),
            reader.read_frame(&mut buf_reader),
        ).await;
        assert!(first_attempt.is_err(), "partial frame must yield pending without newline");

        // Write second half with newline
        server_w.write_all(b",\"method\":\"ping\"}\n").await.unwrap();
        server_w.flush().await.unwrap();

        // Resume read_frame: must return full concatenated frame
        let frame = reader.read_frame(&mut buf_reader).await.unwrap().expect("must yield complete frame");
        assert_eq!(frame, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}");
    }

    #[tokio::test]
    async fn test_frame_reader_corrupted_second_half_fails_fast() {
        // Mutation assertion: if first half was missing or corrupted, reading remaining frame
        // immediately parses as malformed JSON without hanging or waiting for timeout.
        let mut reader = FrameReader::new(1024);
        let (client_r, mut server_w) = tokio::io::duplex(256);
        let mut buf_reader = tokio::io::BufReader::new(client_r);

        // Write only the corrupted tail of a frame
        server_w.write_all(b"status\",\"params\":{}}\n").await.unwrap();
        server_w.flush().await.unwrap();

        let frame = reader.read_frame(&mut buf_reader).await.unwrap().expect("line returned");
        // Deserializing this raw tail fails immediately with JSON parse error
        let parse_res: Result<Value, _> = serde_json::from_str(&frame);
        assert!(parse_res.is_err(), "corrupted tail must produce JSON parse error immediately");
    }
}

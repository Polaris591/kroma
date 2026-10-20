//! Process-local admission shared by all built-in wire protocols for one validated endpoint.
//! Credentials never enter the key. Different models/protocols on the same endpoint share its pool.
//! Separate OS processes still need an external gateway limit; this is not a distributed quota.
use super::endpoint::ValidatedEndpoint;
use super::ProviderError;
use crate::channel_governor::{ChannelGovernor, ChannelPermit, ChannelPolicy, GovernorError};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;

type EndpointKey = (String, String, u16, String);
static CHANNELS: OnceLock<Mutex<HashMap<EndpointKey, Arc<ChannelGovernor>>>> = OnceLock::new();
// Provider reconfiguration must not grow an immortal per-endpoint registry without bound. Idle,
// healthy entries may be reclaimed; open breakers survive client reconstruction through cooldown.
const MAX_CHANNELS: usize = 256;

pub(super) fn shared_channel(
    endpoint: &ValidatedEndpoint,
) -> Result<Arc<ChannelGovernor>, ProviderError> {
    let base = endpoint.base_path.trim_end_matches('/');
    let base = ["/chat/completions", "/messages", "/responses"]
        .iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .unwrap_or(base);
    let base = if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    };
    // The validated wire authority is also the pool identity. Local aliases must not create a
    // second limiter for the same loopback endpoint.
    let host = endpoint.host.to_ascii_lowercase();
    let host = if matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1") {
        "loopback".to_string()
    } else {
        host
    };
    let key = (endpoint.scheme.clone(), host, endpoint.port, base);
    let mut channels = CHANNELS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(channel) = channels.get(&key) {
        return Ok(Arc::clone(channel));
    }
    if channels.len() >= MAX_CHANNELS {
        channels.retain(|_, channel| {
            let health = channel.snapshot();
            Arc::strong_count(channel) > 1
                || health.in_flight > 0
                || (health.state != "closed" && health.retry_after != Some(Duration::ZERO))
        });
        if channels.len() >= MAX_CHANNELS {
            return Err(ProviderError::NotDispatched(Box::new(
                ProviderError::Transport(
                    "process channel registry is full of live or cooling channels".into(),
                ),
            )));
        }
    }
    // Provider max_retries and the run's remaining attempts remain authoritative. Their existing
    // retry count is not expanded by adding channel admission.
    let channel = Arc::new(ChannelGovernor::new(ChannelPolicy {
        base_backoff: Duration::from_millis(50),
        max_attempts: usize::MAX,
        ..ChannelPolicy::serial()
    }));
    channels.insert(key, Arc::clone(&channel));
    Ok(channel)
}

pub(super) async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub(super) async fn admit<'a>(
    channel: &'a ChannelGovernor,
    cancel: &mut watch::Receiver<bool>,
    timeout: Duration,
    attempts_used: usize,
) -> Result<ChannelPermit<'a>, ProviderError> {
    let result = tokio::select! {
        biased;
        _ = cancelled(cancel) => Err(ProviderError::Cancelled),
        _ = tokio::time::sleep(timeout) => Err(ProviderError::Timeout),
        permit = channel.acquire() => permit.map_err(|error| match error {
            GovernorError::ChannelOpen { retry_after } => ProviderError::ChannelUnavailable {
                retry_after_ms: retry_after.as_millis().min(u128::from(u64::MAX)) as u64,
            },
        }),
    };
    result.map_err(|error| {
        if attempts_used == 0 {
            ProviderError::NotDispatched(Box::new(error))
        } else {
            error
        }
    })
}

pub(super) fn record_outcome(channel: &ChannelGovernor, error: Option<&ProviderError>) {
    match error {
        None => channel.record_success(),
        // Health failures only. A user cancellation, malformed model content or a per-request
        // authorization rejection must not poison unrelated users' otherwise healthy channel.
        Some(
            ProviderError::HttpStatus {
                status: 429 | 500 | 502 | 503 | 504,
                ..
            }
            | ProviderError::Transport(_)
            | ProviderError::Timeout
            | ProviderError::IncompleteStream(_),
        ) => {
            channel.record_failure();
        }
        Some(ProviderError::HttpStatus { .. } | ProviderError::RedirectBlocked(_)) => {
            channel.record_success();
        }
        _ => {}
    }
}

/// Retry-After cannot shorten local backoff. If it exceeds the bounded waiting policy, refuse
/// instead of retrying earlier than the server requested. Waiting is cancellable and bounded by
/// the request timeout; admission/wire counters happen only afterwards.
pub(super) async fn backoff(
    channel: &ChannelGovernor,
    attempts_used: usize,
    retry_after: Option<Duration>,
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), ProviderError> {
    let local = channel.backoff_for_attempt(attempts_used.saturating_add(1));
    let delay = local.max(retry_after.unwrap_or_default());
    if delay > channel.policy().max_backoff || delay >= timeout {
        return Err(ProviderError::ChannelUnavailable {
            retry_after_ms: delay.as_millis().min(u128::from(u64::MAX)) as u64,
        });
    }
    let start = Instant::now();
    tokio::select! {
        biased;
        _ = cancelled(cancel) => Err(ProviderError::Cancelled),
        _ = tokio::time::sleep(timeout) => Err(ProviderError::Timeout),
        _ = tokio::time::sleep(delay) => {
            // Keep the deadline result stable even when both timers become ready together.
            if start.elapsed() >= timeout { Err(ProviderError::Timeout) } else { Ok(()) }
        }
    }
}

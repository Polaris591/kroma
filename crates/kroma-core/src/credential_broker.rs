//! M1-02: the credential broker's decision, the liabilities it must survive a restart with, and
//! the enforcement point that makes the decision binding.
//!
//! A broker that only checks "is this URL allowed" is a proxy with extra steps. The grant here is
//! bound to a job, a subject, a set of models, an endpoint and a spend envelope, and every request
//! is checked against all of them before it may be dispatched. Two things are refused on purpose:
//!
//! * **Redirects.** A permitted endpoint that answers `302` to somewhere else is how an allowlist
//!   becomes a general-purpose forwarder. A redirect is not followed; it is a deny.
//! * **Private addresses.** Loopback, link-local (including the cloud metadata address), RFC1918,
//!   IPv6 unique-local / link-local and IPv4-mapped IPv6 spellings of those are refused unless the
//!   operator named that exact endpoint. Otherwise "call the model API" is also "read the instance
//!   metadata service". The check normalizes the address family first (R14), so `::ffff:169.254.169.254`
//!   is judged as the IPv4 address it reaches. A hostname is not resolved here; a deployment that
//!   needs to prove the resolved destination must bind it at the connect layer.
//!
//! The state that matters across a restart is the money: outstanding reservations and *unknown*
//! settlements are serialised and restored, because a broker that forgets them will happily
//! re-spend the same allowance.
//!
//! [`BrokerGate`] plus [`BrokeredProvider`] are the wiring half: a provider wrapped in them cannot
//! dispatch unless [`authorize`] returns `Ok`, and every dispatch it performs reserves before the
//! call and settles after it against this same [`BrokerState`]. The decision and the ledger are one
//! object on purpose — a second accounting path would drift from the one that gates the request.

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::agent_loop::REQUEST_SERIALIZATION_OVERHEAD_BYTES;
use crate::budget::BudgetConfig;
use crate::provider::{
    DispatchBinding, ModelProvider, ProviderCompletion, ProviderError, ProviderRequest,
};

/// What one job may do. Nothing here is inferable from a URL: the model set and the envelope are
/// granted, not requested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerGrant {
    pub job_id: String,
    pub subject: String,
    /// Exact `scheme://host:port` strings. There is no wildcard, because a wildcard is a decision
    /// someone forgot to make.
    pub endpoints: BTreeSet<String>,
    pub models: BTreeSet<String>,
    pub token_limit: u64,
    pub cost_limit_micros: u64,
    pub epoch: u64,
}

/// One request as it arrives at the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerRequest {
    pub job_id: String,
    pub subject: String,
    pub endpoint: String,
    pub model: String,
    pub max_tokens: u64,
    /// Set when the endpoint answered with a redirect the client was about to follow.
    pub follows_redirect_to: Option<String>,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deny {
    JobMismatch,
    SubjectMismatch,
    EndpointNotAllowed,
    ModelNotAllowed,
    EpochStale {
        granted: u64,
        presented: u64,
    },
    /// A redirect the broker refuses to follow, naming where it pointed.
    RedirectRefused {
        location: String,
    },
    /// A private target that the operator did not name explicitly.
    PrivateAddress {
        host: String,
    },
    BudgetExceeded {
        requested: u64,
        remaining: u64,
    },
    /// The provider cannot state the endpoint and model it will use, so nothing can be authorized.
    /// Refusing here is the difference between "not granted" and "assumed harmless".
    UnboundDispatch,
    /// A paid run started without a declared spend envelope. There is no default envelope: an
    /// implicit one is exactly the "start first, account later" the pilot spec forbids.
    NoOwnerEnvelope,
}

impl fmt::Display for Deny {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Deny::JobMismatch => write!(f, "request job does not match the granted job"),
            Deny::SubjectMismatch => {
                write!(f, "request subject does not match the granted subject")
            }
            Deny::EndpointNotAllowed => write!(f, "endpoint is not in the granted endpoint set"),
            Deny::ModelNotAllowed => write!(f, "model is not in the granted model set"),
            Deny::EpochStale {
                granted,
                presented,
            } => write!(
                f,
                "grant epoch {granted} does not match the presented epoch {presented}"
            ),
            Deny::RedirectRefused { location } => {
                write!(f, "redirect to {location} is refused, not followed")
            }
            Deny::PrivateAddress { host } => write!(
                f,
                "endpoint host {host} is a private address the grant did not opt into"
            ),
            Deny::BudgetExceeded {
                requested,
                remaining,
            } => write!(
                f,
                "budget exceeded: requested {requested} tokens, {remaining} remaining"
            ),
            Deny::UnboundDispatch => write!(
                f,
                "provider states no dispatch binding (endpoint and model), so the request cannot be authorized"
            ),
            Deny::NoOwnerEnvelope => write!(
                f,
                "no owner spend envelope was declared, so a paid run may not start"
            ),
        }
    }
}

/// Durable money. Written before dispatch and restored on start-up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerState {
    /// Requests dispatched but not yet settled, with the allowance they reserved.
    pub reservations: Vec<Reservation>,
    /// Settlements where the provider's usage was unknown. The worst case is kept, not dropped.
    pub unknown_liability_tokens: u64,
    /// Settlements that arrived with a provider-reported usage. Counted against the envelope for
    /// the rest of the grant's life, so a long run cannot re-spend what it already spent.
    /// `serde(default)` keeps a ledger written before this field existed loadable.
    #[serde(default)]
    pub settled_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub job_id: String,
    pub tokens: u64,
    pub cost_micros: u64,
}

impl BrokerState {
    /// Reserve before dispatch. Restoring this list after a restart is what stops the same
    /// allowance being spent twice.
    pub fn reserve(&mut self, reservation: Reservation) {
        self.reservations.push(reservation);
    }

    /// Settlement with known usage: the reservation is released.
    pub fn settle(&mut self, job_id: &str) -> bool {
        match self
            .reservations
            .iter()
            .position(|entry| entry.job_id == job_id)
        {
            Some(index) => {
                self.reservations.remove(index);
                true
            }
            None => false,
        }
    }

    /// Settlement with known usage: the reservation is released and the provider-reported tokens are
    /// recorded as spent, so they are counted against the grant from now on.
    pub fn settle_with_usage(&mut self, job_id: &str, reported_tokens: u64) -> bool {
        match self
            .reservations
            .iter()
            .position(|entry| entry.job_id == job_id)
        {
            Some(index) => {
                self.reservations.remove(index);
                self.settled_tokens = self.settled_tokens.saturating_add(reported_tokens);
                true
            }
            None => false,
        }
    }

    /// Settlement with unknown usage: the reservation is *not* released, its tokens become
    /// liability. A broker that refunded here would treat "we cannot prove it was billed" as free.
    pub fn settle_unknown(&mut self, job_id: &str) -> bool {
        match self
            .reservations
            .iter()
            .position(|entry| entry.job_id == job_id)
        {
            Some(index) => {
                let entry = self.reservations.remove(index);
                self.unknown_liability_tokens += entry.tokens;
                true
            }
            None => false,
        }
    }

    pub fn reserved_tokens(&self) -> u64 {
        self.reservations.iter().map(|entry| entry.tokens).sum()
    }
}

fn host_of(endpoint: &str) -> Option<&str> {
    let rest = endpoint.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    // An IPv6 literal is bracketed and contains its own colons, so the brackets are stripped
    // before the port split; otherwise `[::1]:443` parses as a hostname and never looks private.
    if let Some(stripped) = authority.strip_prefix('[') {
        return stripped.split_once(']').map(|(host, _)| host);
    }
    Some(
        authority
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(authority),
    )
}

fn is_private_v4(address: std::net::Ipv4Addr) -> bool {
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
}

/// R14: one normalization for every address family. An IPv4-mapped IPv6 literal (`::ffff:a.b.c.d`)
/// reaches the same socket as the IPv4 address, so it is judged as that IPv4 address; IPv6 unique
/// local (`fc00::/7`) and unicast link-local (`fe80::/10`) are private for the same reason RFC1918
/// is. Hostnames are still not resolved here — the URL gate cannot see the answer the connect layer
/// will get.
pub(crate) fn is_private(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => is_private_v4(v4),
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => is_private_v4(v4),
            None => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
            }
        },
        // A hostname is not a private address; it is resolved later and by someone else.
        Err(_) => false,
    }
}

/// Decide whether one request may be dispatched.
///
/// Checks run from the cheapest and most specific to the most expensive: identity, then model,
/// then redirect, then endpoint, then private target, then budget.
pub fn authorize(
    grant: &BrokerGrant,
    request: &BrokerRequest,
    state: &BrokerState,
) -> Result<(), Deny> {
    if request.job_id != grant.job_id {
        return Err(Deny::JobMismatch);
    }
    if request.subject != grant.subject {
        return Err(Deny::SubjectMismatch);
    }
    if request.epoch != grant.epoch {
        return Err(Deny::EpochStale {
            granted: grant.epoch,
            presented: request.epoch,
        });
    }
    if !grant.models.contains(&request.model) {
        return Err(Deny::ModelNotAllowed);
    }
    if let Some(location) = &request.follows_redirect_to {
        return Err(Deny::RedirectRefused {
            location: location.clone(),
        });
    }
    if !grant.endpoints.contains(&request.endpoint) {
        return Err(Deny::EndpointNotAllowed);
    }
    if let Some(host) = host_of(&request.endpoint) {
        if is_private(host) {
            return Err(Deny::PrivateAddress {
                host: host.to_string(),
            });
        }
    }
    let remaining = grant.token_limit.saturating_sub(
        state
            .reserved_tokens()
            .saturating_add(state.unknown_liability_tokens)
            .saturating_add(state.settled_tokens),
    );
    if request.max_tokens > remaining {
        return Err(Deny::BudgetExceeded {
            requested: request.max_tokens,
            remaining,
        });
    }
    Ok(())
}

/// The identity a caller presents for itself when it asks to dispatch.
///
/// It is kept separate from the [`BrokerGrant`] on purpose: a grant states what a job *may* do, the
/// presented identity states which job is *asking*. Checking one against the other is what makes
/// the job, subject and epoch bindings real instead of vacuous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerIdentity {
    pub job_id: String,
    pub subject: String,
    pub epoch: u64,
}

/// One granted dispatch, held from admission until settlement.
///
/// The settlement methods consume it, so a dispatch cannot be settled twice or be dispatched without
/// a reservation: the only way to obtain one is a successful [`BrokerGate::admit`].
#[derive(Debug, PartialEq, Eq)]
pub struct DispatchPermit {
    label: String,
    tokens: u64,
}

/// The broker as an enforcement point: admission, reservation and settlement against one
/// [`BrokerState`].
///
/// Nothing here decides policy of its own; [`authorize`] does that. What the gate adds is that the
/// decision is taken *at the dispatch* and that its outcome is written to the same ledger the
/// budget check reads, so a sequence of dispatches depletes one envelope instead of each being
/// judged against a fresh one.
pub struct BrokerGate {
    grant: BrokerGrant,
    presented: BrokerIdentity,
    state: Arc<Mutex<BrokerState>>,
    /// Distinguishes concurrent or repeated dispatches of the same job in the ledger. One job can
    /// have many dispatches (parent turns, child subagents), and a reservation key must identify the
    /// dispatch that settlement refers to; the authorization check still compares the plain job id.
    sequence: AtomicU64,
}

impl BrokerGate {
    pub fn new(
        grant: BrokerGrant,
        presented: BrokerIdentity,
        state: Arc<Mutex<BrokerState>>,
    ) -> Self {
        Self {
            grant,
            presented,
            state,
            sequence: AtomicU64::new(0),
        }
    }

    /// The checkpoint. Either this returns a permit, or the request must not be sent.
    ///
    /// `binding` is what the provider says it will put on the wire; a provider that does not state
    /// it is refused rather than assumed safe. `requested_tokens` is this dispatch's worst-case
    /// claim, which is reserved before the call and settled afterwards.
    pub fn admit(
        &self,
        binding: Option<&DispatchBinding>,
        requested_tokens: u64,
    ) -> Result<DispatchPermit, Deny> {
        let Some(binding) = binding else {
            return Err(Deny::UnboundDispatch);
        };
        let request = BrokerRequest {
            job_id: self.presented.job_id.clone(),
            subject: self.presented.subject.clone(),
            endpoint: binding.endpoint.clone(),
            model: binding.model.clone(),
            max_tokens: requested_tokens,
            // The protocol clients refuse to follow a redirect themselves, so a request is never
            // admitted with a redirect destination attached.
            follows_redirect_to: None,
            epoch: self.presented.epoch,
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        authorize(&self.grant, &request, &state)?;
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let label = format!("{}#{}", self.grant.job_id, sequence);
        state.reserve(Reservation {
            job_id: label.clone(),
            tokens: requested_tokens,
            // No price table is wired here: the token envelope is the enforced bound.
            cost_micros: 0,
        });
        Ok(DispatchPermit {
            label,
            tokens: requested_tokens,
        })
    }

    /// Settlement with a usage the provider reported: the reservation is released and the reported
    /// tokens are recorded as spent.
    pub fn settle_with_usage(&self, permit: DispatchPermit, reported_tokens: u64) {
        let settled = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .settle_with_usage(&permit.label, reported_tokens);
        debug_assert!(
            settled,
            "dispatch {} was admitted but is missing from the ledger",
            permit.label
        );
    }

    /// Settlement where the usage is unknown: the reservation becomes liability, it is *not*
    /// refunded. `we cannot prove it was billed` is not `it was free`.
    pub fn settle_unknown(&self, permit: DispatchPermit) {
        let settled = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .settle_unknown(&permit.label);
        debug_assert!(
            settled,
            "dispatch {} was admitted but is missing from the ledger",
            permit.label
        );
    }

    /// Settlement for a dispatch that provably never left the process: only here is the reservation
    /// released without recording spend.
    pub fn release_undispatched(&self, permit: DispatchPermit) {
        let released = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .settle(&permit.label);
        debug_assert!(
            released,
            "dispatch {} was admitted but is missing from the ledger",
            permit.label
        );
    }
}

/// The worst-case token claim for one dispatch.
///
/// Computed from the request rather than taken from the caller: the checkpoint has to bound a caller
/// that did not budget at all, otherwise "I forgot to reserve" would be a way past the envelope.
/// The input estimate is the crate's conservative one (the same function the context-window check
/// uses) plus the output cap the request declares.
///
/// Known gap: a request that declares no output cap (and a protocol client that applies no default)
/// is only bounded on its input side; see the M1-02 wiring report.
fn pre_dispatch_claim(req: &ProviderRequest) -> u64 {
    let tool_schema_bytes = serde_json::to_string(&req.tools)
        .map(|json| json.len())
        .unwrap_or(0);
    crate::budget::conservative_request_tokens(
        &req.system_prompt,
        &req.messages,
        tool_schema_bytes,
        REQUEST_SERIALIZATION_OVERHEAD_BYTES,
    )
    .saturating_add(req.max_output_tokens.unwrap_or(0))
}

/// A [`ModelProvider`] that may only dispatch through a [`BrokerGate`].
///
/// Wrapping is the wiring: every dispatch that reaches the inner provider passes the checkpoint
/// first, and because callers hand the *same* `Arc<dyn ModelProvider>` to parent runs, subagent
/// children and benchmark arms, wrapping once covers those dispatches too.
///
/// A refused dispatch returns [`ProviderError::NotDispatched`] wrapping
/// [`ProviderError::BrokerDenied`], so the caller's own ledger refunds the allowance it reserved for
/// an attempt that provably never happened.
pub struct BrokeredProvider {
    inner: Arc<dyn ModelProvider>,
    gate: Arc<BrokerGate>,
}

impl BrokeredProvider {
    pub fn new(inner: Arc<dyn ModelProvider>, gate: Arc<BrokerGate>) -> Self {
        Self { inner, gate }
    }
}

/// Wrap a provider so that every dispatch is admitted, reserved and settled against one broker
/// state, with a grant built from what the provider itself declares it will put on the wire.
///
/// The destination is taken from [`ModelProvider::dispatch_binding`] rather than re-derived from the
/// base URL: one source of truth for "where will this request go", so the gate and the wire cannot
/// disagree. A provider that declares nothing is returned unwrapped, because the broker cannot vouch
/// for a destination it cannot see - callers that need the strict answer ask
/// [`provider_declares_destination`] first, which is what the M1-06 pilot gate does.
pub fn enforce(
    provider: Arc<dyn ModelProvider>,
    job_id: &str,
    token_limit: u64,
) -> Arc<dyn ModelProvider> {
    let Some(binding) = provider.dispatch_binding() else {
        return provider;
    };
    let subject = "kroma-run".to_string();
    let grant = BrokerGrant {
        job_id: job_id.to_string(),
        subject: subject.clone(),
        endpoints: std::iter::once(binding.endpoint).collect(),
        models: std::iter::once(binding.model).collect(),
        token_limit,
        cost_limit_micros: 0,
        epoch: 0,
    };
    let identity = BrokerIdentity {
        job_id: grant.job_id.clone(),
        subject,
        epoch: grant.epoch,
    };
    let state = Arc::new(Mutex::new(BrokerState::default()));
    Arc::new(BrokeredProvider::new(
        provider,
        Arc::new(BrokerGate::new(grant, identity, state)),
    ))
}

/// The owner's spend envelope for one paid run, in tokens.
///
/// There is deliberately no default and no "unlimited" arm: a run that cannot state what it may
/// spend is refused before the first dispatch, which is what the pilot spec means by "no valid
/// envelope, no start". `None` and `Some(0)` are both refusals - zero is not a small envelope, it is
/// a run that could never make a single request.
pub fn owner_envelope(declared_tokens: Option<u64>) -> Result<u64, Deny> {
    match declared_tokens {
        Some(tokens) if tokens > 0 => Ok(tokens),
        _ => Err(Deny::NoOwnerEnvelope),
    }
}

/// Whether this provider states the destination and model it will send, which is the precondition
/// for the broker to be able to gate it at all.
pub fn provider_declares_destination(provider: &Arc<dyn ModelProvider>) -> bool {
    provider.dispatch_binding().is_some()
}

impl ModelProvider for BrokeredProvider {
    fn dispatch_binding(&self) -> Option<DispatchBinding> {
        self.inner.dispatch_binding()
    }

    /// R07: a decorator must forward every capability method. The runner asks this before it
    /// decides whether the request may carry cache breakpoints, so dropping it here silently
    /// disabled explicit prompt caching for every broker-wrapped provider.
    fn message_semantics(&self) -> crate::provider::MessageSemantics {
        self.inner.message_semantics()
    }

    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        budget: &'a BudgetConfig,
        cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            let claim = pre_dispatch_claim(&req);
            let permit = self
                .gate
                .admit(self.inner.dispatch_binding().as_ref(), claim)
                .map_err(|deny| {
                    ProviderError::NotDispatched(Box::new(ProviderError::BrokerDenied(
                        deny.to_string(),
                    )))
                })?;
            // Cloned before the request moves into the inner provider: it is the only evidence of
            // whether anything was actually transmitted.
            let wire_attempts = req.wire_attempts.clone();
            match self.inner.complete(req, budget, cancel_rx).await {
                Ok(completion) => {
                    if completion.attempts_used > 1 {
                        // A retried call cannot prove what its failed attempt consumed, so the whole
                        // reservation stays liability — the same rule the run ledger applies.
                        self.gate.settle_unknown(permit);
                    } else if let Some(usage) = completion.turn.usage.as_ref() {
                        self.gate.settle_with_usage(
                            permit,
                            usage.input_tokens.saturating_add(usage.output_tokens),
                        );
                    } else {
                        self.gate.settle_unknown(permit);
                    }
                    Ok(completion)
                }
                Err(error) => {
                    let transmitted = wire_attempts.map(|wire| wire.get() as u64).unwrap_or(0);
                    if error.is_provably_zero_consumption() && transmitted == 0 {
                        self.gate.release_undispatched(permit);
                    } else {
                        self.gate.settle_unknown(permit);
                    }
                    Err(error)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> BrokerGrant {
        BrokerGrant {
            job_id: "job-1".to_string(),
            subject: "worker".to_string(),
            endpoints: ["https://api.example.com:443".to_string()]
                .into_iter()
                .collect(),
            models: ["gemini-3.8-flash".to_string()].into_iter().collect(),
            token_limit: 1000,
            cost_limit_micros: 5_000_000,
            epoch: 7,
        }
    }

    fn request() -> BrokerRequest {
        BrokerRequest {
            job_id: "job-1".to_string(),
            subject: "worker".to_string(),
            endpoint: "https://api.example.com:443".to_string(),
            model: "gemini-3.8-flash".to_string(),
            max_tokens: 100,
            follows_redirect_to: None,
            epoch: 7,
        }
    }

    /// M1-02-A1: job, subject, model, endpoint, epoch and budget are all bound, and none of them
    /// is inferable from another.
    #[test]
    fn the_grant_binds_job_subject_model_endpoint_epoch_and_budget() {
        let grant = grant();
        let state = BrokerState::default();
        assert_eq!(authorize(&grant, &request(), &state), Ok(()));

        let mut wrong = request();
        wrong.model = "gpt-other".to_string();
        assert_eq!(
            authorize(&grant, &wrong, &state),
            Err(Deny::ModelNotAllowed)
        );

        let mut wrong = request();
        wrong.endpoint = "https://api.other.com:443".to_string();
        assert_eq!(
            authorize(&grant, &wrong, &state),
            Err(Deny::EndpointNotAllowed)
        );

        let mut wrong = request();
        wrong.subject = "someone-else".to_string();
        assert_eq!(
            authorize(&grant, &wrong, &state),
            Err(Deny::SubjectMismatch)
        );

        let mut wrong = request();
        wrong.epoch = 6;
        assert_eq!(
            authorize(&grant, &wrong, &state),
            Err(Deny::EpochStale {
                granted: 7,
                presented: 6
            })
        );

        let mut spent = BrokerState::default();
        spent.reserve(Reservation {
            job_id: "job-0".to_string(),
            tokens: 950,
            cost_micros: 0,
        });
        assert_eq!(
            authorize(&grant, &request(), &spent),
            Err(Deny::BudgetExceeded {
                requested: 100,
                remaining: 50
            })
        );
    }

    /// M1-02-A2: a redirect is a deny, and a private target is a deny even when it is on the
    /// allowlist — the operator has to name the exact endpoint to opt in.
    #[test]
    fn redirects_and_private_targets_are_refused() {
        let grant = grant();
        let state = BrokerState::default();

        let mut redirected = request();
        redirected.follows_redirect_to = Some("https://evil.example.com:443".to_string());
        assert_eq!(
            authorize(&grant, &redirected, &state),
            Err(Deny::RedirectRefused {
                location: "https://evil.example.com:443".to_string()
            })
        );

        // Metadata service and loopback are refused even if the grant names them.
        for endpoint in [
            "http://169.254.169.254:80",
            "http://127.0.0.1:8080",
            "http://10.0.0.5:443",
            "http://[::1]:443",
        ] {
            let mut permissive = grant.clone();
            permissive.endpoints.insert(endpoint.to_string());
            let mut ask = request();
            ask.endpoint = endpoint.to_string();
            assert!(
                matches!(
                    authorize(&permissive, &ask, &state),
                    Err(Deny::PrivateAddress { .. })
                ),
                "{endpoint} must be refused as a private target"
            );
        }
    }

    /// M1-02-A3: reservations and unknown liability survive a restart, and an unknown settlement
    /// is not silently refunded.
    #[test]
    fn restart_restores_reservations_and_keeps_unknown_liability() {
        let mut state = BrokerState::default();
        state.reserve(Reservation {
            job_id: "job-a".to_string(),
            tokens: 200,
            cost_micros: 100,
        });
        state.reserve(Reservation {
            job_id: "job-b".to_string(),
            tokens: 300,
            cost_micros: 100,
        });
        assert!(state.settle_unknown("job-a"));
        assert!(
            !state.settle("job-a"),
            "an unknown settlement is not a refund"
        );

        let json = serde_json::to_string(&state).unwrap();
        let mut restored: BrokerState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.unknown_liability_tokens, 200);
        assert_eq!(
            restored.reserved_tokens(),
            300,
            "the live reservation is preserved"
        );

        // The restored broker must not hand out the allowance that is already committed.
        let mut grant = grant();
        grant.token_limit = 400;
        let mut ask = request();
        ask.max_tokens = 200;
        assert_eq!(
            authorize(&grant, &ask, &restored),
            Err(Deny::BudgetExceeded {
                requested: 200,
                // 400 granted - 300 reserved - 200 unknown liability saturates at zero, it does not
                // wrap around into a fresh allowance.
                remaining: 0
            })
        );
        assert!(restored.settle("job-b"));
        assert_eq!(restored.reserved_tokens(), 0);
    }

    #[test]
    fn host_parsing_covers_bracketed_ipv6() {
        assert_eq!(host_of("http://[::1]:443"), Some("::1"));
        assert!(is_private("::1"));
        assert_eq!(
            host_of("http://169.254.169.254:80"),
            Some("169.254.169.254")
        );
        assert!(is_private("169.254.169.254"));
    }

    /// R14: every IPv6 spelling that reaches a private IPv4 or a private IPv6 range is refused, and
    /// the mapped forms are judged by the IPv4 rules rather than as opaque IPv6 addresses.
    #[test]
    fn ipv6_private_and_ipv4_mapped_targets_are_refused() {
        for host in [
            "::ffff:169.254.169.254",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.5",
            "fc00::1",
            "fd12:3456:789a::1",
            "fe80::1",
            "::1",
            "::",
        ] {
            assert!(
                is_private(host),
                "{host} must be refused as a private target"
            );
        }
        for host in ["2606:4700:4700::1111", "::ffff:8.8.8.8", "api.example.com"] {
            assert!(!is_private(host), "{host} is not a private address");
        }
    }

    fn binding_for(model: &str) -> DispatchBinding {
        DispatchBinding {
            endpoint: "https://api.example.com:443".to_string(),
            model: model.to_string(),
        }
    }

    fn identity(job_id: &str, subject: &str, epoch: u64) -> BrokerIdentity {
        BrokerIdentity {
            job_id: job_id.to_string(),
            subject: subject.to_string(),
            epoch,
        }
    }

    fn gate_with(presented: BrokerIdentity) -> (BrokerGate, Arc<Mutex<BrokerState>>) {
        let state = Arc::new(Mutex::new(BrokerState::default()));
        (
            BrokerGate::new(grant(), presented, Arc::clone(&state)),
            state,
        )
    }

    /// M1-02-B10: what the caller presents is checked against the grant, so the job, subject and
    /// epoch bindings are not vacuous copies of the grant's own fields.
    #[test]
    fn the_gate_checks_the_presented_identity_against_the_grant() {
        for (presented, expected) in [
            (identity("job-2", "worker", 7), Deny::JobMismatch),
            (identity("job-1", "someone-else", 7), Deny::SubjectMismatch),
            (
                identity("job-1", "worker", 6),
                Deny::EpochStale {
                    granted: 7,
                    presented: 6,
                },
            ),
        ] {
            let (gate, state) = gate_with(presented);
            assert_eq!(
                gate.admit(Some(&binding_for("gemini-3.8-flash")), 100),
                Err(expected)
            );
            assert!(
                state.lock().unwrap().reservations.is_empty(),
                "a refused admission reserves nothing"
            );
        }
    }

    /// M1-02-B11: two dispatches of the same job get distinct ledger keys, so one settlement cannot
    /// release the other's reservation and the unknown-liability attribution stays exact.
    #[test]
    fn each_dispatch_settles_only_its_own_reservation() {
        let (gate, state) = gate_with(identity("job-1", "worker", 7));
        let first = gate
            .admit(Some(&binding_for("gemini-3.8-flash")), 100)
            .unwrap();
        let second = gate
            .admit(Some(&binding_for("gemini-3.8-flash")), 250)
            .unwrap();
        assert_eq!(state.lock().unwrap().reserved_tokens(), 350);

        gate.settle_with_usage(first, 40);
        assert_eq!(state.lock().unwrap().settled_tokens, 40);
        assert_eq!(
            state.lock().unwrap().reserved_tokens(),
            250,
            "the second dispatch keeps its own reservation"
        );

        gate.settle_unknown(second);
        let state = state.lock().unwrap();
        assert!(state.reservations.is_empty());
        assert_eq!(state.settled_tokens, 40);
        assert_eq!(state.unknown_liability_tokens, 250);
    }

    /// M1-02-B12: a settled dispatch survives a restart as settled spend, so a restarted broker
    /// cannot hand out the same allowance again.
    #[test]
    fn a_settled_spend_is_not_reoffered_after_a_restart() {
        let (gate, state) = gate_with(identity("job-1", "worker", 7));
        let permit = gate
            .admit(Some(&binding_for("gemini-3.8-flash")), 900)
            .unwrap();
        gate.settle_with_usage(permit, 950);

        let json = serde_json::to_string(&*state.lock().unwrap()).unwrap();
        let restored: BrokerState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.settled_tokens, 950);
        assert_eq!(
            authorize(&grant(), &request(), &restored),
            Err(Deny::BudgetExceeded {
                requested: 100,
                remaining: 50,
            })
        );
    }
}

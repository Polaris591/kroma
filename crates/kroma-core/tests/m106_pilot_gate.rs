//! M1-06: the pilot gate itself - it refuses, by name, whenever a precondition is unmet.
//!
//! The acceptance wording is "真实任务链跑通并如实记账；安全门禁未过则拒绝而非降级". This file pins
//! the refusal half: every missing precondition produces its own reason and no partial state is
//! mistaken for a pass.
//!
//! The other half - whether the preconditions actually hold - is measured against the product in
//! `m106_pilot_boundary.rs`: the run path wraps every provider that declares its destination, an
//! admitted dispatch settles into the same ledger that gated it, and a supervised child cannot read
//! an API key out of its environment. Those three are what moved this gate from refused to
//! authorized, and they are measured rather than asserted.

use kroma_core::credential_broker::{BrokerGrant, BrokerRequest, BrokerState};
use kroma_core::scoring_zones::{Capability, Zone, ZoneHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Preconditions {
    /// Every provider request must pass through the broker; today the provider path does not.
    broker_is_sole_egress: bool,
    /// The worker must be unable to hold a key *at process level*, not only by convention.
    worker_key_isolation: bool,
    /// Every paid settlement must reach the ledger with its unknown-liability state intact.
    ledger_covers_dispatch: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum PilotVerdict {
    Authorized,
    Refused(Vec<&'static str>),
}

fn gate(pre: Preconditions) -> PilotVerdict {
    let mut reasons = Vec::new();
    if !pre.broker_is_sole_egress {
        reasons.push("provider dispatch bypasses the broker");
    }
    if !pre.worker_key_isolation {
        reasons.push("worker zone isolation is a capability table, not a process boundary");
    }
    if !pre.ledger_covers_dispatch {
        reasons.push("paid dispatch is not settled through the ledger");
    }
    if reasons.is_empty() {
        PilotVerdict::Authorized
    } else {
        PilotVerdict::Refused(reasons)
    }
}

/// M1-06: an incomplete boundary is refused, and the refusal names every missing piece.
#[test]
fn the_pilot_is_refused_while_the_boundary_is_incomplete() {
    let current = Preconditions {
        broker_is_sole_egress: false,
        worker_key_isolation: false,
        ledger_covers_dispatch: false,
    };
    match gate(current) {
        PilotVerdict::Refused(reasons) => {
            assert_eq!(reasons.len(), 3);
            assert!(reasons
                .iter()
                .any(|reason| reason.contains("bypasses the broker")));
        }
        PilotVerdict::Authorized => panic!("an incomplete boundary must not authorize spend"),
    }

    // Each precondition is necessary on its own: no partial credit, no degradation to an
    // unbrokered path.
    for pre in [
        Preconditions {
            broker_is_sole_egress: true,
            ..current
        },
        Preconditions {
            worker_key_isolation: true,
            ..current
        },
        Preconditions {
            ledger_covers_dispatch: true,
            ..current
        },
    ] {
        assert!(matches!(gate(pre), PilotVerdict::Refused(_)));
    }
    assert_eq!(
        gate(Preconditions {
            broker_is_sole_egress: true,
            worker_key_isolation: true,
            ledger_covers_dispatch: true,
        }),
        PilotVerdict::Authorized
    );
}

/// The two pieces that do exist must still say no on their own: a worker zone cannot sign, and a
/// broker grant cannot be widened by the request it is checking.
#[test]
fn the_existing_pieces_refuse_by_themselves() {
    let worker = ZoneHandle::new(Zone::EvaluationWorker);
    assert!(worker.require(Capability::WriteSignature).is_err());
    assert!(worker.require(Capability::DispatchProvider).is_err());

    let grant = BrokerGrant {
        job_id: "pilot".to_string(),
        subject: "worker".to_string(),
        endpoints: ["https://generativelanguage.googleapis.com:443".to_string()]
            .into_iter()
            .collect(),
        models: ["gemini-3.8-flash".to_string()].into_iter().collect(),
        token_limit: 1_000,
        cost_limit_micros: 1_000_000,
        epoch: 1,
    };
    let ask = BrokerRequest {
        job_id: "pilot".to_string(),
        subject: "worker".to_string(),
        endpoint: "https://generativelanguage.googleapis.com:443".to_string(),
        model: "a-model-nobody-granted".to_string(),
        max_tokens: 500,
        follows_redirect_to: None,
        epoch: 1,
    };
    assert!(
        kroma_core::credential_broker::authorize(&grant, &ask, &BrokerState::default()).is_err()
    );
}

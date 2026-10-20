//! M1-03: scoring zones, so a score cannot be produced by the thing it scores.
//!
//! The blueprint's five-way split collapses to four enforceable questions here, because the
//! answers are what a boundary is for:
//!
//! * may this zone **dispatch a paid request** or read provider credentials?
//! * may it **read the hidden cases**?
//! * may it **write the evidence signature**?
//! * may it **execute candidate code**?
//!
//! The table is a constant, not configuration: a candidate cannot widen it, and a zone cannot
//! widen itself. `EvaluationWorker` executes untrusted code, so it holds no key, no dispatch and
//! no hidden-case access; `EvidenceSigner` can sign but never executes anything.
//!
//! The last piece is the information flow: a hidden result may exist, but whether it may travel
//! back to the agent is a separate decision (`HiddenFeedback`), and for a confirmatory run it may
//! not. Otherwise "hidden" is just a label on data the candidate can read.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Send a request that costs money or needs a provider credential.
    DispatchProvider,
    /// Read the hidden evaluation cases.
    ReadHiddenCases,
    /// Produce the evidence signature.
    WriteSignature,
    /// Run candidate-supplied code.
    ExecuteCandidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// Authority: policy, credentials, budget. Never executes candidate code.
    HostAuthority,
    /// Selects cases, holds the frozen strategy, collects evidence. Never executes candidate code.
    EvaluationController,
    /// Runs one candidate in a disposable sandbox. No key, no dispatch, no hidden cases.
    EvaluationWorker,
    /// Signs what the controller verified. Never executes anything.
    EvidenceSigner,
}

impl Zone {
    /// What each zone may do. Adding a capability here is a deliberate boundary change.
    pub fn capabilities(self) -> &'static [Capability] {
        match self {
            Zone::HostAuthority => &[Capability::DispatchProvider],
            Zone::EvaluationController => &[Capability::ReadHiddenCases],
            Zone::EvaluationWorker => &[Capability::ExecuteCandidate],
            Zone::EvidenceSigner => &[Capability::WriteSignature],
        }
    }

    pub fn may(self, capability: Capability) -> bool {
        self.capabilities().contains(&capability)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Denied {
    pub zone: Zone,
    pub capability: Capability,
}

/// A handle to one zone. Asking for a capability it does not have is an error, not a warning.
#[derive(Debug, Clone, Copy)]
pub struct ZoneHandle {
    zone: Zone,
}

impl ZoneHandle {
    pub fn new(zone: Zone) -> Self {
        Self { zone }
    }

    pub fn zone(&self) -> Zone {
        self.zone
    }

    /// The check every privileged action goes through.
    pub fn require(&self, capability: Capability) -> Result<(), Denied> {
        if self.zone.may(capability) {
            Ok(())
        } else {
            Err(Denied {
                zone: self.zone,
                capability,
            })
        }
    }
}

/// Whether a hidden result may travel back to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenFeedback {
    /// Interactive repair: the agent may see *public* failures only.
    PublicOnly,
    /// Confirmatory run: the final score exists but is never fed back per attempt.
    FinalOnly,
}

impl HiddenFeedback {
    /// Hidden per-attempt detail is never visible to the agent; the only difference between the
    /// two policies is whether a single final verdict is released at the end.
    pub fn per_attempt_visible(self) -> bool {
        false
    }

    pub fn final_verdict_released(self) -> bool {
        matches!(self, HiddenFeedback::FinalOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M1-03-A1: the zone that executes candidate code holds no key, cannot dispatch and cannot
    /// read the hidden cases.
    #[test]
    fn the_worker_zone_holds_no_key_and_no_hidden_cases() {
        let worker = ZoneHandle::new(Zone::EvaluationWorker);
        assert!(worker.require(Capability::ExecuteCandidate).is_ok());
        for capability in [
            Capability::WriteSignature,
            Capability::DispatchProvider,
            Capability::ReadHiddenCases,
        ] {
            assert_eq!(
                worker.require(capability),
                Err(Denied {
                    zone: Zone::EvaluationWorker,
                    capability
                }),
                "a worker must not have {capability:?}"
            );
        }

        let signer = ZoneHandle::new(Zone::EvidenceSigner);
        assert!(signer.require(Capability::WriteSignature).is_ok());
        assert!(
            signer.require(Capability::ExecuteCandidate).is_err(),
            "the signer must never run candidate code"
        );

        let host = ZoneHandle::new(Zone::HostAuthority);
        assert!(host.require(Capability::DispatchProvider).is_ok());
        assert!(host.require(Capability::ReadHiddenCases).is_err());
    }

    /// M1-03-A2: no zone can widen its own capabilities - the table is the only source.
    #[test]
    fn the_capability_table_is_the_only_authority() {
        for zone in [
            Zone::HostAuthority,
            Zone::EvaluationController,
            Zone::EvaluationWorker,
            Zone::EvidenceSigner,
        ] {
            let handle = ZoneHandle::new(zone);
            for capability in [
                Capability::DispatchProvider,
                Capability::ReadHiddenCases,
                Capability::WriteSignature,
                Capability::ExecuteCandidate,
            ] {
                assert_eq!(
                    handle.require(capability).is_ok(),
                    zone.capabilities().contains(&capability),
                    "{zone:?} must agree with the table for {capability:?}"
                );
            }
        }
        // Exactly one zone executes candidate code and exactly one signs: the two are disjoint.
        let executors = [
            Zone::HostAuthority,
            Zone::EvaluationController,
            Zone::EvaluationWorker,
            Zone::EvidenceSigner,
        ]
        .into_iter()
        .filter(|zone| zone.may(Capability::ExecuteCandidate))
        .count();
        assert_eq!(executors, 1);
        assert!(!Zone::EvidenceSigner.may(Capability::ExecuteCandidate));
    }

    /// M1-03-A3: public verification and hidden evaluation are separate flows, and hidden detail
    /// never flows back per attempt under either policy.
    #[test]
    fn hidden_results_do_not_leak_back_per_attempt() {
        for policy in [HiddenFeedback::PublicOnly, HiddenFeedback::FinalOnly] {
            assert!(
                !policy.per_attempt_visible(),
                "hidden per-attempt detail must never reach the agent, even in {policy:?}"
            );
        }
        assert!(!HiddenFeedback::PublicOnly.final_verdict_released());
        assert!(HiddenFeedback::FinalOnly.final_verdict_released());

        // The controller is the only zone that may read hidden cases; the worker that produced
        // the candidate result cannot.
        assert!(Zone::EvaluationController.may(Capability::ReadHiddenCases));
        assert!(!Zone::EvaluationWorker.may(Capability::ReadHiddenCases));
    }
}

//! C00-08: what the model sees is not what it may call.
//!
//! Three sets that the baseline keeps in one place: the tools a registry *has*, the tools a model
//! is *shown*, and the tools a call is *allowed* to reach. Collapsing them is what lets a
//! "discovery" feature quietly become a permission grant, and what lets a dynamic MCP refresh
//! change the schema under a request that is already in flight.
//!
//! So this module keeps them apart on purpose:
//!
//! * `Exposure` says how a tool is presented (`Direct`, `Deferred`, `CodeMode`, `Hidden`).
//! * `ExposureSet` answers presentation questions only; it never authorizes anything.
//! * `RequestSnapshot` freezes the declared set for one request, so a refresh that lands mid-flight
//!   takes effect at the next boundary instead of inside the current request.
//!
//! Authorization itself stays where it already is (`PolicyMode` + the tool gateway); this module
//! only refuses to let exposure stand in for it.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// Declared to the model like a built-in tool.
    Direct,
    /// Not declared until a search loads it for the next request.
    Deferred,
    /// Reachable from a script, never declared.
    CodeMode,
    /// Registered but unreachable.
    Hidden,
}

/// How a tool is presented right now. Presentation only.
#[derive(Debug, Default, Clone)]
pub struct ExposureSet {
    tools: BTreeMap<String, Exposure>,
}

impl ExposureSet {
    pub fn declare(&mut self, name: impl Into<String>, exposure: Exposure) {
        self.tools.insert(name.into(), exposure);
    }

    pub fn withdraw(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    pub fn exposure(&self, name: &str) -> Option<Exposure> {
        self.tools.get(name).copied()
    }

    /// The names declared to the model in this request: `Direct` only. `Deferred` tools are
    /// reachable after a search, but they are not part of the declaration.
    pub fn declared(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter(|(_, exposure)| **exposure == Exposure::Direct)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Names a model may discover, i.e. everything not hidden.
    pub fn discoverable(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter(|(_, exposure)| **exposure != Exposure::Hidden)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// What one request froze: the declaration the model was given, and the names it may reach.
///
/// A refresh produces a *new* snapshot; it never mutates one that a request already holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSnapshot {
    request_id: u64,
    declared: Vec<String>,
    reachable: BTreeSet<String>,
}

impl RequestSnapshot {
    /// Freeze a request against the current exposure set and the currently authorized tools.
    ///
    /// `authorized` is the gateway's answer, not the exposure set's: a `Direct` tool that is not
    /// authorized is still not reachable, and a `Deferred` tool that is authorized is reachable
    /// without being declared. `Hidden` means registered-but-unreachable, so it is excluded even
    /// when the gateway would allow it.
    pub fn freeze(request_id: u64, exposure: &ExposureSet, authorized: &BTreeSet<String>) -> Self {
        let reachable = exposure
            .discoverable()
            .into_iter()
            .filter(|name| authorized.contains(name))
            .collect();
        Self {
            request_id,
            declared: exposure.declared(),
            reachable,
        }
    }

    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    pub fn declared(&self) -> &[String] {
        &self.declared
    }

    /// Whether a call from this request may reach `name`.
    ///
    /// Visibility is neither necessary nor sufficient: a deferred-but-authorized tool is reachable
    /// without being declared, and a declared-but-unauthorized tool is refused.
    pub fn may_call(&self, name: &str) -> bool {
        self.reachable.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authorized(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// C00-08-A1: exposure is not permission, in both directions.
    #[test]
    fn exposure_never_stands_in_for_authorization() {
        let mut exposure = ExposureSet::default();
        exposure.declare("read", Exposure::Direct);
        exposure.declare("shell", Exposure::Direct);
        exposure.declare("mcp__deferred", Exposure::Deferred);
        exposure.declare("mcp__hidden", Exposure::Hidden);

        let snapshot = RequestSnapshot::freeze(
            1,
            &exposure,
            &authorized(&["read", "mcp__deferred", "mcp__hidden"]),
        );

        assert!(snapshot.may_call("read"));
        assert!(
            !snapshot.may_call("shell"),
            "a declared tool the gateway did not authorize must be refused"
        );
        assert!(
            snapshot.declared().contains(&"shell".to_string()),
            "declaration is not a grant: shell stays declared while being uncallable"
        );
        assert!(
            snapshot.may_call("mcp__deferred"),
            "a deferred tool is reachable once discovered, without being declared"
        );
        assert!(
            !snapshot.declared().contains(&"mcp__deferred".to_string()),
            "reachable is not the same as declared"
        );
        assert!(
            !snapshot.may_call("mcp__hidden"),
            "hidden means registered but unreachable, even when the gateway would allow it"
        );
    }

    /// C00-08-A2: a request in flight keeps the schema it was frozen with, even if the exposure
    /// set changes underneath it.
    #[test]
    fn an_in_flight_request_keeps_its_frozen_schema() {
        let mut exposure = ExposureSet::default();
        exposure.declare("read", Exposure::Direct);
        let in_flight = RequestSnapshot::freeze(1, &exposure, &authorized(&["read", "write"]));

        // A dynamic refresh lands while request 1 is still running.
        exposure.declare("write", Exposure::Direct);
        exposure.withdraw("read");

        assert_eq!(
            in_flight.declared(),
            ["read".to_string()],
            "the running request must not see a schema it was not given"
        );
        assert!(
            in_flight.may_call("read"),
            "and must still be able to call what it was given"
        );
        assert!(
            !in_flight.may_call("write"),
            "nor gain access to a tool added mid-flight"
        );
    }

    /// C00-08-A3: the refresh takes effect at the next boundary.
    #[test]
    fn a_refresh_applies_at_the_next_request_boundary() {
        let mut exposure = ExposureSet::default();
        exposure.declare("read", Exposure::Direct);
        let first = RequestSnapshot::freeze(1, &exposure, &authorized(&["read", "write"]));

        exposure.declare("write", Exposure::Direct);
        let second = RequestSnapshot::freeze(2, &exposure, &authorized(&["read", "write"]));

        assert!(!first.may_call("write"));
        assert!(
            second.may_call("write"),
            "the next request sees the refreshed set"
        );
        assert_eq!(second.declared(), ["read".to_string(), "write".to_string()]);
        assert_ne!(first.request_id(), second.request_id());
    }

    /// A withdrawn tool stops being declared and stops being reachable for new requests.
    #[test]
    fn withdrawal_is_visible_to_new_requests_only() {
        let mut exposure = ExposureSet::default();
        exposure.declare("gone", Exposure::Direct);
        let before = RequestSnapshot::freeze(1, &exposure, &authorized(&["gone"]));
        exposure.withdraw("gone");
        let after = RequestSnapshot::freeze(2, &exposure, &authorized(&["gone"]));

        assert!(before.may_call("gone"));
        assert!(
            !after.may_call("gone"),
            "a withdrawn tool is unreachable for new requests"
        );
        assert!(after.declared().is_empty());
        assert_eq!(exposure.exposure("gone"), None);
    }
}

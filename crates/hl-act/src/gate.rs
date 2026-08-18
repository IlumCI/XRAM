//! The authorization gate.
//!
//! Mirrors the proof gate from the discarded verification design: an attempt cannot
//! reach the network unless every condition is satisfied, and the only constructor for
//! a live attempt is the one that checks them.
//!
//! Three independent conditions, all fail-closed:
//!
//! 1. The venue's own stance on automation. `Unknown` blocks — an unread policy is not
//!    consent.
//! 2. An explicit per-niche opt-in from the operator. Ranking something highly is not
//!    permission to act on it.
//! 3. Terms that only a person can accept. Competition rules bind the account holder
//!    legally; nothing here may click through them on their behalf.

use hl_core::AutomationStance;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActMode {
    /// Renders exactly what would happen and does nothing. The default everywhere.
    DryRun,
    Live,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Authorization {
    pub venue: String,
    pub stance: AutomationStance,
    /// Where the stance was read from, so the claim is checkable.
    pub source: String,
    /// Terms a person must accept in their own name before anything can be submitted.
    pub human_acceptance_required: bool,
    /// What that acceptance involves, shown to the operator when we refuse.
    pub acceptance_note: String,
}

impl Authorization {
    pub fn new(venue: &str, stance: AutomationStance, source: &str) -> Self {
        Self {
            venue: venue.into(),
            stance,
            source: source.into(),
            human_acceptance_required: false,
            acceptance_note: String::new(),
        }
    }

    pub fn requiring_human_acceptance(mut self, note: &str) -> Self {
        self.human_acceptance_required = true;
        self.acceptance_note = note.into();
        self
    }
}

/// Niches the operator has explicitly approved, and terms they have confirmed accepting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Consent {
    opted_in: BTreeSet<String>,
    terms_accepted: BTreeSet<String>,
}

impl Consent {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn opt_in(&mut self, niche_id: &str) {
        self.opted_in.insert(niche_id.to_string());
    }
    pub fn accept_terms(&mut self, niche_id: &str) {
        self.terms_accepted.insert(niche_id.to_string());
    }
    pub fn has_opted_in(&self, niche_id: &str) -> bool {
        self.opted_in.contains(niche_id)
    }
    pub fn has_accepted_terms(&self, niche_id: &str) -> bool {
        self.terms_accepted.contains(niche_id)
    }
    pub fn opted_in_niches(&self) -> impl Iterator<Item = &String> {
        self.opted_in.iter()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The venue forbids it, or we have not established that it permits it.
    PolicyForbids { venue: String, stance: AutomationStance },
    /// Nobody said to act on this one.
    NoOperatorOptIn { niche_id: String },
    /// Terms exist that only a person can accept.
    TermsNotAccepted { niche_id: String, note: String },
    /// Nothing worth attempting: no prize, no time, or no evidence.
    NotWorthAttempting { niche_id: String, reason: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::PolicyForbids { venue, stance } => write!(
                f,
                "{venue} automation stance is {stance:?}; an unread or negative policy is not consent"
            ),
            Refusal::NoOperatorOptIn { niche_id } => write!(
                f,
                "{niche_id} has not been opted in; ranking is not permission"
            ),
            Refusal::TermsNotAccepted { niche_id, note } => {
                write!(f, "{niche_id} requires terms accepted in person: {note}")
            }
            Refusal::NotWorthAttempting { niche_id, reason } => {
                write!(f, "{niche_id} is not worth attempting: {reason}")
            }
        }
    }
}

impl std::error::Error for Refusal {}

/// Decide whether a live attempt may proceed.
///
/// Dry runs skip only the *operator* checks — the venue's policy still governs what we
/// are willing to even rehearse, so a forbidden venue produces nothing at all.
pub fn check(
    auth: &Authorization,
    niche_id: &str,
    consent: &Consent,
    mode: ActMode,
) -> Result<(), Refusal> {
    if !auth.stance.may_submit_live() {
        return Err(Refusal::PolicyForbids {
            venue: auth.venue.clone(),
            stance: auth.stance,
        });
    }
    if mode == ActMode::DryRun {
        return Ok(());
    }
    if !consent.has_opted_in(niche_id) {
        return Err(Refusal::NoOperatorOptIn {
            niche_id: niche_id.to_string(),
        });
    }
    if auth.human_acceptance_required && !consent.has_accepted_terms(niche_id) {
        return Err(Refusal::TermsNotAccepted {
            niche_id: niche_id.to_string(),
            note: auth.acceptance_note.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(stance: AutomationStance) -> Authorization {
        Authorization::new("v", stance, "https://example.invalid/policy")
    }

    #[test]
    fn an_unread_policy_blocks_even_a_dry_run() {
        let c = Consent::new();
        for mode in [ActMode::DryRun, ActMode::Live] {
            assert!(matches!(
                check(&auth(AutomationStance::Unknown), "n", &c, mode),
                Err(Refusal::PolicyForbids { .. })
            ));
            assert!(matches!(
                check(&auth(AutomationStance::Forbidden), "n", &c, mode),
                Err(Refusal::PolicyForbids { .. })
            ));
        }
    }

    #[test]
    fn dry_runs_need_no_operator_consent() {
        let c = Consent::new();
        assert!(check(&auth(AutomationStance::Allowed), "n", &c, ActMode::DryRun).is_ok());
    }

    #[test]
    fn ranking_is_not_permission() {
        let c = Consent::new();
        assert!(matches!(
            check(&auth(AutomationStance::Allowed), "n", &c, ActMode::Live),
            Err(Refusal::NoOperatorOptIn { .. })
        ));
    }

    #[test]
    fn terms_cannot_be_accepted_on_someones_behalf() {
        let mut c = Consent::new();
        c.opt_in("n");
        let a = auth(AutomationStance::Allowed).requiring_human_acceptance("competition rules");
        assert!(matches!(
            check(&a, "n", &c, ActMode::Live),
            Err(Refusal::TermsNotAccepted { .. })
        ));
        c.accept_terms("n");
        assert!(check(&a, "n", &c, ActMode::Live).is_ok());
    }

    #[test]
    fn consent_is_per_niche_not_blanket() {
        let mut c = Consent::new();
        c.opt_in("a");
        let a = auth(AutomationStance::Conditional);
        assert!(check(&a, "a", &c, ActMode::Live).is_ok());
        assert!(matches!(
            check(&a, "b", &c, ActMode::Live),
            Err(Refusal::NoOperatorOptIn { .. })
        ));
    }

    #[test]
    fn consent_round_trips_through_serialisation() {
        let mut c = Consent::new();
        c.opt_in("a");
        c.accept_terms("a");
        let json = serde_json::to_string(&c).unwrap();
        let back: Consent = serde_json::from_str(&json).unwrap();
        assert!(back.has_opted_in("a") && back.has_accepted_terms("a"));
        assert!(!back.has_opted_in("b"));
    }
}

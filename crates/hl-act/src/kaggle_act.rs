//! Kaggle actuator.
//!
//! Appraises a competition from public data and rehearses an attempt. It stops at the
//! point where a person is required: joining a competition means accepting its rules
//! under a named account, which is a legal act nothing here will perform on someone's
//! behalf. The API confirms the boundary — submission endpoints answer
//! *"You do not have a Team in this Competition"* until the rules are accepted in person.

use crate::appraise::{Appraisal, Automatability, LeaderboardShape};
use crate::gate::{self, ActMode, Authorization, Consent, Refusal};
use crate::{Attempt, AttemptOutcome, Actuator};
use anyhow::Result;
use hl_core::AutomationStance;
use hl_venues::http::Transport;
use hl_venues::kaggle::{self, Competition};
use serde::Deserialize;

pub struct KaggleActuator {
    transport: Box<dyn Transport>,
    auth: Authorization,
}

impl KaggleActuator {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            auth: Authorization::new(
                "kaggle",
                // Competitions are machine-judged and automated pipelines are ordinary
                // practice; the conditions are one account per person and submissions
                // being your own work.
                AutomationStance::Conditional,
                "https://www.kaggle.com/competition-rules",
            )
            .requiring_human_acceptance(
                "join the competition and accept its rules at kaggle.com; the API \
                 refuses submissions until a team exists",
            ),
        }
    }

    pub fn leaderboard_url(slug: &str) -> String {
        format!("https://www.kaggle.com/api/v1/competitions/{slug}/leaderboard/view")
    }

    fn bearer(&self) -> Option<String> {
        kaggle::auth_token().map(|t| format!("Bearer {t}"))
    }

    /// Visible top of the leaderboard. Kaggle truncates this to the leading entries,
    /// which is enough to see the bar and how tightly packed the field is.
    pub fn leaderboard(&self, slug: &str) -> Result<LeaderboardShape> {
        let Some(bearer) = self.bearer() else {
            return Ok(LeaderboardShape::default());
        };
        let resp = self.transport.get(
            &Self::leaderboard_url(slug),
            &[("Authorization", bearer.as_str()), ("Accept", "application/json")],
        )?;
        if resp.status != 200 {
            return Ok(LeaderboardShape::default());
        }
        Ok(parse_leaderboard(&resp.body))
    }

    /// Appraise a competition honestly.
    ///
    /// `measured_place_rate` comes from the ledger and is `None` until we have actually
    /// placed in something, which keeps the expectation at zero rather than at a
    /// flattering guess.
    pub fn appraise(
        &self,
        competition: &Competition,
        now_ms: u64,
        measured_place_rate: Option<f64>,
    ) -> Result<Appraisal> {
        let shape = self.leaderboard(competition.slug())?;
        let days_left = competition
            .deadline
            .as_deref()
            .and_then(hl_venues::timefmt::parse_rfc3339_utc)
            .map(|d| d.saturating_sub(now_ms) as f64 / hl_core::MS_PER_DAY);

        let mut a = Appraisal {
            niche_id: competition.niche_id(),
            label: competition.title.clone(),
            prize_cents: kaggle::parse_reward_cents(&competition.reward),
            competitors: competition.team_count,
            // Kaggle does not publish the payout split in the listing; three is the
            // common shape and the appraisal says so rather than implying precision.
            paying_places: Some(3),
            score_to_place: shape.worst_visible,
            top_spread: shape.spread(),
            days_left,
            measured_place_rate,
            expected_cents: 0.0,
            automatable: classify(competition),
            basis: format!(
                "{} teams; leaderboard shows {} visible entries",
                competition.team_count, shape.visible_entries
            ),
        };
        a.compute_expected_cents();
        Ok(a)
    }
}

/// Decide whether a competition can be entered without a person in the loop.
pub fn classify(c: &Competition) -> Automatability {
    if kaggle::parse_reward_cents(&c.reward).is_none() {
        return Automatability::NoCashPrize;
    }
    if c.is_kernels_only {
        return Automatability::NotebookOnly;
    }
    // Kaggle does not flag prose tracks in the listing, so they are recognised by name.
    let t = c.title.to_ascii_lowercase();
    if t.contains("paper") || t.contains("essay") || t.contains("writeup") {
        return Automatability::HumanJudged;
    }
    Automatability::FileSubmission
}

#[derive(Deserialize)]
struct LbEnvelope {
    #[serde(default)]
    submissions: Vec<LbRow>,
}

#[derive(Deserialize)]
struct LbRow {
    #[serde(default)]
    score: Option<String>,
}

pub fn parse_leaderboard(body: &str) -> LeaderboardShape {
    let Ok(env) = serde_json::from_str::<LbEnvelope>(body) else {
        return LeaderboardShape::default();
    };
    LeaderboardShape::from_scores(
        env.submissions
            .iter()
            .filter_map(|r| r.score.as_deref())
            .filter_map(|s| s.parse::<f64>().ok())
            .collect(),
    )
}

impl Actuator for KaggleActuator {
    fn id(&self) -> &str {
        "kaggle"
    }

    fn authorization(&self) -> &Authorization {
        &self.auth
    }

    fn attempt(
        &self,
        appraisal: &Appraisal,
        consent: &Consent,
        mode: ActMode,
    ) -> Result<Attempt, Refusal> {
        gate::check(&self.auth, &appraisal.niche_id, consent, mode)?;
        if !appraisal.is_worth_attempting() {
            return Err(Refusal::NotWorthAttempting {
                niche_id: appraisal.niche_id.clone(),
                reason: if appraisal.prize_cents.is_none() {
                    "no cash prize".into()
                } else if appraisal.expected_cents <= 0.0 {
                    "no measured place rate, so expected return is zero".into()
                } else {
                    "window too close to closing".into()
                },
            });
        }

        let plan = format!(
            "join {}, download its data, fit a model, submit predictions before the \
             deadline ({} days out); bar to beat is currently {}",
            appraisal.niche_id,
            appraisal.days_left.map(|d| format!("{d:.0}")).unwrap_or_else(|| "?".into()),
            appraisal
                .score_to_place
                .map(|s| format!("{s}"))
                .unwrap_or_else(|| "unknown".into()),
        );

        Ok(Attempt {
            niche_id: appraisal.niche_id.clone(),
            actuator: self.id().to_string(),
            mode,
            plan,
            // Even authorised and worthwhile, nothing here can produce a submission on
            // its own: a model still has to exist and be good enough to place.
            outcome: AttemptOutcome::Prepared,
            spent_seconds: 0,
            earned_cents: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_venues::http::FixtureTransport;
    use hl_venues::kaggle::parse_competitions;

    const LB: &str = r#"{"submissions":[{"score":"0.951"},{"score":"0.948"},{"score":"0.937"},{"score":null}]}"#;

    fn comp(reward: &str, teams: u64, deadline: &str) -> Competition {
        comp_full(reward, teams, deadline, "Demo", false)
    }

    fn comp_full(
        reward: &str,
        teams: u64,
        deadline: &str,
        title: &str,
        kernels: bool,
    ) -> Competition {
        parse_competitions(&format!(
            r#"[{{"ref":"https://www.kaggle.com/competitions/demo","title":"{title}",
                 "reward":"{reward}","teamCount":{teams},"deadline":"{deadline}",
                 "isKernelsSubmissionsOnly":{kernels}}}]"#
        ))
        .unwrap()
        .remove(0)
    }

    #[test]
    fn classification_matches_what_the_live_board_looks_like() {
        // Big money behind notebook-only entry, prose tracks, and knowledge comps —
        // the three shapes that make automated entry pointless.
        assert_eq!(
            classify(&comp_full("850,000 Usd", 2382, "2030-01-01T00:00:00Z", "ARC AGI", true)),
            Automatability::NotebookOnly
        );
        assert_eq!(
            classify(&comp_full("450,000 Usd", 142, "2030-01-01T00:00:00Z", "ARC Paper Track", false)),
            Automatability::HumanJudged
        );
        assert_eq!(
            classify(&comp_full("Knowledge", 10, "2030-01-01T00:00:00Z", "Titanic", false)),
            Automatability::NoCashPrize
        );
        assert_eq!(
            classify(&comp_full("50,000 Usd", 5185, "2030-01-01T00:00:00Z", "Kaggriculture", false)),
            Automatability::FileSubmission
        );
    }

    fn actuator() -> KaggleActuator {
        std::env::set_var("KAGGLE_KEY", "test-token");
        let t = FixtureTransport::new().with(KaggleActuator::leaderboard_url("demo"), 200, LB);
        KaggleActuator::new(Box::new(t))
    }

    #[test]
    fn leaderboard_shape_is_read_from_the_visible_top() {
        let s = parse_leaderboard(LB);
        assert_eq!(s.visible_entries, 3, "null scores are not entries");
        assert_eq!(s.best, Some(0.951));
        assert_eq!(s.worst_visible, Some(0.937));
    }

    #[test]
    fn an_unreadable_leaderboard_is_absence_not_zero() {
        let s = parse_leaderboard("not json");
        assert_eq!(s.visible_entries, 0);
        assert_eq!(s.best, None);
    }

    #[test]
    fn appraisal_reports_the_bar_and_refuses_to_invent_a_return() {
        let a = actuator()
            .appraise(&comp("450,000 Usd", 142, "2026-12-01T00:00:00Z"), 0, None)
            .unwrap();
        assert_eq!(a.prize_cents, Some(45_000_000));
        assert_eq!(a.score_to_place, Some(0.937));
        assert!((a.top_spread.unwrap() - 0.014).abs() < 1e-9);
        assert_eq!(
            a.expected_cents, 0.0,
            "no history at this venue means no expectation"
        );
    }

    #[test]
    fn a_worthless_niche_is_refused_even_when_fully_authorised() {
        let mut c = Consent::new();
        c.opt_in("kaggle:demo");
        c.accept_terms("kaggle:demo");
        let act = actuator();
        let a = act
            .appraise(&comp("Knowledge", 10_000, "2030-01-01T00:00:00Z"), 0, Some(0.5))
            .unwrap();
        assert!(matches!(
            act.attempt(&a, &c, ActMode::Live),
            Err(Refusal::NotWorthAttempting { .. })
        ));
    }

    #[test]
    fn a_live_attempt_without_accepted_rules_is_refused() {
        let mut c = Consent::new();
        c.opt_in("kaggle:demo");
        let act = actuator();
        let a = act
            .appraise(&comp("450,000 Usd", 142, "2030-01-01T00:00:00Z"), 0, Some(0.02))
            .unwrap();
        let err = act.attempt(&a, &c, ActMode::Live).unwrap_err();
        assert!(matches!(err, Refusal::TermsNotAccepted { .. }));
        assert!(err.to_string().contains("accept its rules"));
    }

    #[test]
    fn a_dry_run_describes_the_work_without_doing_any_of_it() {
        let act = actuator();
        let a = act
            .appraise(&comp("450,000 Usd", 142, "2030-01-01T00:00:00Z"), 0, Some(0.02))
            .unwrap();
        let att = act.attempt(&a, &Consent::new(), ActMode::DryRun).unwrap();
        assert_eq!(att.outcome, AttemptOutcome::Prepared);
        assert_eq!(att.earned_cents, 0);
        assert!(att.plan.contains("fit a model"));
    }
}

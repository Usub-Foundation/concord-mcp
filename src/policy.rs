//! How a negotiation ends. Pure functions: no I/O, no clock, fully unit-tested.
//!
//! Termination guarantee: every open negotiation has a deadline, every answer either keeps
//! the round number or increases it, rounds are capped by `max_rounds`, and once the deadline
//! has passed `evaluate` never returns `Wait` for an open negotiation. An escalated
//! negotiation has its own (longer) deadline after which nothing changes. So every
//! negotiation reaches a terminal state in bounded time, whatever the agents do or fail to do.

use std::collections::BTreeMap;

use crate::model::{NegotiationStatus, Outcome, Risk, Stance};

#[derive(Debug, Clone)]
pub struct Snapshot<'a> {
    pub status: NegotiationStatus,
    pub risk: Risk,
    pub round: i32,
    pub max_rounds: i32,
    pub participants: &'a [String],
    pub stances: &'a BTreeMap<String, Stance>,
    /// Owner of the area the proposal is about (first-ranked tie-breaker).
    pub owner: Option<&'a str>,
    /// Current assignee of the task the proposal is about (second-ranked tie-breaker).
    pub incumbent: Option<&'a str>,
    pub deadline_passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to do yet.
    Wait,
    /// Apply the offer of `round`.
    Apply { round: i32, outcome: Outcome, why: String },
    /// Close without changing anything.
    StatusQuo { outcome: Outcome, why: String },
    /// Hand the decision to the humans behind the participants.
    Escalate { why: String },
}

pub fn evaluate(s: &Snapshot<'_>) -> Verdict {
    let stance = |m: &str| s.stances.get(m).and_then(|st| st.on(s.round));
    let all_accept = !s.participants.is_empty() && s.participants.iter().all(|m| stance(m) == Some("accept"));

    match s.status {
        NegotiationStatus::Closed => Verdict::Wait,
        NegotiationStatus::Escalated => {
            let confirmed = s
                .participants
                .iter()
                .all(|m| s.stances.get(m).is_some_and(|st| st.on(s.round) == Some("accept") && st.human_confirmed));
            let all_humans_answered = s
                .participants
                .iter()
                .all(|m| s.stances.get(m).is_some_and(|st| st.on(s.round).is_some() && st.human_confirmed));
            if confirmed {
                Verdict::Apply {
                    round: s.round,
                    outcome: Outcome::Humans,
                    why: "the humans of all participants accepted the same offer".into(),
                }
            } else if all_humans_answered {
                Verdict::StatusQuo {
                    outcome: Outcome::Humans,
                    why: "every human answered and at least one rejected the offer; nothing changes".into(),
                }
            } else if s.deadline_passed {
                Verdict::StatusQuo {
                    outcome: Outcome::EscalationTimeout,
                    why: "escalated to humans and not decided before the escalation deadline; nothing changes".into(),
                }
            } else {
                Verdict::Wait
            }
        }
        NegotiationStatus::Open => {
            if all_accept {
                return Verdict::Apply {
                    round: s.round,
                    outcome: Outcome::Agreed,
                    why: "all participants accepted".into(),
                };
            }
            if !s.deadline_passed {
                return Verdict::Wait;
            }
            let rejecters: Vec<&String> = s.participants.iter().filter(|m| stance(m) == Some("reject")).collect();
            let silent: Vec<&String> = s.participants.iter().filter(|m| stance(m).is_none()).collect();

            if s.risk == Risk::Destructive {
                if !rejecters.is_empty() && s.round < s.max_rounds {
                    return Verdict::StatusQuo {
                        outcome: Outcome::StatusQuo,
                        why: format!(
                            "destructive change rejected by {} and not countered before the deadline",
                            join(&rejecters)
                        ),
                    };
                }
                let why = if rejecters.is_empty() {
                    format!("destructive change and {} did not answer: silence is not consent here", join(&silent))
                } else {
                    format!("destructive change still rejected by {} after the last round", join(&rejecters))
                };
                return Verdict::Escalate { why };
            }

            if rejecters.is_empty() {
                return Verdict::Apply {
                    round: s.round,
                    outcome: Outcome::Silence,
                    why: format!("no objection from {} before the deadline (silence = consent)", join(&silent)),
                };
            }

            // Safe change with objections left: owner of the area, then the current assignee,
            // then the status quo.
            let ranked = [("area owner", s.owner), ("current assignee", s.incumbent)];
            for (label, who) in ranked {
                let Some(who) = who else { continue };
                if !s.participants.iter().any(|m| m == who) {
                    continue;
                }
                return match stance(who) {
                    Some("reject") => match s.stances.get(who).and_then(|st| st.position) {
                        Some(r) if r != s.round => Verdict::Apply {
                            round: r,
                            outcome: Outcome::Policy,
                            why: format!(
                                "no agreement; the {label} ({who}) decides and keeps its own offer from round {r}"
                            ),
                        },
                        _ => Verdict::StatusQuo {
                            outcome: Outcome::Policy,
                            why: format!("no agreement; the {label} ({who}) decides and rejected the change"),
                        },
                    },
                    _ => Verdict::Apply {
                        round: s.round,
                        outcome: Outcome::Policy,
                        why: format!("no agreement; the {label} ({who}) decides and supports the current offer"),
                    },
                };
            }
            Verdict::StatusQuo {
                outcome: Outcome::Policy,
                why: format!(
                    "no agreement and neither an area owner nor an assignee takes part; status quo stays (rejected by {})",
                    join(&rejecters)
                ),
            }
        }
    }
}

fn join(v: &[&String]) -> String {
    if v.is_empty() {
        return "nobody".into();
    }
    v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(round: i32, stance: Option<&str>, position: Option<i32>, human: bool) -> Stance {
        Stance { round, stance: stance.map(Into::into), human_confirmed: human, position }
    }

    struct Case {
        status: NegotiationStatus,
        risk: Risk,
        round: i32,
        max_rounds: i32,
        stances: Vec<(&'static str, Stance)>,
        owner: Option<&'static str>,
        incumbent: Option<&'static str>,
        deadline_passed: bool,
    }

    impl Case {
        fn new(risk: Risk) -> Self {
            Case {
                status: NegotiationStatus::Open,
                risk,
                round: 1,
                max_rounds: 4,
                stances: vec![("bob", st(1, Some("accept"), Some(1), false))],
                owner: Some("alice"),
                incumbent: None,
                deadline_passed: false,
            }
        }
        fn eval(&self) -> Verdict {
            let participants: Vec<String> = vec!["alice".into(), "bob".into()];
            let stances: BTreeMap<String, Stance> =
                self.stances.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
            evaluate(&Snapshot {
                status: self.status,
                risk: self.risk,
                round: self.round,
                max_rounds: self.max_rounds,
                participants: &participants,
                stances: &stances,
                owner: self.owner,
                incumbent: self.incumbent,
                deadline_passed: self.deadline_passed,
            })
        }
    }

    #[test]
    fn agreement_applies_immediately() {
        let mut c = Case::new(Risk::Destructive);
        c.stances.push(("alice", st(1, Some("accept"), Some(1), false)));
        assert!(matches!(c.eval(), Verdict::Apply { round: 1, outcome: Outcome::Agreed, .. }));
    }

    #[test]
    fn waits_before_deadline() {
        let c = Case::new(Risk::Safe);
        assert_eq!(c.eval(), Verdict::Wait);
        let mut c = Case::new(Risk::Safe);
        c.stances.push(("alice", st(1, Some("reject"), None, false)));
        assert_eq!(c.eval(), Verdict::Wait);
    }

    #[test]
    fn silence_is_consent_for_safe() {
        let mut c = Case::new(Risk::Safe);
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::Apply { round: 1, outcome: Outcome::Silence, .. }));
    }

    #[test]
    fn silence_escalates_destructive() {
        let mut c = Case::new(Risk::Destructive);
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::Escalate { .. }));
    }

    #[test]
    fn destructive_rejection_not_countered_keeps_status_quo() {
        let mut c = Case::new(Risk::Destructive);
        c.stances.push(("alice", st(1, Some("reject"), None, false)));
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::StatusQuo { outcome: Outcome::StatusQuo, .. }));
    }

    #[test]
    fn destructive_deadlock_after_last_round_escalates() {
        let mut c = Case::new(Risk::Destructive);
        c.round = 4;
        c.stances =
            vec![("bob", st(4, Some("accept"), Some(4), false)), ("alice", st(4, Some("reject"), Some(3), false))];
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::Escalate { .. }));
    }

    #[test]
    fn owner_rejection_wins_for_safe() {
        let mut c = Case::new(Risk::Safe);
        c.stances.push(("alice", st(1, Some("reject"), None, false)));
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::StatusQuo { outcome: Outcome::Policy, .. }));
    }

    #[test]
    fn owner_keeps_its_own_earlier_offer() {
        let mut c = Case::new(Risk::Safe);
        c.round = 3;
        c.stances =
            vec![("bob", st(3, Some("accept"), Some(3), false)), ("alice", st(3, Some("reject"), Some(2), false))];
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::Apply { round: 2, outcome: Outcome::Policy, .. }));
    }

    #[test]
    fn owner_as_author_beats_rejecting_assignee() {
        let mut c = Case::new(Risk::Safe);
        c.owner = Some("bob");
        c.incumbent = Some("alice");
        c.stances.push(("alice", st(1, Some("reject"), None, false)));
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::Apply { round: 1, outcome: Outcome::Policy, .. }));
    }

    #[test]
    fn incumbent_decides_without_owner() {
        let mut c = Case::new(Risk::Safe);
        c.owner = None;
        c.incumbent = Some("alice");
        c.stances.push(("alice", st(1, Some("reject"), None, false)));
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::StatusQuo { outcome: Outcome::Policy, .. }));
    }

    #[test]
    fn nobody_ranked_keeps_status_quo() {
        let mut c = Case::new(Risk::Safe);
        c.owner = Some("carol");
        c.stances.push(("alice", st(1, Some("reject"), None, false)));
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::StatusQuo { outcome: Outcome::Policy, .. }));
    }

    #[test]
    fn escalated_needs_every_human() {
        let mut c = Case::new(Risk::Destructive);
        c.status = NegotiationStatus::Escalated;
        c.stances =
            vec![("bob", st(1, Some("accept"), Some(1), true)), ("alice", st(1, Some("accept"), Some(1), false))];
        assert_eq!(c.eval(), Verdict::Wait);
        c.stances[1].1.human_confirmed = true;
        assert!(matches!(c.eval(), Verdict::Apply { outcome: Outcome::Humans, .. }));
    }

    #[test]
    fn humans_can_say_no() {
        let mut c = Case::new(Risk::Destructive);
        c.status = NegotiationStatus::Escalated;
        c.stances = vec![("bob", st(1, Some("accept"), Some(1), true)), ("alice", st(1, Some("reject"), None, true))];
        assert!(matches!(c.eval(), Verdict::StatusQuo { outcome: Outcome::Humans, .. }));
        c.stances[0].1.human_confirmed = false;
        assert_eq!(c.eval(), Verdict::Wait);
    }

    #[test]
    fn escalation_times_out_to_status_quo() {
        let mut c = Case::new(Risk::Destructive);
        c.status = NegotiationStatus::Escalated;
        c.deadline_passed = true;
        assert!(matches!(c.eval(), Verdict::StatusQuo { outcome: Outcome::EscalationTimeout, .. }));
    }

    /// Exhaustive check of the termination property over every combination of answers.
    #[test]
    fn never_waits_after_deadline() {
        let answers = [None, Some("accept"), Some("reject")];
        for status in [NegotiationStatus::Open, NegotiationStatus::Escalated] {
            for risk in [Risk::Safe, Risk::Destructive] {
                for round in 1..=4 {
                    for a in answers {
                        for b in answers {
                            for owner in [None, Some("alice"), Some("bob"), Some("carol")] {
                                for incumbent in [None, Some("alice"), Some("bob")] {
                                    for human in [false, true] {
                                        let mut c = Case::new(risk);
                                        c.status = status;
                                        c.round = round;
                                        c.owner = owner;
                                        c.incumbent = incumbent;
                                        c.deadline_passed = true;
                                        c.stances = vec![
                                            ("alice", st(round, a, Some(1), human)),
                                            ("bob", st(round, b, Some(round), human)),
                                        ];
                                        let v = c.eval();
                                        assert_ne!(
                                            v,
                                            Verdict::Wait,
                                            "waited: {status:?} {risk:?} r{round} {a:?} {b:?}"
                                        );
                                        if status == NegotiationStatus::Escalated {
                                            assert!(!matches!(v, Verdict::Escalate { .. }));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

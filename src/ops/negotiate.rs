//! Proposals and negotiations: open, answer, settle.

use std::collections::BTreeMap;

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::types::Json;

use super::changes::{self, Analysis};
use super::{MAX_NOTE, check_len, untrusted};
use crate::clock::rfc3339;
use crate::error::{HubError, Result};
use crate::hub::Op;
use crate::model::{Change, Negotiation, NegotiationStatus, Offer, Outcome, Risk, Stance, TaskStatus};
use crate::policy::{self, Snapshot, Verdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Agree with the current offer.
    Accept,
    /// Disagree with the current offer (give the reason in `note`).
    Reject,
    /// Replace the current offer with your own (`changes`); everyone else has to answer again.
    Counter,
    /// Proposer only: drop the proposal, nothing changes.
    Withdraw,
}

/// Opens a negotiation if anyone else has to consent, otherwise applies the changes at once.
pub async fn propose_or_apply(op: &mut Op, proposer: &str, changes: Vec<Change>, rationale: &str) -> Result<Value> {
    check_len("rationale", rationale, MAX_NOTE)?;
    let a = changes::analyze(op, proposer, &changes).await?;
    op.ensure_not_frozen(&a.targets).await?;
    op.ensure_no_open_negotiation(&a.targets, None).await?;
    if a.parties.is_empty() {
        let applied = changes::apply(op, proposer, &changes, None).await?;
        return Ok(json!({ "result": "applied", "applied": applied }));
    }
    let n = open(op, proposer, a, changes, rationale).await?;
    let view = view(op, &n, Some(proposer)).await?;
    Ok(json!({
        "result": "proposed",
        "negotiation": view,
        "hint": "Others must consent. Keep working on something else; sync/wait_for_updates will show the answer.",
    }))
}

pub async fn open(
    op: &mut Op,
    proposer: &str,
    a: Analysis,
    changes: Vec<Change>,
    rationale: &str,
) -> Result<Negotiation> {
    let id = op.next_negotiation_id();
    let mut participants = vec![proposer.to_string()];
    participants.extend(a.parties.iter().cloned());
    let mut stances = BTreeMap::new();
    stances.insert(
        proposer.to_string(),
        Stance { round: 1, stance: Some("accept".into()), human_confirmed: false, position: Some(1) },
    );
    let deadline = op.now + op.ns.turn_timeout();
    let n: Negotiation = sqlx::query_as(
        "INSERT INTO negotiations (namespace_id, id, status, risk, proposer, participants, targets, owner, incumbent,
                                   round, max_rounds, stances, deadline, created_at, updated_at)
         VALUES ($1, $2, 'open', $3, $4, $5, $6, $7, $8, 1, $9, $10, $11, $12, $12)
         RETURNING *",
    )
    .bind(&op.ns.id)
    .bind(&id)
    .bind(a.risk.as_str())
    .bind(proposer)
    .bind(Json(&participants))
    .bind(Json(&a.targets))
    .bind(&a.owner)
    .bind(&a.incumbent)
    .bind(op.ns.max_rounds)
    .bind(Json(&stances))
    .bind(deadline)
    .bind(op.now)
    .fetch_one(&mut *op.tx)
    .await?;
    insert_offer(op, &id, 1, proposer, &changes, a.risk, rationale).await?;
    let awaiting: Vec<String> = a.parties.iter().cloned().collect();
    op.emit(
        "negotiation.opened",
        Some(&id),
        None,
        format!(
            "{proposer} proposes {} — {} risk; waiting for {} until {}",
            describe(&changes),
            a.risk,
            awaiting.join(", "),
            rfc3339(deadline)
        ),
        json!({ "negotiation_id": id, "awaiting": awaiting, "risk": a.risk, "rationale": untrusted(proposer, rationale) }),
    )
    .await?;
    Ok(n)
}

async fn insert_offer(
    op: &mut Op,
    id: &str,
    round: i32,
    author: &str,
    changes: &[Change],
    risk: Risk,
    note: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO negotiation_offers (namespace_id, negotiation_id, round, author, changes, risk, note, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&op.ns.id)
    .bind(id)
    .bind(round)
    .bind(author)
    .bind(Json(changes))
    .bind(risk.as_str())
    .bind(note.trim())
    .bind(op.now)
    .execute(&mut *op.tx)
    .await?;
    Ok(())
}

pub async fn load(op: &mut Op, id: &str) -> Result<Negotiation> {
    let n: Option<Negotiation> = sqlx::query_as("SELECT * FROM negotiations WHERE namespace_id = $1 AND id = $2")
        .bind(&op.ns.id)
        .bind(id.trim())
        .fetch_optional(&mut *op.tx)
        .await?;
    n.ok_or_else(|| HubError::not_found(format!("negotiation {id} not found")))
}

pub async fn offers(conn: &mut sqlx::PgConnection, ns: &str, id: &str) -> Result<Vec<Offer>> {
    Ok(sqlx::query_as(
        "SELECT * FROM negotiation_offers WHERE namespace_id = $1 AND negotiation_id = $2 ORDER BY round",
    )
    .bind(ns)
    .bind(id)
    .fetch_all(conn)
    .await?)
}

async fn offer(op: &mut Op, id: &str, round: i32) -> Result<Offer> {
    let o: Option<Offer> = sqlx::query_as(
        "SELECT * FROM negotiation_offers WHERE namespace_id = $1 AND negotiation_id = $2 AND round = $3",
    )
    .bind(&op.ns.id)
    .bind(id)
    .bind(round)
    .fetch_optional(&mut *op.tx)
    .await?;
    o.ok_or_else(|| HubError::Internal(format!("offer {id}#{round} missing")))
}

async fn save(op: &mut Op, n: &Negotiation) -> Result<()> {
    sqlx::query(
        "UPDATE negotiations SET status = $3, outcome = $4, applied_round = $5, explanation = $6, risk = $7,
                                 participants = $8, targets = $9, round = $10, stances = $11, deadline = $12,
                                 updated_at = $13, closed_at = $14
         WHERE namespace_id = $1 AND id = $2",
    )
    .bind(&op.ns.id)
    .bind(&n.id)
    .bind(&n.status)
    .bind(&n.outcome)
    .bind(n.applied_round)
    .bind(&n.explanation)
    .bind(&n.risk)
    .bind(Json(&n.participants.0))
    .bind(Json(&n.targets.0))
    .bind(n.round)
    .bind(Json(&n.stances.0))
    .bind(n.deadline)
    .bind(op.now)
    .bind(n.closed_at)
    .execute(&mut *op.tx)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn respond(
    op: &mut Op,
    caller: &str,
    id: &str,
    round: i32,
    action: Action,
    changes: Option<Vec<Change>>,
    note: &str,
    human_confirmed: bool,
) -> Result<Value> {
    check_len("note", note, MAX_NOTE)?;
    let mut n = load(op, id).await?;
    if n.status() == NegotiationStatus::Closed {
        let v = view(op, &n, Some(caller)).await?;
        return Err(HubError::conflict(
            format!("negotiation {id} is already closed ({})", n.outcome.as_deref().unwrap_or("closed")),
            v,
        ));
    }
    if !n.participants.iter().any(|p| p == caller) {
        return Err(HubError::forbidden(format!(
            "you are not a participant of {id}; comment with send_message instead"
        )));
    }
    if round != n.round {
        let v = view(op, &n, Some(caller)).await?;
        return Err(HubError::conflict(
            format!("round {round} is stale: the current offer is round {}; read it and answer that", n.round),
            v,
        ));
    }
    let escalated = n.status() == NegotiationStatus::Escalated;
    if escalated && action != Action::Withdraw && !human_confirmed {
        return Err(HubError::conflict(
            format!(
                "{id} is escalated to the humans. Show the offer to your human, then call respond again with \
                 their decision and human_confirmed=true. Do not decide on their behalf."
            ),
            view(op, &n, Some(caller)).await?,
        ));
    }

    let mut stance = n.stances.get(caller).cloned().unwrap_or_default();

    let summary = match action {
        Action::Accept => {
            stance.round = n.round;
            stance.stance = Some("accept".into());
            stance.human_confirmed = human_confirmed;
            stance.position = Some(n.round);
            n.stances.0.insert(caller.to_string(), stance);
            format!("{caller} accepted round {} of {id}", n.round)
        }
        Action::Reject => {
            if note.trim().is_empty() {
                return Err(HubError::invalid("reject needs a reason in `note` (or counter with a better offer)"));
            }
            stance.round = n.round;
            stance.stance = Some("reject".into());
            stance.human_confirmed = human_confirmed;
            n.stances.0.insert(caller.to_string(), stance);
            if !escalated {
                // Give the others a full turn to counter.
                n.deadline = op.now + op.ns.turn_timeout();
            }
            format!("{caller} rejected round {} of {id}", n.round)
        }
        Action::Counter => {
            let Some(changes) = changes.filter(|c| !c.is_empty()) else {
                return Err(HubError::invalid("counter needs `changes` (your alternative offer)"));
            };
            if n.round >= n.max_rounds {
                return Err(HubError::conflict(
                    format!(
                        "round limit reached ({}/{}): only accept or reject now; at the deadline the rules decide",
                        n.round, n.max_rounds
                    ),
                    view(op, &n, Some(caller)).await?,
                ));
            }
            let a = changes::analyze(op, caller, &changes).await?;
            let mut targets = n.targets.0.clone();
            for t in &a.targets {
                if !targets.contains(t) {
                    targets.push(t.clone());
                }
            }
            op.ensure_no_open_negotiation(&a.targets, Some(&n.id)).await?;
            for p in a.parties.iter() {
                if !n.participants.contains(p) {
                    n.participants.0.push(p.clone());
                }
            }
            n.targets = Json(targets);
            n.round += 1;
            n.risk = a.risk.as_str().into();
            insert_offer(op, &n.id, n.round, caller, &changes, a.risk, note).await?;
            n.stances.0.insert(
                caller.to_string(),
                Stance { round: n.round, stance: Some("accept".into()), human_confirmed, position: Some(n.round) },
            );
            if escalated && a.risk == Risk::Safe {
                n.status = NegotiationStatus::Open.as_str().into();
            }
            if n.status() == NegotiationStatus::Open {
                n.deadline = op.now + op.ns.turn_timeout();
            }
            format!("{caller} countered {id} with round {}: {}", n.round, describe(&changes))
        }
        Action::Withdraw => {
            if caller != n.proposer {
                return Err(HubError::forbidden("only the proposer can withdraw; reject or counter instead"));
            }
            format!("{caller} withdrew {id}")
        }
    };

    op.emit(
        "negotiation.answered",
        Some(&n.id),
        None,
        &summary,
        json!({
            "negotiation_id": n.id,
            "action": action,
            "round": n.round,
            "human_confirmed": human_confirmed,
            "note": untrusted(caller, note),
        }),
    )
    .await?;

    if action == Action::Withdraw {
        close(op, &mut n, Outcome::Withdrawn, None, "withdrawn by the proposer".into()).await?;
    } else {
        let verdict = evaluate(&n, false);
        settle(op, &mut n, verdict).await?;
    }
    save(op, &n).await?;
    view(op, &n, Some(caller)).await
}

fn evaluate(n: &Negotiation, deadline_passed: bool) -> Verdict {
    policy::evaluate(&Snapshot {
        status: n.status(),
        risk: n.risk(),
        round: n.round,
        max_rounds: n.max_rounds,
        participants: &n.participants.0,
        stances: &n.stances.0,
        owner: n.owner.as_deref(),
        incumbent: n.incumbent.as_deref(),
        deadline_passed,
    })
}

/// Carries out a verdict. Does not persist `n` (the caller saves it).
async fn settle(op: &mut Op, n: &mut Negotiation, verdict: Verdict) -> Result<()> {
    match verdict {
        Verdict::Wait => Ok(()),
        Verdict::Apply { round, outcome, why } => apply_round(op, n, round, outcome, why).await,
        Verdict::StatusQuo { outcome, why } => close(op, n, outcome, None, why).await,
        Verdict::Escalate { why } => {
            n.status = NegotiationStatus::Escalated.as_str().into();
            n.deadline = op.now + op.ns.escalation_timeout();
            n.explanation = why.clone();
            let participants = n.participants.0.clone();
            op.emit(
                "negotiation.escalated",
                Some(&n.id),
                None,
                format!(
                    "{} escalated to the humans ({why}). Every participant must ask their human and answer with \
                     human_confirmed=true before {}; otherwise nothing changes",
                    n.id,
                    rfc3339(n.deadline)
                ),
                json!({ "negotiation_id": n.id, "ask_humans_of": participants }),
            )
            .await?;
            Ok(())
        }
    }
}

async fn apply_round(op: &mut Op, n: &mut Negotiation, round: i32, outcome: Outcome, why: String) -> Result<()> {
    let offer = offer(op, &n.id, round).await?;
    sqlx::query("SAVEPOINT concord_apply").execute(&mut *op.tx).await?;
    let saved_ws = op.ns.clone();
    match changes::apply(op, &offer.author, &offer.changes.0, Some(&n.id)).await {
        Ok(applied) => {
            sqlx::query("RELEASE SAVEPOINT concord_apply").execute(&mut *op.tx).await?;
            let why = format!("{why}; applied round {round}: {}", applied.join("; "));
            close(op, n, outcome, Some(round), why).await
        }
        Err(e @ (HubError::Db(_) | HubError::Internal(_))) => Err(e),
        Err(e) => {
            sqlx::query("ROLLBACK TO SAVEPOINT concord_apply").execute(&mut *op.tx).await?;
            op.ns = saved_ws;
            close(op, n, Outcome::Stale, None, format!("{why}, but round {round} no longer applies: {e}")).await
        }
    }
}

async fn close(op: &mut Op, n: &mut Negotiation, outcome: Outcome, round: Option<i32>, why: String) -> Result<()> {
    n.status = NegotiationStatus::Closed.as_str().into();
    n.outcome = Some(outcome.as_str().into());
    n.applied_round = round;
    n.explanation = why.clone();
    n.closed_at = Some(op.now);
    if round.is_none() {
        reject_unaccepted_tasks(op, n).await?;
    }
    let first = offer(op, &n.id, 1).await?;
    op.decide(
        Some(&n.id),
        outcome.as_str(),
        format!("{} ({} by {}): {why}", n.id, describe(&first.changes.0), n.proposer),
    )
    .await?;
    Ok(())
}

/// A task filed into someone's area that was never accepted is marked rejected.
async fn reject_unaccepted_tasks(op: &mut Op, n: &Negotiation) -> Result<()> {
    let first = offer(op, &n.id, 1).await?;
    for c in &first.changes.0 {
        if let Change::AcceptTask { task_id } = c
            && let Some(mut t) = op.find_task(task_id).await?
            && t.status() == TaskStatus::Proposed
        {
            t.status = TaskStatus::Rejected.as_str().into();
            super::save_task(op, &mut t).await?;
            op.emit(
                "task.rejected",
                Some(&t.id),
                None,
                format!("{} was not accepted ({})", super::task_ref(&t), n.id),
                json!({ "negotiation_id": n.id }),
            )
            .await?;
        }
    }
    Ok(())
}

/// Settles every negotiation of this namespace whose deadline has passed.
pub async fn settle_due(op: &mut Op) -> Result<usize> {
    let due: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM negotiations WHERE namespace_id = $1 AND status <> 'closed' AND deadline <= $2 ORDER BY id",
    )
    .bind(&op.ns.id)
    .bind(op.now)
    .fetch_all(&mut *op.tx)
    .await?;
    let mut count = 0;
    for (id,) in due {
        let mut n = load(op, &id).await?;
        let verdict = evaluate(&n, true);
        if verdict != Verdict::Wait {
            settle(op, &mut n, verdict).await?;
            save(op, &n).await?;
            count += 1;
        }
    }
    Ok(count)
}

/// Hub operator decision: apply the offer of `round`, or keep the status quo (`None`).
pub async fn admin_resolve(op: &mut Op, id: &str, round: Option<i32>, note: &str) -> Result<Value> {
    let mut n = load(op, id).await?;
    if n.status() == NegotiationStatus::Closed {
        return Err(HubError::conflict(format!("{id} is already closed"), json!({})));
    }
    let why = if note.trim().is_empty() {
        "decided by the hub operator".to_string()
    } else {
        format!("decided by the hub operator: {}", note.trim())
    };
    match round {
        Some(r) if r < 1 || r > n.round => return Err(HubError::invalid(format!("{id} has rounds 1..={}", n.round))),
        Some(r) => apply_round(op, &mut n, r, Outcome::Admin, why).await?,
        None => close(op, &mut n, Outcome::Admin, None, why).await?,
    }
    save(op, &n).await?;
    view(op, &n, None).await
}

/// Short text for a list of changes, e.g. "assign T-3 to bob; cancel T-4".
pub fn describe(changes: &[Change]) -> String {
    let parts: Vec<String> = changes
        .iter()
        .map(|c| match c {
            Change::AcceptTask { task_id } => format!("accept {task_id} into the plan"),
            Change::UpdateTask { task_id, patch } => {
                let mut f = vec![];
                if patch.title.is_some() {
                    f.push("title")
                }
                if patch.description.is_some() {
                    f.push("description")
                }
                if patch.acceptance.is_some() {
                    f.push("acceptance")
                }
                if patch.area.is_some() {
                    f.push("area")
                }
                if patch.milestone.is_some() {
                    f.push("milestone")
                }
                if patch.kind.is_some() {
                    f.push("kind")
                }
                if patch.priority.is_some() {
                    f.push("priority")
                }
                if patch.depends_on.is_some() {
                    f.push("depends_on")
                }
                format!("edit {task_id} ({})", f.join(", "))
            }
            Change::Assign { task_id, to } => format!("assign {task_id} to {to}"),
            Change::Split { task_id, parts } => format!("split {task_id} into {} tasks", parts.len()),
            Change::CancelTask { task_id, .. } => format!("cancel {task_id}"),
            Change::SetGoal { .. } => "replace the plan goal".into(),
            Change::AddMilestone { title, .. } => {
                format!("add milestone \"{}\"", title.chars().take(60).collect::<String>())
            }
            Change::UpdateMilestone { milestone_id, .. } => format!("edit milestone {milestone_id}"),
            Change::RemoveMilestone { milestone_id } => format!("remove milestone {milestone_id}"),
            Change::SetArea { name, owner, shared, .. } => match (owner, shared) {
                (_, true) => format!("make area {name} shared"),
                (Some(o), _) => format!("set area {name} owner to {o}"),
                (None, _) => format!("edit area {name}"),
            },
        })
        .collect();
    parts.join("; ")
}

fn rule_text(n: &Negotiation) -> String {
    match n.status() {
        NegotiationStatus::Closed => {
            format!("closed: {} — {}", n.outcome.as_deref().unwrap_or("closed"), n.explanation)
        }
        NegotiationStatus::Escalated => format!(
            "Escalated to the humans: applied only if every participant answers accept with human_confirmed=true \
             on the same round before {}; otherwise nothing changes.",
            rfc3339(n.deadline)
        ),
        NegotiationStatus::Open => match n.risk() {
            Risk::Safe => {
                let owner = n.owner.as_deref().map(|o| format!("the area owner ({o})"));
                let inc = n.incumbent.as_deref().map(|i| format!("the current assignee ({i})"));
                let chain: Vec<String> = [owner, inc].into_iter().flatten().collect();
                format!(
                    "Safe change. If nobody objects by {} it is applied (silence = consent). If objections remain, \
                     {} decides; otherwise the status quo stays.",
                    rfc3339(n.deadline),
                    if chain.is_empty() { "nobody ranks higher, so".into() } else { chain.join(", then ") }
                )
            }
            Risk::Destructive => format!(
                "Destructive change: never applied by silence. If not accepted by everyone by {}, a rejection that \
                 was not countered keeps the status quo; otherwise it goes to the humans.",
                rfc3339(n.deadline)
            ),
        },
    }
}

/// JSON view of a negotiation for agents.
pub async fn view(op: &mut Op, n: &Negotiation, viewer: Option<&str>) -> Result<Value> {
    let ns = op.ns.id.clone();
    view_with(&mut op.tx, &ns, n, viewer, false).await
}

pub async fn view_with(
    conn: &mut sqlx::PgConnection,
    ns: &str,
    n: &Negotiation,
    viewer: Option<&str>,
    history: bool,
) -> Result<Value> {
    let offers = offers(conn, ns, &n.id).await?;
    let current = offers.iter().find(|o| o.round == n.round);
    let offer_json = |o: &Offer| {
        json!({
            "round": o.round,
            "author": o.author,
            "risk": o.risk,
            "changes": o.changes.0,
            "note": untrusted(&o.author, &o.note),
        })
    };
    let stances: BTreeMap<&String, Value> = n
        .stances
        .iter()
        .map(|(m, s)| {
            (m, json!({ "on_current_round": s.on(n.round), "human_confirmed": s.human_confirmed, "position_round": s.position }))
        })
        .collect();
    let mut v = json!({
        "id": n.id,
        "status": n.status,
        "outcome": n.outcome,
        "applied_round": n.applied_round,
        "risk": n.risk,
        "proposer": n.proposer,
        "participants": n.participants.0,
        "awaiting": n.awaiting(),
        "round": n.round,
        "max_rounds": n.max_rounds,
        "deadline": rfc3339(n.deadline),
        "targets": n.targets.0,
        "current_offer": current.map(offer_json),
        "stances": stances,
        "rule": rule_text(n),
    });
    if let Some(viewer) = viewer {
        let must_answer = n.awaiting().iter().any(|m| m == viewer);
        v["you_must_answer"] = json!(must_answer);
        if must_answer {
            v["how_to_answer"] = json!(format!(
                "respond(negotiation_id=\"{}\", round={}, action=accept|reject|counter{})",
                n.id,
                n.round,
                if n.status() == NegotiationStatus::Escalated {
                    ", human_confirmed=true after asking your human"
                } else {
                    ""
                }
            ));
        }
    }
    if history {
        v["offers"] = json!(offers.iter().map(offer_json).collect::<Vec<_>>());
    }
    Ok(v)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NegotiationFilter {
    /// open or escalated (default)
    #[default]
    Active,
    Open,
    Escalated,
    Closed,
    All,
}

pub async fn list(
    conn: &mut sqlx::PgConnection,
    ns: &str,
    caller: &str,
    id: Option<&str>,
    filter: NegotiationFilter,
    mine: bool,
    limit: i64,
) -> Result<Value> {
    if let Some(id) = id {
        let n: Option<Negotiation> = sqlx::query_as("SELECT * FROM negotiations WHERE namespace_id = $1 AND id = $2")
            .bind(ns)
            .bind(id.trim())
            .fetch_optional(&mut *conn)
            .await?;
        let n = n.ok_or_else(|| HubError::not_found(format!("negotiation {id} not found")))?;
        let v = view_with(conn, ns, &n, Some(caller), true).await?;
        return Ok(json!({ "negotiation": v }));
    }
    let statuses: Vec<&str> = match filter {
        NegotiationFilter::Active => vec!["open", "escalated"],
        NegotiationFilter::Open => vec!["open"],
        NegotiationFilter::Escalated => vec!["escalated"],
        NegotiationFilter::Closed => vec!["closed"],
        NegotiationFilter::All => vec!["open", "escalated", "closed"],
    };
    let rows: Vec<Negotiation> = sqlx::query_as(
        "SELECT * FROM negotiations WHERE namespace_id = $1 AND status = ANY($2)
           AND (NOT $3 OR participants ? $4)
         ORDER BY created_at DESC LIMIT $5",
    )
    .bind(ns)
    .bind(&statuses)
    .bind(mine)
    .bind(caller)
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *conn)
    .await?;
    let mut out = vec![];
    for n in &rows {
        out.push(view_with(&mut *conn, ns, n, Some(caller), false).await?);
    }
    Ok(json!({ "negotiations": out }))
}

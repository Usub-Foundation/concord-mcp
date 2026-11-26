//! The plan: goal, areas, milestones, overview and markdown export.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::changes;
use super::negotiate::{describe, propose_or_apply};
use super::{MAX_NOTE, non_empty};
use crate::clock::rfc3339;
use crate::error::{HubError, Result};
use crate::hub::Op;
use crate::model::{
    Area, Change, Decision, Member, Milestone, MilestoneStatus, Namespace, Negotiation, Offer, Task, TaskStatus,
};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlanOp {
    /// Replace the goal (needs the leads' consent once a goal exists).
    SetGoal { goal: String },
    AddMilestone {
        title: String,
        #[serde(default)]
        description: String,
    },
    UpdateMilestone {
        milestone_id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        status: Option<MilestoneStatus>,
    },
    /// Needs the leads' consent.
    RemoveMilestone { milestone_id: String },
    /// Create an area or change it. Changing someone else's area needs their consent.
    SetArea {
        name: String,
        #[serde(default)]
        owner: Option<String>,
        #[serde(default)]
        shared: bool,
        #[serde(default)]
        paths: Option<Vec<String>>,
        #[serde(default)]
        description: Option<String>,
    },
    /// Add a line to the plan's decision log.
    AddNote { text: String },
}

impl PlanOp {
    fn into_change(self) -> Option<Change> {
        Some(match self {
            PlanOp::SetGoal { goal } => Change::SetGoal { goal },
            PlanOp::AddMilestone { title, description } => Change::AddMilestone { title, description },
            PlanOp::UpdateMilestone { milestone_id, title, description, status } => {
                Change::UpdateMilestone { milestone_id, title, description, status }
            }
            PlanOp::RemoveMilestone { milestone_id } => Change::RemoveMilestone { milestone_id },
            PlanOp::SetArea { name, owner, shared, paths, description } => {
                Change::SetArea { name, owner, shared, paths, description }
            }
            PlanOp::AddNote { .. } => return None,
        })
    }
}

/// Applies what the caller may change alone and bundles the rest into one proposal.
pub async fn update(
    op: &mut Op,
    caller: &str,
    expected: Option<i64>,
    ops: Vec<PlanOp>,
    rationale: &str,
) -> Result<Value> {
    if let Some(v) = expected
        && v != op.ns.plan_version
    {
        return Err(HubError::conflict(
            format!("the plan changed: you have version {v}, current is {}; re-read it", op.ns.plan_version),
            json!({ "plan_version": op.ns.plan_version }),
        ));
    }
    if ops.is_empty() {
        return Err(HubError::invalid("no ops given"));
    }
    let mut applied = vec![];
    let mut pending = vec![];
    for o in ops {
        if let PlanOp::AddNote { text } = &o {
            let text = non_empty("text", text, MAX_NOTE)?;
            op.decide(None, "note", format!("{caller}: {text}")).await?;
            applied.push("note added to the decision log".to_string());
            continue;
        }
        let change = o.into_change().expect("non-note op");
        let a = changes::analyze(op, caller, std::slice::from_ref(&change)).await?;
        if a.parties.is_empty() {
            op.ensure_not_frozen(&a.targets).await?;
            op.ensure_no_open_negotiation(&a.targets, None).await?;
            applied.extend(changes::apply(op, caller, &[change], None).await?);
        } else {
            pending.push(change);
        }
    }
    let proposal =
        if pending.is_empty() { None } else { Some(propose_or_apply(op, caller, pending, rationale).await?) };
    Ok(json!({ "applied": applied, "proposal": proposal, "plan_version": op.ns.plan_version }))
}

pub struct PlanData {
    pub ns: Namespace,
    pub members: Vec<Member>,
    pub areas: Vec<Area>,
    pub milestones: Vec<Milestone>,
    pub tasks: Vec<Task>,
    pub negotiations: Vec<Negotiation>,
    pub first_offers: BTreeMap<String, Offer>,
    pub decisions: Vec<Decision>,
}

pub async fn load(conn: &mut PgConnection, ns_id: &str, include_closed: bool) -> Result<PlanData> {
    let ns: Option<Namespace> =
        sqlx::query_as("SELECT * FROM namespaces WHERE id = $1").bind(ns_id).fetch_optional(&mut *conn).await?;
    let ns = ns.ok_or_else(|| HubError::not_found(format!("namespace {ns_id} not found")))?;
    let members: Vec<Member> = sqlx::query_as("SELECT * FROM members WHERE namespace_id = $1 AND active ORDER BY id")
        .bind(ns_id)
        .fetch_all(&mut *conn)
        .await?;
    let areas: Vec<Area> = sqlx::query_as("SELECT * FROM areas WHERE namespace_id = $1 ORDER BY name")
        .bind(ns_id)
        .fetch_all(&mut *conn)
        .await?;
    let milestones: Vec<Milestone> =
        sqlx::query_as("SELECT * FROM milestones WHERE namespace_id = $1 ORDER BY position")
            .bind(ns_id)
            .fetch_all(&mut *conn)
            .await?;
    let tasks: Vec<Task> = sqlx::query_as(
        "SELECT * FROM tasks WHERE namespace_id = $1 AND ($2 OR status NOT IN ('cancelled', 'rejected'))
         ORDER BY created_at, id",
    )
    .bind(ns_id)
    .bind(include_closed)
    .fetch_all(&mut *conn)
    .await?;
    let negotiations: Vec<Negotiation> =
        sqlx::query_as("SELECT * FROM negotiations WHERE namespace_id = $1 AND status <> 'closed' ORDER BY created_at")
            .bind(ns_id)
            .fetch_all(&mut *conn)
            .await?;
    let offers: Vec<(String, Offer)> = sqlx::query_as::<_, OfferWithId>(
        "SELECT o.negotiation_id, o.round, o.author, o.changes, o.risk, o.note, o.created_at
         FROM negotiation_offers o JOIN negotiations n ON n.namespace_id = o.namespace_id AND n.id = o.negotiation_id
         WHERE o.namespace_id = $1 AND n.status <> 'closed' AND o.round = 1",
    )
    .bind(ns_id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|o| (o.negotiation_id.clone(), o.into_offer()))
    .collect();
    let decisions: Vec<Decision> = sqlx::query_as(
        "SELECT seq, negotiation_id, outcome, summary, actor, created_at FROM decisions
         WHERE namespace_id = $1 ORDER BY seq DESC LIMIT 20",
    )
    .bind(ns_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(PlanData {
        ns,
        members,
        areas,
        milestones,
        tasks,
        negotiations,
        first_offers: offers.into_iter().collect(),
        decisions,
    })
}

#[derive(sqlx::FromRow)]
struct OfferWithId {
    negotiation_id: String,
    round: i32,
    author: String,
    changes: sqlx::types::Json<Vec<Change>>,
    risk: String,
    note: String,
    created_at: time::OffsetDateTime,
}

impl OfferWithId {
    fn into_offer(self) -> Offer {
        Offer {
            round: self.round,
            author: self.author,
            changes: self.changes,
            risk: self.risk,
            note: self.note,
            created_at: self.created_at,
        }
    }
}

fn load_of(tasks: &[Task], member: &str) -> usize {
    tasks.iter().filter(|t| t.assignee.as_deref() == Some(member) && t.status().is_held()).count()
}

pub fn team(p: &PlanData) -> Vec<Value> {
    p.members
        .iter()
        .map(|m| {
            let areas: Vec<&str> =
                p.areas.iter().filter(|a| a.owner.as_deref() == Some(m.id.as_str())).map(|a| a.name.as_str()).collect();
            json!({
                "id": m.id,
                "display_name": m.display_name,
                "human": m.human,
                "role": m.role,
                "areas": areas,
                "active_tasks": load_of(&p.tasks, &m.id),
                "last_seen_at": m.last_seen_at.map(rfc3339),
            })
        })
        .collect()
}

pub fn negotiation_brief(p: &PlanData, n: &Negotiation) -> Value {
    json!({
        "id": n.id,
        "status": n.status,
        "risk": n.risk,
        "proposer": n.proposer,
        "about": p.first_offers.get(&n.id).map(|o| describe(&o.changes.0)),
        "round": n.round,
        "awaiting": n.awaiting(),
        "deadline": rfc3339(n.deadline),
    })
}

pub fn to_json(p: &PlanData) -> Value {
    let brief = super::tasks::brief;
    let milestones: Vec<Value> = p
        .milestones
        .iter()
        .map(|m| {
            let ts: Vec<&Task> = p.tasks.iter().filter(|t| t.milestone.as_deref() == Some(m.id.as_str())).collect();
            json!({
                "id": m.id,
                "title": m.title,
                "description": m.description,
                "status": m.status,
                "tasks": ts.iter().map(|t| brief(t)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let unscheduled: Vec<Value> = p.tasks.iter().filter(|t| t.milestone.is_none()).map(brief).collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for t in &p.tasks {
        *counts.entry(t.status.as_str()).or_default() += 1;
    }
    json!({
        "namespace": { "id": p.ns.id, "name": p.ns.name, "policies": p.ns.policies() },
        "goal": p.ns.goal,
        "plan_version": p.ns.plan_version,
        "team": team(p),
        "areas": p.areas,
        "milestones": milestones,
        "unscheduled_tasks": unscheduled,
        "task_counts": counts,
        "open_negotiations": p.negotiations.iter().map(|n| negotiation_brief(p, n)).collect::<Vec<_>>(),
        "recent_decisions": p.decisions,
    })
}

fn status_mark(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Done => "[x]",
        TaskStatus::Cancelled | TaskStatus::Rejected => "[-]",
        TaskStatus::InProgress | TaskStatus::Review => "[~]",
        TaskStatus::Blocked => "[!]",
        TaskStatus::Proposed => "[?]",
        _ => "[ ]",
    }
}

fn md_task(out: &mut String, t: &Task) {
    let who = t.assignee.as_deref().map(|a| format!(" — @{a}")).unwrap_or_default();
    let area = t.area.as_deref().map(|a| format!(" `{a}`")).unwrap_or_default();
    let deps = if t.depends_on.is_empty() { String::new() } else { format!(" (after {})", t.depends_on.join(", ")) };
    let _ = writeln!(
        out,
        "- {} **{}** {} _{}_ {}{}{}{}",
        status_mark(t.status()),
        t.id,
        t.title.replace('\n', " "),
        t.status,
        t.priority,
        area,
        who,
        deps
    );
}

pub fn to_markdown(p: &PlanData) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {}\n", p.ns.name);
    let _ = writeln!(out, "_Plan version {} · generated by concord-mcp_\n", p.ns.plan_version);
    let _ = writeln!(
        out,
        "## Goal\n\n{}\n",
        if p.ns.goal.trim().is_empty() { "_(not set yet)_" } else { p.ns.goal.trim() }
    );
    let _ = writeln!(out, "## Team\n\n| member | human | role | areas | active tasks |\n|---|---|---|---|---|");
    for m in team(p) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            m["id"].as_str().unwrap_or(""),
            m["human"].as_str().unwrap_or(""),
            m["role"].as_str().unwrap_or(""),
            m["areas"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_default(),
            m["active_tasks"]
        );
    }
    if !p.areas.is_empty() {
        let _ = writeln!(out, "\n## Areas\n");
        for a in &p.areas {
            let _ = writeln!(
                out,
                "- **{}** — {}{}{}",
                a.name,
                a.owner.as_deref().map(|o| format!("owner @{o}")).unwrap_or_else(|| "shared".into()),
                if a.paths.is_empty() { String::new() } else { format!(" · `{}`", a.paths.join("`, `")) },
                if a.description.is_empty() { String::new() } else { format!(" · {}", a.description) }
            );
        }
    }
    for m in &p.milestones {
        let _ = writeln!(out, "\n## {} {} ({})\n", m.id, m.title, m.status);
        if !m.description.is_empty() {
            let _ = writeln!(out, "{}\n", m.description);
        }
        for t in p.tasks.iter().filter(|t| t.milestone.as_deref() == Some(m.id.as_str())) {
            md_task(&mut out, t);
        }
    }
    let rest: Vec<&Task> = p.tasks.iter().filter(|t| t.milestone.is_none()).collect();
    if !rest.is_empty() {
        let _ = writeln!(out, "\n## Unscheduled\n");
        for t in rest {
            md_task(&mut out, t);
        }
    }
    if !p.negotiations.is_empty() {
        let _ = writeln!(out, "\n## Open negotiations\n");
        for n in &p.negotiations {
            let about = p.first_offers.get(&n.id).map(|o| describe(&o.changes.0)).unwrap_or_default();
            let _ = writeln!(
                out,
                "- **{}** ({}, {}) {} proposes: {} — round {}/{}, waiting for {} until {}",
                n.id,
                n.status,
                n.risk,
                n.proposer,
                about,
                n.round,
                n.max_rounds,
                n.awaiting().join(", "),
                rfc3339(n.deadline)
            );
        }
    }
    if !p.decisions.is_empty() {
        let _ = writeln!(out, "\n## Recent decisions\n");
        for d in &p.decisions {
            let _ = writeln!(out, "- {} · {} · {}", rfc3339(d.created_at), d.outcome, d.summary.replace('\n', " "));
        }
    }
    out
}

pub async fn whoami(conn: &mut PgConnection, ns: &str, member: &str) -> Result<Value> {
    let p = load(conn, ns, false).await?;
    let me = p.members.iter().find(|m| m.id == member).cloned();
    let my_areas: Vec<&Area> = p.areas.iter().filter(|a| a.owner.as_deref() == Some(member)).collect();
    Ok(json!({
        "namespace": { "id": p.ns.id, "name": p.ns.name, "goal": p.ns.goal, "plan_version": p.ns.plan_version, "policies": p.ns.policies() },
        "you": me,
        "your_areas": my_areas,
        "team": team(&p),
        "cursor": p.ns.event_seq,
        "next": "Call sync(since=cursor) at the start of every work session and before picking new work.",
    }))
}

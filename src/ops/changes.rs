//! Who has to consent to a change, how risky it is, and how it is applied.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use sqlx::types::Json;

use super::{
    MAX_CHANGES, MAX_NOTE, MAX_TEXT, MAX_TITLE, check_len, ensure_deps, ensure_milestone, insert_task, non_empty,
    resolve_area, save_task, task_ref,
};
use crate::error::{HubError, Result};
use crate::hub::Op;
use crate::model::{Change, NewTask, Risk, Task, TaskPatch, TaskStatus};

#[derive(Debug, Clone)]
pub struct Analysis {
    /// Members whose consent is needed (never includes the proposer).
    pub parties: BTreeSet<String>,
    pub risk: Risk,
    pub targets: Vec<String>,
    /// Tie-breakers, taken from the first change.
    pub owner: Option<String>,
    pub incumbent: Option<String>,
}

struct One {
    parties: Vec<Option<String>>,
    risk: Risk,
    owner: Option<String>,
    incumbent: Option<String>,
}

pub async fn analyze(op: &mut Op, proposer: &str, changes: &[Change]) -> Result<Analysis> {
    if changes.is_empty() {
        return Err(HubError::invalid("a proposal needs at least one change"));
    }
    if changes.len() > MAX_CHANGES {
        return Err(HubError::invalid(format!("too many changes in one proposal (max {MAX_CHANGES})")));
    }
    let mut out =
        Analysis { parties: BTreeSet::new(), risk: Risk::Safe, targets: vec![], owner: None, incumbent: None };
    for (i, c) in changes.iter().enumerate() {
        let one = analyze_one(op, proposer, c).await?;
        if i == 0 {
            out.owner = one.owner;
            out.incumbent = one.incumbent;
        }
        out.parties.extend(one.parties.into_iter().flatten());
        out.risk = out.risk.max(one.risk);
        for t in c.targets() {
            if !out.targets.contains(&t) {
                out.targets.push(t);
            }
        }
    }
    out.parties.remove(proposer);
    // Only active members can be asked; a removed member's consent is not required.
    let active: BTreeSet<String> = op.active_members().await?.into_iter().map(|m| m.id).collect();
    out.parties.retain(|p| active.contains(p));
    Ok(out)
}

async fn open_task(op: &mut Op, id: &str) -> Result<Task> {
    let t = op.task(id).await?;
    if t.status().is_closed() {
        return Err(HubError::invalid(format!("task {id} is {}; create a follow-up task instead", t.status)));
    }
    Ok(t)
}

async fn analyze_one(op: &mut Op, proposer: &str, c: &Change) -> Result<One> {
    Ok(match c {
        Change::AcceptTask { task_id } => {
            let t = op.task(task_id).await?;
            if t.status() != TaskStatus::Proposed {
                return Err(HubError::invalid(format!("task {task_id} is not proposed (status {})", t.status)));
            }
            let owner = op.area_owner(t.area.as_deref()).await?;
            One { parties: vec![owner.clone()], risk: Risk::Safe, owner, incumbent: None }
        }
        Change::UpdateTask { task_id, patch } => {
            let t = open_task(op, task_id).await?;
            validate_patch(op, &t, patch).await?;
            let owner = op.area_owner(t.area.as_deref()).await?;
            let mut parties = vec![owner.clone(), t.assignee.clone()];
            if let Some(a) = patch.area.as_deref().filter(|a| !a.is_empty()) {
                parties.push(op.area_owner(Some(a)).await?);
            }
            let in_work = matches!(t.status(), TaskStatus::InProgress | TaskStatus::Review);
            let someone_elses = t.assignee.as_deref().is_some_and(|a| a != proposer);
            let risk = if patch.touches_content() && in_work && someone_elses { Risk::Destructive } else { Risk::Safe };
            One { parties, risk, owner, incumbent: t.assignee.clone() }
        }
        Change::Assign { task_id, to } => {
            let t = open_task(op, task_id).await?;
            op.active_member(to).await?;
            let owner = op.area_owner(t.area.as_deref()).await?;
            One { parties: vec![t.assignee.clone(), Some(to.clone())], risk: Risk::Safe, owner, incumbent: t.assignee }
        }
        Change::Split { task_id, parts } => {
            let t = open_task(op, task_id).await?;
            if parts.len() < 2 || parts.len() > 20 {
                return Err(HubError::invalid("split needs between 2 and 20 parts"));
            }
            let owner = op.area_owner(t.area.as_deref()).await?;
            let mut parties = vec![owner.clone(), t.assignee.clone()];
            for p in parts {
                validate_new_task(op, p).await?;
                let area = match (p.area.as_deref(), p.paths.as_deref()) {
                    (None, None) => t.area.clone(),
                    (a, ps) => resolve_area(op, a, ps).await?,
                };
                parties.push(op.area_owner(area.as_deref()).await?);
                parties.push(p.assignee.clone());
            }
            let in_work = matches!(t.status(), TaskStatus::InProgress | TaskStatus::Review);
            let someone_elses = t.assignee.as_deref().is_some_and(|a| a != proposer);
            let risk = if in_work && someone_elses { Risk::Destructive } else { Risk::Safe };
            One { parties, risk, owner, incumbent: t.assignee }
        }
        Change::CancelTask { task_id, reason } => {
            non_empty("reason", reason, MAX_NOTE)?;
            let t = open_task(op, task_id).await?;
            let owner = op.area_owner(t.area.as_deref()).await?;
            let parties = vec![owner.clone(), t.assignee.clone(), Some(t.created_by.clone())];
            One { parties, risk: Risk::Destructive, owner, incumbent: t.assignee }
        }
        Change::SetGoal { goal } => {
            non_empty("goal", goal, MAX_TEXT)?;
            let parties = op.approvers().await?.into_iter().map(Some).collect();
            let risk = if op.ns.goal.trim().is_empty() { Risk::Safe } else { Risk::Destructive };
            One { parties, risk, owner: None, incumbent: None }
        }
        Change::AddMilestone { title, description } => {
            non_empty("title", title, MAX_TITLE)?;
            check_len("description", description, MAX_TEXT)?;
            One { parties: vec![], risk: Risk::Safe, owner: None, incumbent: None }
        }
        Change::UpdateMilestone { milestone_id, title, description, .. } => {
            ensure_milestone(op, milestone_id).await?;
            if let Some(t) = title {
                non_empty("title", t, MAX_TITLE)?;
            }
            if let Some(d) = description {
                check_len("description", d, MAX_TEXT)?;
            }
            One { parties: vec![], risk: Risk::Safe, owner: None, incumbent: None }
        }
        Change::RemoveMilestone { milestone_id } => {
            ensure_milestone(op, milestone_id).await?;
            let parties = op.approvers().await?.into_iter().map(Some).collect();
            One { parties, risk: Risk::Destructive, owner: None, incumbent: None }
        }
        Change::SetArea { name, owner, shared, paths, description } => {
            super::valid_slug("area", name)?;
            if let Some(o) = owner {
                op.active_member(o).await?;
            }
            if let Some(d) = description {
                check_len("description", d, MAX_NOTE)?;
            }
            if let Some(p) = paths
                && p.len() > 50
            {
                return Err(HubError::invalid("too many path globs (max 50)"));
            }
            let current = op.area(name).await?.and_then(|a| a.owner);
            let new_owner = if *shared { None } else { owner.clone().or_else(|| current.clone()) };
            let changes_owner = new_owner != current;
            let mut parties = vec![current.clone()];
            if changes_owner {
                parties.push(new_owner.clone());
            }
            // Taking an area away from its owner is destructive; giving yours away is not.
            let risk = if changes_owner && current.as_deref().is_some_and(|c| c != proposer) {
                Risk::Destructive
            } else {
                Risk::Safe
            };
            One { parties, risk, owner: current, incumbent: None }
        }
    })
}

async fn validate_new_task(op: &mut Op, t: &NewTask) -> Result<()> {
    non_empty("title", &t.title, MAX_TITLE)?;
    check_len("description", &t.description, MAX_TEXT)?;
    check_len("acceptance", &t.acceptance, MAX_TEXT)?;
    if let Some(m) = t.milestone.as_deref().filter(|m| !m.is_empty()) {
        ensure_milestone(op, m).await?;
    }
    if let Some(a) = &t.assignee {
        op.active_member(a).await?;
    }
    ensure_deps(op, None, &t.depends_on).await?;
    Ok(())
}

async fn validate_patch(op: &mut Op, t: &Task, p: &TaskPatch) -> Result<()> {
    if p.is_empty() {
        return Err(HubError::invalid("patch changes nothing"));
    }
    if let Some(v) = &p.title {
        non_empty("title", v, MAX_TITLE)?;
    }
    if let Some(v) = &p.description {
        check_len("description", v, MAX_TEXT)?;
    }
    if let Some(v) = &p.acceptance {
        check_len("acceptance", v, MAX_TEXT)?;
    }
    if let Some(a) = p.area.as_deref().filter(|a| !a.is_empty()) {
        resolve_area(op, Some(a), None).await?;
    }
    if let Some(m) = p.milestone.as_deref().filter(|m| !m.is_empty()) {
        ensure_milestone(op, m).await?;
    }
    if let Some(d) = &p.depends_on {
        ensure_deps(op, Some(&t.id), d).await?;
    }
    Ok(())
}

/// Applies `changes` in order on behalf of `by`. Any error aborts the whole batch (the
/// caller rolls back). Returns one short summary per change.
pub async fn apply(op: &mut Op, by: &str, changes: &[Change], negotiation: Option<&str>) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(changes.len());
    for c in changes {
        out.push(apply_one(op, by, c, negotiation).await?);
    }
    Ok(out)
}

fn via(negotiation: Option<&str>) -> Value {
    json!({ "negotiation_id": negotiation })
}

async fn apply_one(op: &mut Op, by: &str, c: &Change, neg: Option<&str>) -> Result<String> {
    match c {
        Change::AcceptTask { task_id } => {
            let mut t = op.task(task_id).await?;
            if t.status() != TaskStatus::Proposed {
                return Err(HubError::invalid(format!("task {task_id} is no longer proposed (status {})", t.status)));
            }
            let status = if t.assignee.is_some() { TaskStatus::Claimed } else { TaskStatus::Open };
            t.status = status.as_str().into();
            t.lease_until = status.is_held().then(|| op.now + op.ns.lease());
            save_task(op, &mut t).await?;
            let s = format!("{} accepted into the plan ({})", task_ref(&t), t.status);
            op.emit("task.accepted", Some(&t.id), None, &s, via(neg)).await?;
            Ok(s)
        }
        Change::UpdateTask { task_id, patch } => {
            let mut t = open_task(op, task_id).await?;
            validate_patch(op, &t, patch).await?;
            let mut fields = vec![];
            if let Some(v) = &patch.title {
                t.title = v.trim().into();
                fields.push("title");
            }
            if let Some(v) = &patch.description {
                t.description = v.trim().into();
                fields.push("description");
            }
            if let Some(v) = &patch.acceptance {
                t.acceptance = v.trim().into();
                fields.push("acceptance");
            }
            if let Some(v) = &patch.area {
                t.area = Some(v.trim().to_string()).filter(|s| !s.is_empty());
                fields.push("area");
            }
            if let Some(v) = &patch.milestone {
                t.milestone = Some(v.trim().to_string()).filter(|s| !s.is_empty());
                fields.push("milestone");
            }
            if let Some(v) = patch.kind {
                t.kind = v.as_str().into();
                fields.push("kind");
            }
            if let Some(v) = patch.priority {
                t.priority = v.as_str().into();
                fields.push("priority");
            }
            if let Some(v) = &patch.depends_on {
                t.depends_on = Json(ensure_deps(op, Some(&t.id), v).await?);
                fields.push("depends_on");
            }
            save_task(op, &mut t).await?;
            let s = format!("{} updated: {}", task_ref(&t), fields.join(", "));
            op.emit("task.updated", Some(&t.id), None, &s, json!({ "fields": fields, "negotiation_id": neg })).await?;
            Ok(s)
        }
        Change::Assign { task_id, to } => {
            let mut t = open_task(op, task_id).await?;
            op.active_member(to).await?;
            let before = t.assignee.clone();
            t.assignee = Some(to.clone());
            if t.status() == TaskStatus::Open {
                t.status = TaskStatus::Claimed.as_str().into();
            }
            if t.status().is_held() {
                t.lease_until = Some(op.now + op.ns.lease());
            }
            save_task(op, &mut t).await?;
            let s = match before {
                Some(b) if b != *to => format!("{} handed from {b} to {to}", task_ref(&t)),
                _ => format!("{} assigned to {to}", task_ref(&t)),
            };
            op.emit("task.assigned", Some(&t.id), None, &s, json!({ "to": to, "negotiation_id": neg })).await?;
            Ok(s)
        }
        Change::Split { task_id, parts } => {
            let mut orig = open_task(op, task_id).await?;
            let mut ids = vec![];
            for p in parts {
                let status = if p.assignee.is_some() { TaskStatus::Claimed } else { TaskStatus::Open };
                let t = insert_task(op, p, by, status, p.assignee.as_deref(), Some(&orig)).await?;
                op.emit(
                    "task.created",
                    Some(&t.id),
                    None,
                    format!("{} created as part of {}", task_ref(&t), orig.id),
                    json!({ "split_from": orig.id, "negotiation_id": neg }),
                )
                .await?;
                ids.push(t.id);
            }
            // Whatever waited for the original now waits for all of its parts.
            sqlx::query(
                "UPDATE tasks SET depends_on = (depends_on - $2::text) || $3::jsonb, version = version + 1, updated_at = $4
                 WHERE namespace_id = $1 AND depends_on ? $2",
            )
            .bind(&op.ns.id)
            .bind(&orig.id)
            .bind(Json(&ids))
            .bind(op.now)
            .execute(&mut *op.tx)
            .await?;
            orig.status = TaskStatus::Cancelled.as_str().into();
            orig.blocked_reason = Some(format!("split into {}", ids.join(", ")));
            save_task(op, &mut orig).await?;
            let s = format!("{} split into {}", task_ref(&orig), ids.join(", "));
            op.emit("task.split", Some(&orig.id), None, &s, json!({ "parts": ids, "negotiation_id": neg })).await?;
            Ok(s)
        }
        Change::CancelTask { task_id, reason } => {
            let mut t = open_task(op, task_id).await?;
            t.status = TaskStatus::Cancelled.as_str().into();
            t.blocked_reason = Some(reason.trim().chars().take(MAX_NOTE).collect());
            save_task(op, &mut t).await?;
            let s = format!("{} cancelled", task_ref(&t));
            op.emit(
                "task.cancelled",
                Some(&t.id),
                None,
                &s,
                json!({ "reason": super::untrusted(by, reason), "negotiation_id": neg }),
            )
            .await?;
            Ok(s)
        }
        Change::SetGoal { goal } => {
            op.ns.goal = goal.trim().to_string();
            op.bump_plan();
            let s = "plan goal replaced".to_string();
            op.emit("plan.goal_set", Some("plan:goal"), None, &s, json!({ "goal": op.ns.goal, "negotiation_id": neg }))
                .await?;
            Ok(s)
        }
        Change::AddMilestone { title, description } => {
            let id = op.next_milestone_id();
            sqlx::query(
                "INSERT INTO milestones (namespace_id, id, title, description, position, status)
                 VALUES ($1, $2, $3, $4,
                         (SELECT COALESCE(MAX(position), 0) + 1 FROM milestones WHERE namespace_id = $1), 'open')",
            )
            .bind(&op.ns.id)
            .bind(&id)
            .bind(title.trim())
            .bind(description.trim())
            .execute(&mut *op.tx)
            .await?;
            op.bump_plan();
            let s = format!("milestone {id} \"{}\" added", title.trim());
            op.emit("milestone.added", Some(&id), None, &s, via(neg)).await?;
            Ok(s)
        }
        Change::UpdateMilestone { milestone_id, title, description, status } => {
            ensure_milestone(op, milestone_id).await?;
            sqlx::query(
                "UPDATE milestones SET title = COALESCE($3, title), description = COALESCE($4, description),
                                       status = COALESCE($5, status)
                 WHERE namespace_id = $1 AND id = $2",
            )
            .bind(&op.ns.id)
            .bind(milestone_id)
            .bind(title.as_deref().map(str::trim))
            .bind(description.as_deref().map(str::trim))
            .bind(status.map(|s| s.as_str()))
            .execute(&mut *op.tx)
            .await?;
            op.bump_plan();
            let s = format!("milestone {milestone_id} updated");
            op.emit("milestone.updated", Some(milestone_id), None, &s, via(neg)).await?;
            Ok(s)
        }
        Change::RemoveMilestone { milestone_id } => {
            ensure_milestone(op, milestone_id).await?;
            sqlx::query("UPDATE tasks SET milestone = NULL WHERE namespace_id = $1 AND milestone = $2")
                .bind(&op.ns.id)
                .bind(milestone_id)
                .execute(&mut *op.tx)
                .await?;
            sqlx::query("DELETE FROM milestones WHERE namespace_id = $1 AND id = $2")
                .bind(&op.ns.id)
                .bind(milestone_id)
                .execute(&mut *op.tx)
                .await?;
            op.bump_plan();
            let s = format!("milestone {milestone_id} removed");
            op.emit("milestone.removed", Some(milestone_id), None, &s, via(neg)).await?;
            Ok(s)
        }
        Change::SetArea { name, owner, shared, paths, description } => {
            let current = op.area(name).await?;
            let new_owner =
                if *shared { None } else { owner.clone().or_else(|| current.as_ref().and_then(|a| a.owner.clone())) };
            if let Some(o) = &new_owner {
                op.active_member(o).await?;
            }
            let paths: Vec<String> = paths
                .clone()
                .or_else(|| current.as_ref().map(|a| a.paths.0.clone()))
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            let description =
                description.clone().or_else(|| current.as_ref().map(|a| a.description.clone())).unwrap_or_default();
            sqlx::query(
                "INSERT INTO areas (namespace_id, name, owner, paths, description) VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (namespace_id, name)
                 DO UPDATE SET owner = EXCLUDED.owner, paths = EXCLUDED.paths, description = EXCLUDED.description",
            )
            .bind(&op.ns.id)
            .bind(name)
            .bind(&new_owner)
            .bind(Json(&paths))
            .bind(description.trim())
            .execute(&mut *op.tx)
            .await?;
            op.bump_plan();
            let owner_txt = new_owner.as_deref().unwrap_or("nobody (shared)");
            let s = if current.is_some() {
                format!("area {name} updated (owner: {owner_txt})")
            } else {
                format!("area {name} created (owner: {owner_txt})")
            };
            op.emit(
                "area.set",
                Some(&format!("area:{name}")),
                None,
                &s,
                json!({ "owner": new_owner, "paths": paths, "negotiation_id": neg }),
            )
            .await?;
            Ok(s)
        }
    }
}

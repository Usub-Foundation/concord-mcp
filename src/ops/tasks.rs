//! Task operations: create, claim, status changes, edits, picking the next task, leases.

use std::collections::HashMap;

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use time::OffsetDateTime;

use super::negotiate::propose_or_apply;
use super::{MAX_NOTE, MAX_TEXT, check_len, insert_task, resolve_area, save_task, task_ref, untrusted};
use crate::clock::rfc3339;
use crate::error::{HubError, Result};
use crate::hub::Op;
use crate::model::{Area, Change, Milestone, NewTask, Priority, Task, TaskKind, TaskPatch, TaskStatus};

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct CreateTask {
    /// Short imperative title, e.g. "Fix 500 on empty cart".
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// What must be true for the task to count as done.
    #[serde(default)]
    pub acceptance: String,
    /// Area name; if omitted it is inferred from `paths`. A task in someone else's area is
    /// filed as `proposed` and their owner has to accept it (silence = consent).
    #[serde(default)]
    pub area: Option<String>,
    /// Files or directories involved, e.g. ["server/src/cart.rs"].
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub milestone: Option<String>,
    /// feature (default), bug, chore or followup.
    #[serde(default)]
    pub kind: Option<TaskKind>,
    /// p0 (urgent) .. p3 (someday). Default p2.
    #[serde(default)]
    pub priority: Option<Priority>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Bugs: steps to reproduce.
    #[serde(default)]
    pub repro: String,
    /// Bugs: what you saw (error text, failing test, file:line).
    #[serde(default)]
    pub evidence: String,
    /// Take the task yourself right away.
    #[serde(default)]
    pub claim: bool,
    /// Suggest another member to do it; they have to consent.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Why the task is needed; shown to whoever has to accept it.
    #[serde(default)]
    pub rationale: String,
}

pub async fn create(op: &mut Op, caller: &str, input: CreateTask) -> Result<Value> {
    check_len("repro", &input.repro, MAX_TEXT)?;
    check_len("evidence", &input.evidence, MAX_TEXT)?;
    let mut description = input.description.trim().to_string();
    for (label, text) in [("Steps to reproduce", &input.repro), ("Evidence", &input.evidence)] {
        if !text.trim().is_empty() {
            if !description.is_empty() {
                description.push_str("\n\n");
            }
            description.push_str(&format!("{label}:\n{}", text.trim()));
        }
    }
    let area = resolve_area(op, input.area.as_deref(), input.paths.as_deref()).await?;
    let owner = op.area_owner(area.as_deref()).await?;
    let foreign = owner.as_deref().is_some_and(|o| o != caller);
    let other = input.assignee.clone().filter(|a| a != caller);
    if input.claim && other.is_some() {
        return Err(HubError::invalid("use either claim=true or assignee=<someone else>, not both"));
    }
    let mine = input.claim || input.assignee.as_deref() == Some(caller);
    let status = if foreign {
        TaskStatus::Proposed
    } else if mine {
        TaskStatus::Claimed
    } else {
        TaskStatus::Open
    };
    let new = NewTask {
        title: input.title,
        description,
        acceptance: input.acceptance,
        area: area.clone(),
        paths: None,
        milestone: input.milestone,
        kind: input.kind,
        priority: input.priority,
        depends_on: input.depends_on,
        assignee: None,
    };
    let task = insert_task(op, &new, caller, status, mine.then_some(caller), None).await?;
    let kind = if foreign { "task.proposed" } else { "task.created" };
    let summary = match (&owner, foreign) {
        (Some(o), true) => {
            format!("{caller} filed {} into {o}'s area {}", task_ref(&task), area.as_deref().unwrap_or(""))
        }
        _ => format!("{caller} created {}", task_ref(&task)),
    };
    op.emit(
        kind,
        Some(&task.id),
        None,
        summary,
        json!({ "kind": task.kind, "rationale": untrusted(caller, &input.rationale) }),
    )
    .await?;

    let mut changes = vec![];
    if foreign {
        changes.push(Change::AcceptTask { task_id: task.id.clone() });
    }
    if let Some(o) = other {
        changes.push(Change::Assign { task_id: task.id.clone(), to: o });
    }
    if changes.is_empty() {
        let hint = if mine {
            "It is yours: start it with set_task_status(action=start)."
        } else {
            "Open for anyone to claim."
        };
        return Ok(json!({ "task": task, "hint": hint }));
    }
    let proposal = propose_or_apply(op, caller, changes, &input.rationale).await?;
    let task = op.task(&task.id).await?;
    Ok(json!({ "task": task, "proposal": proposal }))
}

pub async fn claim(op: &mut Op, caller: &str, id: &str) -> Result<Value> {
    let mut t = op.task(id).await?;
    op.ensure_not_frozen(&[format!("task:{}", t.id)]).await?;
    match t.status() {
        TaskStatus::Open => {}
        s if s.is_held() && t.assignee.as_deref() == Some(caller) => {
            renew_one(op, &mut t).await?;
            return Ok(json!({ "task": t, "result": "already_yours", "hint": "Lease renewed." }));
        }
        s if s.is_held() => {
            let holder = t.assignee.clone().unwrap_or_default();
            return Err(HubError::conflict(
                format!("{} is already {} by {holder}", t.id, t.status),
                json!({
                    "holder": holder,
                    "status": t.status,
                    "lease_until": t.lease_until.map(rfc3339),
                    "hint": format!(
                        "Pick another task (next_task), or ask for it: propose(changes=[{{\"op\":\"assign\",\"task_id\":\"{}\",\"to\":\"{caller}\"}}])",
                        t.id
                    ),
                }),
            ));
        }
        TaskStatus::Proposed => {
            return Err(HubError::conflict(
                format!("{} is proposed and waits for its area owner's acceptance", t.id),
                json!({ "hint": "see list_negotiations / sync for the open proposal" }),
            ));
        }
        _ => return Err(HubError::invalid(format!("{} is {} and cannot be claimed", t.id, t.status))),
    }
    t.status = TaskStatus::Claimed.as_str().into();
    t.assignee = Some(caller.to_string());
    t.lease_until = Some(op.now + op.ns.lease());
    save_task(op, &mut t).await?;
    op.emit("task.claimed", Some(&t.id), None, format!("{caller} claimed {}", task_ref(&t)), json!({})).await?;
    crate::connectors::engine::on_task_change(op, &t, crate::connectors::engine::TaskEvent::Claimed, caller).await?;
    let blocking = unresolved_deps(&mut op.tx, &op.ns.id, &t).await?;
    Ok(json!({
        "task": t,
        "result": "claimed",
        "unfinished_dependencies": blocking,
        "hint": if blocking.is_empty() { "Start with set_task_status(action=start)." } else { "Dependencies are not finished yet; start will refuse unless force=true." },
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusAction {
    /// claimed/blocked -> in_progress (assignee).
    Start,
    /// Post a progress note and renew your lease (assignee). Needs `note`.
    Progress,
    /// in_progress -> review: ask someone else to check it (assignee).
    Submit,
    /// review -> done (anyone but the assignee, preferably the area owner).
    Approve,
    /// review -> in_progress with feedback in `note` (anyone but the assignee).
    RequestChanges,
    /// claimed/in_progress -> done without review (assignee).
    Done,
    /// -> blocked; give the reason in `note` (assignee).
    Block,
    /// blocked -> in_progress (assignee or area owner).
    Unblock,
    /// Give the task back to the pool (assignee).
    Release,
    /// done -> open (area owner, assignee or creator). For new defects prefer create_task kind=bug.
    Reopen,
}

pub async fn set_status(
    op: &mut Op,
    caller: &str,
    id: &str,
    action: StatusAction,
    note: &str,
    expected_version: Option<i64>,
    force: bool,
) -> Result<Value> {
    check_len("note", note, MAX_NOTE)?;
    let mut t = op.task(id).await?;
    if let Some(v) = expected_version
        && v != t.version
    {
        return Err(HubError::conflict(
            format!("{} changed: you have version {v}, current is {}", t.id, t.version),
            json!({ "task": t }),
        ));
    }
    if action != StatusAction::Progress {
        op.ensure_not_frozen(&[format!("task:{}", t.id)]).await?;
    }
    let status = t.status();
    let is_assignee = t.assignee.as_deref() == Some(caller);
    let owner = op.area_owner(t.area.as_deref()).await?;
    let need_assignee = || -> Result<()> {
        if is_assignee {
            Ok(())
        } else {
            Err(HubError::forbidden(format!(
                "only the assignee ({}) can do that; claim the task or ask for it with propose(assign)",
                t.assignee.as_deref().unwrap_or("nobody")
            )))
        }
    };
    let need_note = |what: &str| -> Result<()> {
        if note.trim().is_empty() { Err(HubError::invalid(format!("{what} needs a `note`"))) } else { Ok(()) }
    };
    let wrong = |allowed: &str| {
        HubError::invalid(format!("cannot {action:?} a task that is {status} (allowed from: {allowed})"))
    };
    let mut unblocked_check = false;
    let summary: String;

    match action {
        StatusAction::Start => {
            need_assignee()?;
            if !matches!(status, TaskStatus::Claimed | TaskStatus::Blocked | TaskStatus::InProgress) {
                return Err(wrong("claimed, blocked"));
            }
            let blocking = unresolved_deps(&mut op.tx, &op.ns.id, &t).await?;
            if !blocking.is_empty() && !force {
                return Err(HubError::conflict(
                    format!("{} waits for unfinished dependencies", t.id),
                    json!({ "unfinished_dependencies": blocking, "hint": "finish them first or pass force=true" }),
                ));
            }
            t.status = TaskStatus::InProgress.as_str().into();
            t.blocked_reason = None;
            summary = format!("{caller} started {}", task_ref(&t));
        }
        StatusAction::Progress => {
            need_assignee()?;
            need_note("progress")?;
            renew_one(op, &mut t).await?;
            op.emit(
                "task.progress",
                Some(&t.id),
                None,
                format!("{caller} posted progress on {}", task_ref(&t)),
                json!({ "note": untrusted(caller, note) }),
            )
            .await?;
            return Ok(json!({ "task": t, "hint": "Lease renewed." }));
        }
        StatusAction::Submit => {
            need_assignee()?;
            if !matches!(status, TaskStatus::InProgress | TaskStatus::Claimed) {
                return Err(wrong("in_progress, claimed"));
            }
            t.status = TaskStatus::Review.as_str().into();
            t.lease_until = None;
            summary = format!(
                "{caller} submitted {} for review{}",
                task_ref(&t),
                owner.as_deref().filter(|o| *o != caller).map(|o| format!(" (area owner: {o})")).unwrap_or_default()
            );
        }
        StatusAction::Approve | StatusAction::RequestChanges => {
            if status != TaskStatus::Review {
                return Err(wrong("review"));
            }
            if is_assignee {
                return Err(HubError::forbidden("someone other than the assignee has to review"));
            }
            if action == StatusAction::Approve {
                t.status = TaskStatus::Done.as_str().into();
                unblocked_check = true;
                summary = format!("{caller} approved {}", task_ref(&t));
            } else {
                need_note("request_changes")?;
                t.status = TaskStatus::InProgress.as_str().into();
                t.lease_until = Some(op.now + op.ns.lease());
                summary = format!("{caller} requested changes on {}", task_ref(&t));
            }
        }
        StatusAction::Done => {
            need_assignee()?;
            if !matches!(status, TaskStatus::Claimed | TaskStatus::InProgress) {
                return Err(wrong("claimed, in_progress (a task in review needs approve from someone else)"));
            }
            t.status = TaskStatus::Done.as_str().into();
            unblocked_check = true;
            summary = format!("{caller} finished {}", task_ref(&t));
        }
        StatusAction::Block => {
            need_assignee()?;
            need_note("block")?;
            if !matches!(status, TaskStatus::Claimed | TaskStatus::InProgress) {
                return Err(wrong("claimed, in_progress"));
            }
            t.status = TaskStatus::Blocked.as_str().into();
            t.blocked_reason = Some(note.trim().to_string());
            summary = format!("{caller} is blocked on {}", task_ref(&t));
        }
        StatusAction::Unblock => {
            if !(is_assignee || owner.as_deref() == Some(caller)) {
                return Err(HubError::forbidden("only the assignee or the area owner can unblock"));
            }
            if status != TaskStatus::Blocked {
                return Err(wrong("blocked"));
            }
            t.status = TaskStatus::InProgress.as_str().into();
            t.blocked_reason = None;
            summary = format!("{caller} unblocked {}", task_ref(&t));
        }
        StatusAction::Release => {
            need_assignee()?;
            if !status.is_held() {
                return Err(wrong("claimed, in_progress, blocked, review"));
            }
            t.status = TaskStatus::Open.as_str().into();
            t.assignee = None;
            summary = format!("{caller} released {}", task_ref(&t));
        }
        StatusAction::Reopen => {
            if status != TaskStatus::Done {
                return Err(wrong("done"));
            }
            let allowed = is_assignee || owner.as_deref() == Some(caller) || t.created_by == caller;
            if !allowed {
                return Err(HubError::forbidden(
                    "only the area owner, the assignee or the creator can reopen; file a bug with create_task(kind=bug) instead",
                ));
            }
            t.status = TaskStatus::Open.as_str().into();
            t.assignee = None;
            summary = format!("{caller} reopened {}", task_ref(&t));
        }
    }
    if t.status().is_held() && t.status() != TaskStatus::Review {
        t.lease_until = Some(op.now + op.ns.lease());
    }
    save_task(op, &mut t).await?;
    op.emit(
        &format!("task.{}", serde_json::to_value(action)?.as_str().unwrap_or("status")),
        Some(&t.id),
        None,
        &summary,
        json!({ "status": t.status, "note": untrusted(caller, note) }),
    )
    .await?;
    let mut unblocked = vec![];
    if unblocked_check {
        unblocked = announce_unblocked(op, &t.id).await?;
        let by = t.assignee.clone().unwrap_or_else(|| caller.to_string());
        crate::connectors::engine::on_task_change(op, &t, crate::connectors::engine::TaskEvent::Done, &by).await?;
    }
    Ok(json!({ "task": t, "now_unblocked": unblocked }))
}

/// Tasks that depended on `done_id` and have no unfinished dependency left.
async fn announce_unblocked(op: &mut Op, done_id: &str) -> Result<Vec<String>> {
    let dependents: Vec<Task> = sqlx::query_as(
        "SELECT * FROM tasks WHERE namespace_id = $1 AND depends_on ? $2
           AND status NOT IN ('done', 'cancelled', 'rejected')",
    )
    .bind(&op.ns.id)
    .bind(done_id)
    .fetch_all(&mut *op.tx)
    .await?;
    let mut out = vec![];
    for d in dependents {
        if unresolved_deps(&mut op.tx, &op.ns.id, &d).await?.is_empty() {
            op.emit(
                "task.unblocked",
                Some(&d.id),
                None,
                format!("{} has no unfinished dependencies any more", task_ref(&d)),
                json!({ "assignee": d.assignee }),
            )
            .await?;
            out.push(d.id);
        }
    }
    Ok(out)
}

pub async fn update(
    op: &mut Op,
    caller: &str,
    id: &str,
    expected_version: i64,
    patch: TaskPatch,
    rationale: &str,
) -> Result<Value> {
    let t = op.task(id).await?;
    if t.version != expected_version {
        return Err(HubError::conflict(
            format!(
                "{} changed: you have version {expected_version}, current is {}; re-read and retry",
                t.id, t.version
            ),
            json!({ "task": t }),
        ));
    }
    propose_or_apply(op, caller, vec![Change::UpdateTask { task_id: t.id.clone(), patch }], rationale).await
}

pub async fn unresolved_deps(conn: &mut PgConnection, ns: &str, t: &Task) -> Result<Vec<String>> {
    if t.depends_on.is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, status FROM tasks WHERE namespace_id = $1 AND id = ANY($2)")
            .bind(ns)
            .bind(&t.depends_on.0)
            .fetch_all(conn)
            .await?;
    Ok(rows
        .into_iter()
        .filter(|(_, s)| !TaskStatus::parse(s).is_some_and(|s| s.resolves_dependency()))
        .map(|(id, _)| id)
        .collect())
}

async fn renew_one(op: &mut Op, t: &mut Task) -> Result<()> {
    let until = op.now + op.ns.lease();
    sqlx::query("UPDATE tasks SET lease_until = $3 WHERE namespace_id = $1 AND id = $2")
        .bind(&op.ns.id)
        .bind(&t.id)
        .bind(until)
        .execute(&mut *op.tx)
        .await?;
    t.lease_until = Some(until);
    Ok(())
}

/// Heartbeat: any `sync` / `wait_for_updates` keeps the caller's leases alive. Runs without
/// the namespace lock; the sweeper re-checks the lease under row locks before expiring.
pub async fn renew_leases(
    conn: &mut PgConnection,
    ns: &str,
    member: &str,
    now: OffsetDateTime,
    lease: time::Duration,
) -> Result<u64> {
    let r = sqlx::query(
        "UPDATE tasks SET lease_until = $3
         WHERE namespace_id = $1 AND assignee = $2 AND status IN ('claimed', 'in_progress', 'blocked')
           AND lease_until IS NOT NULL AND lease_until < $4",
    )
    .bind(ns)
    .bind(member)
    .bind(now + lease)
    .bind(now + lease / 2)
    .execute(conn)
    .await?;
    Ok(r.rows_affected())
}

/// Returns tasks whose holder went silent to the pool.
pub async fn expire_leases(op: &mut Op) -> Result<usize> {
    let expired: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, title, assignee FROM tasks
         WHERE namespace_id = $1 AND lease_until < $2 AND status IN ('claimed', 'in_progress', 'blocked')
         FOR UPDATE",
    )
    .bind(&op.ns.id)
    .bind(op.now)
    .fetch_all(&mut *op.tx)
    .await?;
    for (id, title, assignee) in &expired {
        sqlx::query(
            "UPDATE tasks SET status = 'open', assignee = NULL, lease_until = NULL, version = version + 1, updated_at = $3
             WHERE namespace_id = $1 AND id = $2",
        )
        .bind(&op.ns.id)
        .bind(id)
        .bind(op.now)
        .execute(&mut *op.tx)
        .await?;
        let who = assignee.as_deref().unwrap_or("?");
        op.emit(
            "task.lease_expired",
            Some(id),
            None,
            format!("{id} \"{title}\" is open again: {who} did not report for the whole lease"),
            json!({ "previous_assignee": assignee }),
        )
        .await?;
    }
    Ok(expired.len())
}

/// Ranked open tasks for `member`: own area first, then shared, then others' areas; then
/// priority, milestone order and age. Tasks with unfinished dependencies are skipped.
pub async fn candidates(conn: &mut PgConnection, ns: &str, member: &str) -> Result<Vec<(Task, &'static str)>> {
    let all: Vec<Task> =
        sqlx::query_as("SELECT * FROM tasks WHERE namespace_id = $1").bind(ns).fetch_all(&mut *conn).await?;
    let areas: Vec<Area> =
        sqlx::query_as("SELECT * FROM areas WHERE namespace_id = $1").bind(ns).fetch_all(&mut *conn).await?;
    let milestones: Vec<Milestone> =
        sqlx::query_as("SELECT * FROM milestones WHERE namespace_id = $1").bind(ns).fetch_all(&mut *conn).await?;
    let status: HashMap<&str, TaskStatus> = all.iter().map(|t| (t.id.as_str(), t.status())).collect();
    let owner: HashMap<&str, Option<&str>> = areas.iter().map(|a| (a.name.as_str(), a.owner.as_deref())).collect();
    let mpos: HashMap<&str, i64> = milestones.iter().map(|m| (m.id.as_str(), m.position)).collect();
    let mut out: Vec<(i32, i32, i64, i64, &Task, &'static str)> = vec![];
    for t in &all {
        if t.status() != TaskStatus::Open {
            continue;
        }
        let ready = t.depends_on.iter().all(|d| status.get(d.as_str()).is_none_or(|s| s.resolves_dependency()));
        if !ready {
            continue;
        }
        let (tier, why) = match t.area.as_deref().and_then(|a| owner.get(a).copied().flatten()) {
            Some(o) if o == member => (0, "your area"),
            None => (1, "shared"),
            Some(_) => (2, "someone else's area (allowed, but its owner may ask for it back)"),
        };
        let prio = Priority::parse(&t.priority).map(|p| p as i32).unwrap_or(2);
        let pos = t.milestone.as_deref().and_then(|m| mpos.get(m).copied()).unwrap_or(i64::MAX);
        out.push((tier, prio, pos, t.number(), t, why));
    }
    out.sort_by_key(|a| (a.0, a.1, a.2, a.3));
    Ok(out.into_iter().map(|(.., t, why)| (t.clone(), why)).collect())
}

pub async fn active_of(conn: &mut PgConnection, ns: &str, member: &str) -> Result<Vec<Task>> {
    Ok(sqlx::query_as(
        "SELECT * FROM tasks WHERE namespace_id = $1 AND assignee = $2
           AND status IN ('claimed', 'in_progress', 'blocked', 'review')
         ORDER BY created_at",
    )
    .bind(ns)
    .bind(member)
    .fetch_all(conn)
    .await?)
}

/// Compact task line for lists (full text via get_task).
pub fn brief(t: &Task) -> Value {
    let mut desc: String = t.description.chars().take(240).collect();
    if t.description.chars().count() > 240 {
        desc.push('…');
    }
    json!({
        "id": t.id,
        "title": t.title,
        "status": t.status,
        "assignee": t.assignee,
        "area": t.area,
        "milestone": t.milestone,
        "kind": t.kind,
        "priority": t.priority,
        "depends_on": t.depends_on.0,
        "version": t.version,
        "lease_until": t.lease_until.map(rfc3339),
        "description": desc,
    })
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct TaskFilter {
    /// Only these statuses. Default: everything not closed.
    #[serde(default)]
    pub status: Option<Vec<TaskStatus>>,
    #[serde(default)]
    pub area: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub milestone: Option<String>,
    #[serde(default)]
    pub kind: Option<TaskKind>,
    /// Only tasks assigned to you.
    #[serde(default)]
    pub mine: bool,
    /// Include done, cancelled and rejected tasks.
    #[serde(default)]
    pub include_closed: bool,
    /// Max results (default 100, max 500).
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

pub async fn list(conn: &mut PgConnection, ns: &str, caller: &str, f: &TaskFilter) -> Result<Value> {
    let statuses: Option<Vec<String>> = match (&f.status, f.include_closed) {
        (Some(s), _) => Some(s.iter().map(|s| s.as_str().to_string()).collect()),
        (None, true) => None,
        (None, false) => {
            Some(TaskStatus::ALL.iter().filter(|s| !s.is_closed()).map(|s| s.as_str().to_string()).collect())
        }
    };
    let assignee = if f.mine { Some(caller.to_string()) } else { f.assignee.clone() };
    let limit = f.limit.unwrap_or(100).clamp(1, 500);
    let offset = f.offset.unwrap_or(0).max(0);
    let rows: Vec<Task> = sqlx::query_as(
        "SELECT * FROM tasks
         WHERE namespace_id = $1
           AND ($2::text[] IS NULL OR status = ANY($2))
           AND ($3::text IS NULL OR area = $3)
           AND ($4::text IS NULL OR assignee = $4)
           AND ($5::text IS NULL OR milestone = $5)
           AND ($6::text IS NULL OR kind = $6)
         ORDER BY created_at, id
         LIMIT $7 OFFSET $8",
    )
    .bind(ns)
    .bind(statuses)
    .bind(&f.area)
    .bind(assignee)
    .bind(&f.milestone)
    .bind(f.kind.map(|k| k.as_str()))
    .bind(limit + 1)
    .bind(offset)
    .fetch_all(conn)
    .await?;
    let more = rows.len() as i64 > limit;
    let tasks: Vec<Value> = rows.iter().take(limit as usize).map(brief).collect();
    Ok(json!({ "tasks": tasks, "more": more, "next_offset": more.then_some(offset + limit) }))
}

pub async fn get(conn: &mut PgConnection, ns: &str, caller: &str, id: &str) -> Result<Value> {
    let t: Option<Task> = sqlx::query_as("SELECT * FROM tasks WHERE namespace_id = $1 AND id = $2")
        .bind(ns)
        .bind(id.trim())
        .fetch_optional(&mut *conn)
        .await?;
    let t = t.ok_or_else(|| HubError::not_found(format!("task {id} not found")))?;
    let negotiations: Vec<crate::model::Negotiation> = sqlx::query_as(
        "SELECT * FROM negotiations WHERE namespace_id = $1 AND targets ? $2 ORDER BY created_at DESC LIMIT 10",
    )
    .bind(ns)
    .bind(format!("task:{}", t.id))
    .fetch_all(&mut *conn)
    .await?;
    let mut negs = vec![];
    for n in &negotiations {
        negs.push(super::negotiate::view_with(&mut *conn, ns, n, Some(caller), false).await?);
    }
    let history: Vec<crate::model::Event> = sqlx::query_as(
        "SELECT seq, at, actor, kind, entity, summary, data FROM events
         WHERE namespace_id = $1 AND entity = $2 AND (audience IS NULL OR audience ? $3)
         ORDER BY seq DESC LIMIT 30",
    )
    .bind(ns)
    .bind(&t.id)
    .bind(caller)
    .fetch_all(&mut *conn)
    .await?;
    let blocking = unresolved_deps(&mut *conn, ns, &t).await?;
    Ok(json!({
        "task": t,
        "unfinished_dependencies": blocking,
        "negotiations": negs,
        "history": history.into_iter().rev().collect::<Vec<_>>(),
    }))
}

/// Creates several tasks at once (a backlog pasted by a human, or a list an agent was given).
/// Leads and admins plan the work, so their tasks open directly in any area; everyone else
/// follows the same rules as `create` (someone else's area becomes a proposal).
pub async fn create_many(
    op: &mut Op,
    caller: &str,
    role: crate::model::Role,
    tasks: Vec<NewTask>,
    rationale: &str,
) -> Result<Value> {
    if !role.can_write() {
        return Err(HubError::forbidden("viewers cannot create tasks"));
    }
    if tasks.len() > super::import::MAX_IMPORT {
        return Err(HubError::invalid(format!("too many tasks at once (max {})", super::import::MAX_IMPORT)));
    }
    let mut created = vec![];
    let mut proposals = vec![];
    for t in tasks {
        if role.approves_plan() {
            if let Some(a) = &t.assignee {
                op.active_member(a).await?;
            }
            let status = if t.assignee.is_some() { TaskStatus::Claimed } else { TaskStatus::Open };
            let task = insert_task(op, &t, caller, status, t.assignee.as_deref(), None).await?;
            op.emit(
                "task.created",
                Some(&task.id),
                None,
                format!("{caller} created {}", task_ref(&task)),
                json!({ "kind": task.kind, "by_human_planner": true }),
            )
            .await?;
            created.push(brief(&task));
        } else {
            let v = create(
                op,
                caller,
                CreateTask {
                    title: t.title,
                    description: t.description,
                    acceptance: t.acceptance,
                    area: t.area,
                    paths: t.paths,
                    milestone: t.milestone,
                    kind: t.kind,
                    priority: t.priority,
                    depends_on: t.depends_on,
                    assignee: t.assignee,
                    rationale: rationale.to_string(),
                    ..CreateTask::default()
                },
            )
            .await?;
            if !v["proposal"].is_null() {
                proposals.push(v["proposal"]["negotiation"]["id"].clone());
            }
            created.push(v["task"].clone());
        }
    }
    Ok(json!({ "count": created.len(), "created": created, "proposals": proposals }))
}

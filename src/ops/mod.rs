//! Hub operations. Write operations take `&mut Op` (one locked namespace transaction);
//! read operations take a plain connection or the pool.

pub mod admin;
pub mod changes;
pub mod import;
pub mod negotiate;
pub mod plan;
pub mod sync;
pub mod tasks;

use std::collections::{HashMap, HashSet};

use sqlx::types::Json;

use crate::error::{HubError, Result};
use crate::glob;
use crate::hub::Op;
use crate::model::{NewTask, Priority, Task, TaskKind, TaskStatus};

pub const MAX_TITLE: usize = 200;
pub const MAX_TEXT: usize = 20_000;
pub const MAX_NOTE: usize = 4_000;
pub const MAX_CHANGES: usize = 50;

pub fn check_len(field: &str, value: &str, max: usize) -> Result<()> {
    if value.chars().count() > max {
        return Err(HubError::invalid(format!("{field} is too long (max {max} characters)")));
    }
    Ok(())
}

pub fn non_empty(field: &str, value: &str, max: usize) -> Result<String> {
    let v = value.trim();
    if v.is_empty() {
        return Err(HubError::invalid(format!("{field} must not be empty")));
    }
    check_len(field, v, max)?;
    Ok(v.to_string())
}

/// Ids for namespaces and members: lowercase slug, 1–32 chars.
pub fn valid_slug(kind: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 32
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(HubError::invalid(format!(
            "{kind} id {s:?} must be 1-32 chars of a-z, 0-9, '-' or '_' and start with a letter or digit"
        )))
    }
}

/// Area of a new task: the explicit one (must exist), else the best match for `paths`, else none.
pub async fn resolve_area(op: &mut Op, area: Option<&str>, paths: Option<&[String]>) -> Result<Option<String>> {
    if let Some(a) = area.map(str::trim).filter(|a| !a.is_empty()) {
        return match op.area(a).await? {
            Some(found) => Ok(Some(found.name)),
            None => {
                let known: Vec<String> = op.areas().await?.into_iter().map(|a| a.name).collect();
                Err(HubError::invalid(format!("unknown area {a:?}; known areas: {known:?}")))
            }
        };
    }
    let Some(paths) = paths.filter(|p| !p.is_empty()) else { return Ok(None) };
    let mut best: Option<(usize, String)> = None;
    for a in op.areas().await? {
        for pattern in a.paths.iter() {
            if paths.iter().any(|p| glob::matches(pattern, p)) {
                let score = glob::specificity(pattern);
                if best.as_ref().is_none_or(|(s, _)| score > *s) {
                    best = Some((score, a.name.clone()));
                }
            }
        }
    }
    Ok(best.map(|(_, name)| name))
}

pub async fn ensure_milestone(op: &mut Op, milestone: &str) -> Result<()> {
    let found: Option<(String,)> = sqlx::query_as("SELECT id FROM milestones WHERE namespace_id = $1 AND id = $2")
        .bind(&op.ns.id)
        .bind(milestone)
        .fetch_optional(&mut *op.tx)
        .await?;
    match found {
        Some(_) => Ok(()),
        None => Err(HubError::invalid(format!("unknown milestone {milestone:?}"))),
    }
}

/// All listed tasks exist and adding `task -> deps` keeps the dependency graph acyclic.
pub async fn ensure_deps(op: &mut Op, task: Option<&str>, deps: &[String]) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let deps: Vec<String> = deps.iter().map(|d| d.trim().to_string()).filter(|d| seen.insert(d.clone())).collect();
    if deps.is_empty() {
        return Ok(deps);
    }
    let rows: Vec<(String, Json<Vec<String>>)> =
        sqlx::query_as("SELECT id, depends_on FROM tasks WHERE namespace_id = $1")
            .bind(&op.ns.id)
            .fetch_all(&mut *op.tx)
            .await?;
    let mut graph: HashMap<String, Vec<String>> = rows.into_iter().map(|(id, d)| (id, d.0)).collect();
    for d in &deps {
        if !graph.contains_key(d) {
            return Err(HubError::invalid(format!("depends_on: task {d} does not exist")));
        }
    }
    let Some(task) = task else { return Ok(deps) };
    if deps.iter().any(|d| d == task) {
        return Err(HubError::invalid("a task cannot depend on itself"));
    }
    graph.insert(task.to_string(), deps.clone());
    let mut stack: Vec<String> = deps.clone();
    let mut visited = HashSet::new();
    while let Some(n) = stack.pop() {
        if n == task {
            return Err(HubError::invalid(format!("depends_on would create a cycle through {task}")));
        }
        if visited.insert(n.clone())
            && let Some(next) = graph.get(&n)
        {
            stack.extend(next.iter().cloned());
        }
    }
    Ok(deps)
}

/// Inserts a new task. The caller decides status and assignee (and checked permissions).
pub async fn insert_task(
    op: &mut Op,
    new: &NewTask,
    created_by: &str,
    status: TaskStatus,
    assignee: Option<&str>,
    defaults: Option<&Task>,
) -> Result<Task> {
    let title = non_empty("title", &new.title, MAX_TITLE)?;
    check_len("description", &new.description, MAX_TEXT)?;
    check_len("acceptance", &new.acceptance, MAX_TEXT)?;
    let area = match (new.area.as_deref(), new.paths.as_deref()) {
        (None, None) => defaults.and_then(|d| d.area.clone()),
        (a, p) => resolve_area(op, a, p).await?,
    };
    let milestone = match new.milestone.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => {
            ensure_milestone(op, m).await?;
            Some(m.to_string())
        }
        None => defaults.and_then(|d| d.milestone.clone()),
    };
    let kind = new
        .kind
        .map(|k| k.as_str().to_string())
        .or_else(|| defaults.map(|d| d.kind.clone()))
        .unwrap_or_else(|| TaskKind::Feature.as_str().into());
    let priority = new
        .priority
        .map(|p| p.as_str().to_string())
        .or_else(|| defaults.map(|d| d.priority.clone()))
        .unwrap_or_else(|| Priority::P2.as_str().into());
    let deps = ensure_deps(op, None, &new.depends_on).await?;
    if let Some(a) = assignee {
        op.active_member(a).await?;
    }
    let id = op.next_task_id();
    let lease = status.is_held().then(|| op.now + op.ns.lease());
    let task: Task = sqlx::query_as(
        "INSERT INTO tasks (namespace_id, id, title, description, acceptance, area, milestone, kind, priority,
                            status, assignee, depends_on, created_by, version, lease_until, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 1, $14, $15, $15)
         RETURNING *",
    )
    .bind(&op.ns.id)
    .bind(&id)
    .bind(title)
    .bind(new.description.trim())
    .bind(new.acceptance.trim())
    .bind(area)
    .bind(milestone)
    .bind(kind)
    .bind(priority)
    .bind(status.as_str())
    .bind(assignee)
    .bind(Json(deps))
    .bind(created_by)
    .bind(lease)
    .bind(op.now)
    .fetch_one(&mut *op.tx)
    .await?;
    Ok(task)
}

/// Writes every mutable column of `t`, bumping its version.
pub async fn save_task(op: &mut Op, t: &mut Task) -> Result<()> {
    t.version += 1;
    t.updated_at = op.now;
    if !t.status().is_held() {
        t.lease_until = None;
    }
    sqlx::query(
        "UPDATE tasks SET title = $3, description = $4, acceptance = $5, area = $6, milestone = $7, kind = $8,
                          priority = $9, status = $10, assignee = $11, depends_on = $12, version = $13,
                          lease_until = $14, blocked_reason = $15, updated_at = $16
         WHERE namespace_id = $1 AND id = $2",
    )
    .bind(&op.ns.id)
    .bind(&t.id)
    .bind(&t.title)
    .bind(&t.description)
    .bind(&t.acceptance)
    .bind(&t.area)
    .bind(&t.milestone)
    .bind(&t.kind)
    .bind(&t.priority)
    .bind(&t.status)
    .bind(&t.assignee)
    .bind(Json(t.depends_on.0.clone()))
    .bind(t.version)
    .bind(t.lease_until)
    .bind(&t.blocked_reason)
    .bind(t.updated_at)
    .execute(&mut *op.tx)
    .await?;
    Ok(())
}

/// Short human-readable reference used in event summaries.
pub fn task_ref(t: &Task) -> String {
    let mut title: String = t.title.chars().take(80).collect();
    if t.title.chars().count() > 80 {
        title.push('…');
    }
    format!("{} \"{}\"", t.id, title)
}

/// Text written by an agent, wrapped so readers can tell it apart from hub output.
pub fn untrusted(from: &str, text: &str) -> serde_json::Value {
    serde_json::json!({ "from": from, "untrusted_text": text })
}

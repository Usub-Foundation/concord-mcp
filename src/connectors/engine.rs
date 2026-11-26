//! Connector lifecycle: configuration, sync (tracker → tasks), attach, and the outbox that
//! writes progress back (tasks → tracker).

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::types::Json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use super::{Config, ExtIssue, Kind, Tracker, TrackerError, crypto};
use crate::clock::rfc3339;
use crate::error::{HubError, Result};
use crate::hub::{Hub, Op};
use crate::model::{NewTask, Task, TaskKind, TaskStatus};
use crate::ops::{MAX_TEXT, MAX_TITLE, insert_task, non_empty, save_task, task_ref};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConnectorRow {
    pub id: String,
    pub namespace_id: String,
    pub kind: String,
    pub name: String,
    pub config: Json<Value>,
    pub secret_enc: Option<Vec<u8>>,
    pub secret_hint: String,
    pub enabled: bool,
    pub sync_every_secs: i64,
    pub cursor: Option<OffsetDateTime>,
    pub last_sync_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
    pub last_result: Option<Json<Value>>,
    pub created_by: String,
    pub created_at: OffsetDateTime,
}

impl ConnectorRow {
    pub fn kind(&self) -> Result<Kind> {
        Kind::parse(&self.kind).ok_or_else(|| HubError::Internal(format!("unknown connector kind {}", self.kind)))
    }
    pub fn config(&self) -> Result<Config> {
        serde_json::from_value(self.config.0.clone()).map_err(|e| HubError::Internal(format!("connector config: {e}")))
    }
    pub fn actor(&self) -> String {
        format!("connector:{}", self.id)
    }
    pub fn view(&self) -> Value {
        json!({
            "id": self.id,
            "namespace": self.namespace_id,
            "kind": self.kind,
            "name": self.name,
            "config": self.config.0,
            "credential": if self.secret_enc.is_some() { json!(self.secret_hint) } else { Value::Null },
            "enabled": self.enabled,
            "sync_every_secs": self.sync_every_secs,
            "cursor": self.cursor.map(rfc3339),
            "last_sync_at": self.last_sync_at.map(rfc3339),
            "last_error": self.last_error,
            "last_result": self.last_result.as_ref().map(|r| r.0.clone()),
            "created_by": self.created_by,
            "created_at": rfc3339(self.created_at),
        })
    }
}

fn validate(kind: Kind, cfg: &Config, hub: &Hub) -> Result<()> {
    // Building the client checks the per-tracker settings (project format, Jira e-mail, ...).
    super::build(kind, cfg, "placeholder", hub.http.clone()).map(|_| ())
}

pub async fn load(pool: &sqlx::PgPool, ns: &str, id: &str) -> Result<ConnectorRow> {
    let row: Option<ConnectorRow> = sqlx::query_as("SELECT * FROM connectors WHERE namespace_id = $1 AND id = $2")
        .bind(ns)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.ok_or_else(|| HubError::not_found(format!("connector {id} not found")))
}

pub async fn list(pool: &sqlx::PgPool, ns: &str) -> Result<Vec<Value>> {
    let rows: Vec<ConnectorRow> =
        sqlx::query_as("SELECT * FROM connectors WHERE namespace_id = $1 ORDER BY created_at")
            .bind(ns)
            .fetch_all(pool)
            .await?;
    let counts: Vec<(String, i64)> =
        sqlx::query_as("SELECT connector_id, COUNT(*) FROM task_links WHERE namespace_id = $1 GROUP BY connector_id")
            .bind(ns)
            .fetch_all(pool)
            .await?;
    let failed: Vec<(String, i64)> = sqlx::query_as(
        "SELECT connector_id, COUNT(*) FROM connector_outbox WHERE namespace_id = $1 AND done_at IS NULL AND attempts > 0 GROUP BY connector_id",
    )
    .bind(ns)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let mut v = r.view();
            v["linked_tasks"] = json!(counts.iter().find(|c| c.0 == r.id).map(|c| c.1).unwrap_or(0));
            v["pending_writes"] = json!(failed.iter().find(|c| c.0 == r.id).map(|c| c.1).unwrap_or(0));
            v
        })
        .collect())
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewConnector {
    pub kind: Kind,
    #[serde(default)]
    pub name: String,
    pub config: Config,
    /// The tracker credential (token / API key). Stored encrypted, never returned.
    pub secret: String,
    #[serde(default)]
    pub sync_every_secs: Option<i64>,
}

pub async fn create(hub: &Hub, ns: &str, by: &str, n: NewConnector) -> Result<Value> {
    validate(n.kind, &n.config, hub)?;
    let secret = non_empty("credential", &n.secret, 4000)?;
    let id = format!("con_{}", &crate::token::generate()[4..14]);
    let name = if n.name.trim().is_empty() {
        format!("{} {}", n.kind.label(), n.config.project)
    } else {
        n.name.trim().to_string()
    };
    let every = n.sync_every_secs.unwrap_or(120).clamp(30, 86400);
    sqlx::query(
        "INSERT INTO connectors (id, namespace_id, kind, name, config, secret_enc, secret_hint, sync_every_secs, created_by, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(&id)
    .bind(ns)
    .bind(n.kind.as_str())
    .bind(&name)
    .bind(Json(serde_json::to_value(&n.config)?))
    .bind(hub.secrets.seal(&secret))
    .bind(crypto::hint(&secret))
    .bind(every)
    .bind(by)
    .bind(hub.now())
    .execute(&hub.pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => HubError::not_found(format!("namespace {ns} not found")),
        _ => HubError::Db(e),
    })?;
    hub.write(ns, by, |op| {
        let name = name.clone();
        Box::pin(async move {
            op.emit("connector.created", None, None, format!("tracker connector \"{name}\" added"), json!({})).await?;
            Ok(())
        })
    })
    .await?;
    Ok(load(&hub.pool, ns, &id).await?.view())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PatchConnector {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub config: Option<Config>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub sync_every_secs: Option<i64>,
}

pub async fn update(hub: &Hub, ns: &str, id: &str, p: PatchConnector) -> Result<Value> {
    let row = load(&hub.pool, ns, id).await?;
    if let Some(cfg) = &p.config {
        validate(row.kind()?, cfg, hub)?;
    }
    let secret = p.secret.as_deref().map(str::trim).filter(|s| !s.is_empty());
    sqlx::query(
        "UPDATE connectors SET name = COALESCE($3, name), config = COALESCE($4, config),
                secret_enc = COALESCE($5, secret_enc), secret_hint = COALESCE($6, secret_hint),
                enabled = COALESCE($7, enabled), sync_every_secs = COALESCE($8, sync_every_secs), last_error = NULL
         WHERE namespace_id = $1 AND id = $2",
    )
    .bind(ns)
    .bind(id)
    .bind(p.name.as_deref().map(str::trim).filter(|n| !n.is_empty()))
    .bind(p.config.as_ref().map(serde_json::to_value).transpose()?.map(Json))
    .bind(secret.map(|s| hub.secrets.seal(s)))
    .bind(secret.map(crypto::hint))
    .bind(p.enabled)
    .bind(p.sync_every_secs.map(|s| s.clamp(30, 86400)))
    .execute(&hub.pool)
    .await?;
    Ok(load(&hub.pool, ns, id).await?.view())
}

pub async fn delete(hub: &Hub, ns: &str, id: &str) -> Result<()> {
    let row = load(&hub.pool, ns, id).await?;
    sqlx::query("DELETE FROM connectors WHERE namespace_id = $1 AND id = $2")
        .bind(ns)
        .bind(id)
        .execute(&hub.pool)
        .await?;
    hub.write(ns, "operator", move |op| {
        Box::pin(async move {
            op.emit(
                "connector.removed",
                None,
                None,
                format!("tracker connector \"{}\" removed; its tasks stay", row.name),
                json!({}),
            )
            .await?;
            Ok(())
        })
    })
    .await
}

fn tracker(hub: &Hub, row: &ConnectorRow) -> Result<Box<dyn Tracker>> {
    let enc = row.secret_enc.as_deref().ok_or_else(|| HubError::invalid("the connector has no credential"))?;
    let secret = hub.secrets.open(enc)?;
    super::build(row.kind()?, &row.config()?, &secret, hub.http.clone())
}

pub async fn test(hub: &Hub, ns: &str, id: &str) -> Result<Value> {
    let row = load(&hub.pool, ns, id).await?;
    let t = tracker(hub, &row)?;
    match t.check().await {
        Ok(detail) => Ok(json!({ "ok": true, "detail": detail })),
        Err(e) => Ok(json!({ "ok": false, "detail": e.to_string() })),
    }
}

fn compose_description(kind: Kind, issue: &ExtIssue) -> String {
    let body = issue.body.trim();
    let footer = format!("{} {}: {}", kind.label(), issue.key, issue.url);
    let mut d = if body.is_empty() { footer } else { format!("{body}\n\n{footer}") };
    if d.chars().count() > MAX_TEXT {
        d = d.chars().take(MAX_TEXT - 1).collect::<String>() + "…";
    }
    d
}

fn clip_title(t: &str) -> String {
    let t = t.trim();
    let t = if t.is_empty() { "(untitled issue)" } else { t };
    if t.chars().count() > MAX_TITLE {
        t.chars().take(MAX_TITLE - 1).collect::<String>() + "…"
    } else {
        t.to_string()
    }
}

/// Creates a task for an issue and links them.
async fn import_issue(op: &mut Op, row: &ConnectorRow, cfg: &Config, issue: &ExtIssue, by: &str) -> Result<Task> {
    let kind = row.kind()?;
    let lower: Vec<String> = issue.labels.iter().map(|l| l.to_lowercase()).collect();
    let mut area = cfg
        .area_by_label
        .iter()
        .find(|(label, _)| lower.contains(&label.to_lowercase()))
        .map(|(_, a)| a.clone())
        .or_else(|| cfg.default_area.clone());
    if let Some(a) = &area
        && op.area(a).await?.is_none()
    {
        area = None;
    }
    let mut milestone = cfg.milestone.clone();
    if let Some(m) = &milestone {
        let found: Option<(String,)> = sqlx::query_as("SELECT id FROM milestones WHERE namespace_id = $1 AND id = $2")
            .bind(&op.ns.id)
            .bind(m)
            .fetch_optional(&mut *op.tx)
            .await?;
        if found.is_none() {
            milestone = None;
        }
    }
    let is_bug = cfg.bug_labels.iter().any(|b| lower.contains(&b.to_lowercase()));
    let new = NewTask {
        title: clip_title(&issue.title),
        description: compose_description(kind, issue),
        area,
        milestone,
        kind: Some(if is_bug { TaskKind::Bug } else { TaskKind::Feature }),
        priority: issue.priority,
        ..NewTask::default()
    };
    let task = insert_task(op, &new, by, TaskStatus::Open, None, None).await?;
    sqlx::query(
        "INSERT INTO task_links (namespace_id, task_id, connector_id, external_id, external_key, url, external_open,
                                 external_state, external_updated_at, linked_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(&op.ns.id)
    .bind(&task.id)
    .bind(&row.id)
    .bind(&issue.external_id)
    .bind(&issue.key)
    .bind(&issue.url)
    .bind(issue.open)
    .bind(&issue.state)
    .bind(issue.updated_at)
    .bind(op.now)
    .execute(&mut *op.tx)
    .await?;
    op.emit(
        "task.imported",
        Some(&task.id),
        None,
        format!("{} imported from {} {}", task_ref(&task), kind.label(), issue.key),
        json!({ "connector_id": row.id, "external_key": issue.key, "url": issue.url }),
    )
    .await?;
    Ok(task)
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct SyncReport {
    pub fetched: usize,
    pub imported: usize,
    pub updated: usize,
    pub closed: usize,
    pub reopened: usize,
}

#[derive(sqlx::FromRow)]
struct LinkRow {
    task_id: String,
    external_open: bool,
}

async fn refresh(
    op: &mut Op,
    row: &ConnectorRow,
    link: LinkRow,
    issue: &ExtIssue,
    report: &mut SyncReport,
) -> Result<()> {
    let kind = row.kind()?;
    let Some(mut task) = op.find_task(&link.task_id).await? else { return Ok(()) };
    let status = task.status();
    let mut fields = vec![];
    if !status.is_closed() {
        let title = clip_title(&issue.title);
        if task.title != title {
            task.title = title;
            fields.push("title");
        }
        let desc = compose_description(kind, issue);
        if task.description != desc {
            task.description = desc;
            fields.push("description");
        }
    }
    let mut event: Option<(&str, String)> = None;
    if !issue.open && link.external_open && !status.is_closed() {
        task.status = TaskStatus::Cancelled.as_str().into();
        task.blocked_reason = Some(format!("closed in {} ({})", kind.label(), issue.state));
        event = Some((
            "task.cancelled",
            format!("{} cancelled: {} {} was closed", task_ref(&task), kind.label(), issue.key),
        ));
        report.closed += 1;
    } else if issue.open
        && !link.external_open
        && status == TaskStatus::Cancelled
        && task.blocked_reason.as_deref().is_some_and(|r| r.starts_with("closed in"))
    {
        task.status = TaskStatus::Open.as_str().into();
        task.assignee = None;
        task.blocked_reason = None;
        event = Some((
            "task.reopened",
            format!("{} reopened: {} {} was reopened", task_ref(&task), kind.label(), issue.key),
        ));
        report.reopened += 1;
    }
    if !fields.is_empty() || event.is_some() {
        save_task(op, &mut task).await?;
        if !fields.is_empty() {
            report.updated += 1;
            op.emit(
                "task.updated",
                Some(&task.id),
                None,
                format!("{} updated from {} {}: {}", task_ref(&task), kind.label(), issue.key, fields.join(", ")),
                json!({ "fields": fields, "connector_id": row.id }),
            )
            .await?;
        }
        if let Some((k, s)) = event {
            op.emit(k, Some(&task.id), None, s, json!({ "connector_id": row.id })).await?;
        }
    }
    sqlx::query(
        "UPDATE task_links SET external_open = $3, external_state = $4, external_updated_at = $5, url = $6
         WHERE connector_id = $1 AND external_id = $2",
    )
    .bind(&row.id)
    .bind(&issue.external_id)
    .bind(issue.open)
    .bind(&issue.state)
    .bind(issue.updated_at)
    .bind(&issue.url)
    .execute(&mut *op.tx)
    .await?;
    Ok(())
}

/// Pulls changes from the tracker. Without `force` it only runs when the connector is due;
/// the claim is an atomic update, so several hub instances never sync one connector at once.
pub async fn sync_one(hub: &Hub, ns: &str, id: &str, force: bool) -> Result<Value> {
    let now = hub.now();
    let row: Option<ConnectorRow> = sqlx::query_as(
        "UPDATE connectors SET last_sync_at = $3
         WHERE namespace_id = $1 AND id = $2 AND enabled
           AND ($4 OR last_sync_at IS NULL OR last_sync_at + make_interval(secs => sync_every_secs::double precision) <= $3)
         RETURNING *",
    )
    .bind(ns)
    .bind(id)
    .bind(now)
    .bind(force)
    .fetch_optional(&hub.pool)
    .await?;
    let Some(row) = row else {
        return Ok(json!({ "skipped": true, "reason": "disabled or not due yet" }));
    };
    let outcome = async {
        let cfg = row.config()?;
        let t = tracker(hub, &row)?;
        let issues = t.list_changed(row.cursor).await.map_err(HubError::from)?;
        let cursor = issues.iter().filter_map(|i| i.updated_at).max().max(row.cursor);
        let report = hub
            .write(ns, &row.actor(), {
                let row = row.clone();
                move |op| {
                    Box::pin(async move {
                        let mut report = SyncReport { fetched: issues.len(), ..SyncReport::default() };
                        for issue in &issues {
                            let link: Option<LinkRow> = sqlx::query_as(
                                "SELECT task_id, external_open FROM task_links WHERE connector_id = $1 AND external_id = $2",
                            )
                            .bind(&row.id)
                            .bind(&issue.external_id)
                            .fetch_optional(&mut *op.tx)
                            .await?;
                            match link {
                                Some(link) => refresh(op, &row, link, issue, &mut report).await?,
                                None if cfg.auto_import && issue.open => {
                                    import_issue(op, &row, &cfg, issue, &row.actor()).await?;
                                    report.imported += 1;
                                }
                                None => {}
                            }
                        }
                        Ok(report)
                    })
                }
            })
            .await?;
        Ok::<_, HubError>((report, cursor))
    }
    .await;
    match outcome {
        Ok((report, cursor)) => {
            sqlx::query("UPDATE connectors SET cursor = $3, last_error = NULL, last_result = $4 WHERE namespace_id = $1 AND id = $2")
                .bind(ns)
                .bind(id)
                .bind(cursor)
                .bind(Json(json!({ "at": rfc3339(now), "report": report })))
                .execute(&hub.pool)
                .await?;
            Ok(json!({ "synced": true, "report": report }))
        }
        Err(e) => {
            let msg = match &e {
                HubError::Db(_) | HubError::Internal(_) => "internal error during sync".to_string(),
                other => other.to_string(),
            };
            sqlx::query("UPDATE connectors SET last_error = $3 WHERE namespace_id = $1 AND id = $2")
                .bind(ns)
                .bind(id)
                .bind(&msg)
                .execute(&hub.pool)
                .await?;
            Err(e)
        }
    }
}

/// Attaches one issue (URL, key or number) to the namespace as a task.
pub async fn attach(hub: &Hub, ns: &str, id: &str, reference: &str, by: &str) -> Result<Value> {
    let row = load(&hub.pool, ns, id).await?;
    let cfg = row.config()?;
    let t = tracker(hub, &row)?;
    let issue = t.get(reference).await.map_err(HubError::from)?;
    if !issue.open {
        return Err(HubError::invalid(format!("{} is closed in {}", issue.key, row.kind()?.label())));
    }
    let by = by.to_string();
    hub.write(ns, &by.clone(), move |op| {
        Box::pin(async move {
            let existing: Option<(String,)> =
                sqlx::query_as("SELECT task_id FROM task_links WHERE connector_id = $1 AND external_id = $2")
                    .bind(&row.id)
                    .bind(&issue.external_id)
                    .fetch_optional(&mut *op.tx)
                    .await?;
            if let Some((task_id,)) = existing {
                return Ok(json!({ "already_linked": true, "task": op.task(&task_id).await? }));
            }
            let task = import_issue(op, &row, &cfg, &issue, &by).await?;
            Ok(json!({ "already_linked": false, "task": task }))
        })
    })
    .await
}

/// (id, task, issue key, action, attempts, last error, delivered at)
type OutboxView = (i64, String, String, String, i32, Option<String>, Option<OffsetDateTime>);

pub async fn links(pool: &sqlx::PgPool, ns: &str, id: &str) -> Result<Value> {
    let rows: Vec<(String, String, String, String, bool, String, String)> = sqlx::query_as(
        "SELECT l.task_id, l.external_key, l.url, l.external_state, l.external_open, t.status, t.title
         FROM task_links l JOIN tasks t ON t.namespace_id = l.namespace_id AND t.id = l.task_id
         WHERE l.namespace_id = $1 AND l.connector_id = $2 ORDER BY l.linked_at DESC LIMIT 200",
    )
    .bind(ns)
    .bind(id)
    .fetch_all(pool)
    .await?;
    let outbox: Vec<OutboxView> = sqlx::query_as(
        "SELECT id, task_id, external_key, action, attempts, last_error, done_at FROM connector_outbox
         WHERE namespace_id = $1 AND connector_id = $2 ORDER BY id DESC LIMIT 50",
    )
    .bind(ns)
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(json!({
        "links": rows.into_iter().map(|(task, key, url, state, open, status, title)| json!({
            "task_id": task, "title": title, "task_status": status, "external_key": key, "url": url,
            "external_state": state, "external_open": open,
        })).collect::<Vec<_>>(),
        "writes": outbox.into_iter().map(|(oid, task, key, action, attempts, err, done)| json!({
            "id": oid, "task_id": task, "external_key": key, "action": action, "attempts": attempts,
            "last_error": err, "done_at": done.map(rfc3339),
        })).collect::<Vec<_>>(),
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskEvent {
    Claimed,
    Done,
}

/// Queues tracker writes for a task change, in the caller's transaction.
pub async fn on_task_change(op: &mut Op, task: &Task, event: TaskEvent, by: &str) -> Result<()> {
    let links: Vec<(String, String, String, bool, Json<Value>)> = sqlx::query_as(
        "SELECT l.connector_id, l.external_id, l.external_key, l.external_open, c.config
         FROM task_links l JOIN connectors c ON c.id = l.connector_id
         WHERE l.namespace_id = $1 AND l.task_id = $2 AND c.enabled",
    )
    .bind(&op.ns.id)
    .bind(&task.id)
    .fetch_all(&mut *op.tx)
    .await?;
    for (connector, external_id, key, open, cfg) in links {
        let Ok(cfg) = serde_json::from_value::<Config>(cfg.0) else { continue };
        let mut writes: Vec<(&str, String)> = vec![];
        match event {
            TaskEvent::Claimed if cfg.writeback.comment_on_claim => writes
                .push(("comment", format!("{by} took this on in concord (namespace {}, task {}).", op.ns.id, task.id))),
            TaskEvent::Done => {
                if cfg.writeback.comment_on_done {
                    writes.push((
                        "comment",
                        format!("Done in concord by {by} (namespace {}, task {}).", op.ns.id, task.id),
                    ));
                }
                if cfg.writeback.close_on_done && open {
                    writes.push(("complete", String::new()));
                }
            }
            _ => {}
        }
        for (action, body) in writes {
            let dedup = format!("{connector}:{}:{:?}:{action}:{}", task.id, event, task.version);
            sqlx::query(
                "INSERT INTO connector_outbox (connector_id, namespace_id, task_id, external_id, external_key, action, body,
                                               dedup_key, next_attempt_at, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) ON CONFLICT (dedup_key) DO NOTHING",
            )
            .bind(&connector)
            .bind(&op.ns.id)
            .bind(&task.id)
            .bind(&external_id)
            .bind(&key)
            .bind(action)
            .bind(&body)
            .bind(&dedup)
            .bind(op.now)
            .execute(&mut *op.tx)
            .await?;
        }
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct OutboxRow {
    id: i64,
    connector_id: String,
    namespace_id: String,
    external_id: String,
    external_key: String,
    action: String,
    body: String,
    attempts: i32,
}

const MAX_ATTEMPTS: i32 = 10;

/// Delivers due tracker writes. Rows are leased first (next attempt pushed out), so a crash
/// mid-delivery only delays a retry; comments carry no state, so at worst one may repeat.
pub async fn deliver_due(hub: &Hub) -> Result<usize> {
    let now = hub.now();
    let rows: Vec<OutboxRow> = sqlx::query_as(
        "UPDATE connector_outbox SET next_attempt_at = $1 + interval '5 minutes', attempts = attempts + 1
         WHERE id IN (SELECT id FROM connector_outbox WHERE done_at IS NULL AND next_attempt_at <= $1 AND attempts < $2
                      ORDER BY id LIMIT 50 FOR UPDATE SKIP LOCKED)
         RETURNING id, connector_id, namespace_id, external_id, external_key, action, body, attempts",
    )
    .bind(now)
    .bind(MAX_ATTEMPTS)
    .fetch_all(&hub.pool)
    .await?;
    let mut rows = rows;
    rows.sort_by_key(|r| r.id);
    let mut delivered = 0;
    let mut clients: std::collections::HashMap<String, Result<Box<dyn Tracker>>> = std::collections::HashMap::new();
    for r in rows {
        if !clients.contains_key(&r.connector_id) {
            let t = match load(&hub.pool, &r.namespace_id, &r.connector_id).await {
                Ok(row) => tracker(hub, &row),
                Err(e) => Err(e),
            };
            clients.insert(r.connector_id.clone(), t);
        }
        let result = match clients.get(&r.connector_id) {
            Some(Ok(t)) => match r.action.as_str() {
                "comment" => t.comment(&r.external_id, &r.external_key, &r.body).await,
                _ => t.complete(&r.external_id, &r.external_key).await,
            },
            Some(Err(e)) => Err(TrackerError::Other(e.to_string())),
            None => Err(TrackerError::Other("connector missing".into())),
        };
        match result {
            Ok(()) => {
                delivered += 1;
                sqlx::query("UPDATE connector_outbox SET done_at = $2, last_error = NULL WHERE id = $1")
                    .bind(r.id)
                    .bind(hub.now())
                    .execute(&hub.pool)
                    .await?;
                if r.action == "complete" {
                    sqlx::query(
                        "UPDATE task_links SET external_open = FALSE WHERE connector_id = $1 AND external_id = $2",
                    )
                    .bind(&r.connector_id)
                    .bind(&r.external_id)
                    .execute(&hub.pool)
                    .await?;
                }
            }
            Err(e) => {
                let wait = match e {
                    TrackerError::RateLimited(s) => s.max(30) as i64,
                    _ => (60_i64 << (r.attempts - 1).clamp(0, 8)).min(6 * 3600),
                };
                tracing::warn!(connector = %r.connector_id, action = %r.action, key = %r.external_key, error = %e, "tracker write failed");
                sqlx::query("UPDATE connector_outbox SET next_attempt_at = $2, last_error = $3 WHERE id = $1")
                    .bind(r.id)
                    .bind(hub.now() + time::Duration::seconds(wait))
                    .bind(e.to_string())
                    .execute(&hub.pool)
                    .await?;
            }
        }
    }
    Ok(delivered)
}

/// Syncs every connector that is due.
pub async fn sync_due(hub: &Hub) -> Result<usize> {
    let due: Vec<(String, String)> = sqlx::query_as(
        "SELECT namespace_id, id FROM connectors WHERE enabled
           AND (last_sync_at IS NULL OR last_sync_at + make_interval(secs => sync_every_secs::double precision) <= $1)",
    )
    .bind(hub.now())
    .fetch_all(&hub.pool)
    .await?;
    let mut n = 0;
    for (ns, id) in due {
        match sync_one(hub, &ns, &id, false).await {
            Ok(v) if v["synced"] == json!(true) => n += 1,
            Ok(_) => {}
            Err(e) => tracing::warn!(connector = %id, error = %e, "tracker sync failed"),
        }
    }
    Ok(n)
}

pub async fn run(hub: Arc<Hub>, every: Duration, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(every) => {}
        }
        if let Err(e) = sync_due(&hub).await {
            tracing::warn!(error = %e, "connector sync loop");
        }
        if let Err(e) = deliver_due(&hub).await {
            tracing::warn!(error = %e, "connector outbox loop");
        }
    }
}

//! The hub: database pool, clock, notifier, and the transaction wrapper every write goes through.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;

use crate::clock::Clock;
use crate::error::{HubError, Result};
use crate::model::{Area, Member, Namespace, Role, Task};
use crate::notify::{self, Notifier};

pub struct Hub {
    pub pool: PgPool,
    pub clock: Arc<dyn Clock>,
    pub notifier: Arc<Notifier>,
    /// Base URL agents and browsers use to reach the hub (for invites and connection hints).
    pub public_url: String,
    /// HTTP client for task trackers.
    pub http: reqwest::Client,
    /// Encrypts tracker credentials at rest.
    pub secrets: Arc<crate::connectors::crypto::SecretBox>,
}

/// One write transaction on one namespace. The namespace row is locked for the whole
/// transaction, so writes inside a namespace are serialised and event seqs follow commit order.
pub struct Op {
    pub tx: Transaction<'static, Postgres>,
    pub ns: Namespace,
    pub actor: String,
    pub now: OffsetDateTime,
    start_seq: i64,
}

pub const MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

impl Hub {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>) -> Self {
        Hub {
            pool,
            clock,
            notifier: Arc::new(Notifier::new()),
            public_url: "http://127.0.0.1:8787".into(),
            http: crate::connectors::http_client(),
            secrets: Arc::new(crate::connectors::crypto::SecretBox::ephemeral()),
        }
    }

    pub fn with_secrets(mut self, secrets: crate::connectors::crypto::SecretBox) -> Self {
        self.secrets = Arc::new(secrets);
        self
    }

    pub fn with_public_url(mut self, url: impl Into<String>) -> Self {
        self.public_url = url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await.map_err(|e| HubError::Internal(format!("migration failed: {e}")))
    }

    pub fn now(&self) -> OffsetDateTime {
        self.clock.now()
    }

    pub async fn begin(&self, namespace: &str, actor: &str) -> Result<Op> {
        let mut tx = self.pool.begin().await?;
        let ns: Option<Namespace> = sqlx::query_as("SELECT * FROM namespaces WHERE id = $1 FOR UPDATE")
            .bind(namespace)
            .fetch_optional(&mut *tx)
            .await?;
        let ns = ns.ok_or_else(|| HubError::not_found(format!("namespace {namespace} not found")))?;
        Ok(Op { tx, start_seq: ns.event_seq, ns, actor: actor.to_string(), now: self.clock.now() })
    }

    /// Like `begin`, but returns `None` instead of waiting when another transaction holds the
    /// namespace. Background jobs use it so they never queue behind requests (or each other).
    pub async fn try_begin(&self, namespace: &str, actor: &str) -> Result<Option<Op>> {
        let mut tx = self.pool.begin().await?;
        let ns: Option<Namespace> = sqlx::query_as("SELECT * FROM namespaces WHERE id = $1 FOR UPDATE SKIP LOCKED")
            .bind(namespace)
            .fetch_optional(&mut *tx)
            .await?;
        Ok(ns.map(|ns| Op { tx, start_seq: ns.event_seq, ns, actor: actor.to_string(), now: self.clock.now() }))
    }

    pub async fn commit(&self, op: Op) -> Result<()> {
        let Op { mut tx, ns, start_seq, .. } = op;
        sqlx::query(
            "UPDATE namespaces SET goal = $2, plan_version = $3, event_seq = $4, next_task_no = $5,
                 next_negotiation_no = $6, next_milestone_no = $7
             WHERE id = $1",
        )
        .bind(&ns.id)
        .bind(&ns.goal)
        .bind(ns.plan_version)
        .bind(ns.event_seq)
        .bind(ns.next_task_no)
        .bind(ns.next_negotiation_no)
        .bind(ns.next_milestone_no)
        .execute(&mut *tx)
        .await?;
        let emitted = ns.event_seq > start_seq;
        if emitted {
            // NOTIFY is transactional: delivered to other instances only if we commit.
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(notify::CHANNEL)
                .bind(notify::payload(&ns.id, ns.event_seq))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        if emitted {
            self.notifier.publish(&ns.id, ns.event_seq);
        }
        Ok(())
    }

    /// Runs `f` inside a namespace write transaction. Commits on `Ok`, rolls back on `Err`.
    pub async fn write<T, F>(&self, namespace: &str, actor: &str, f: F) -> Result<T>
    where
        T: Send,
        F: for<'a> FnOnce(&'a mut Op) -> BoxFuture<'a, Result<T>> + Send,
    {
        let mut op = self.begin(namespace, actor).await?;
        let out = f(&mut op).await?;
        self.commit(op).await?;
        Ok(out)
    }
}

impl Op {
    pub fn conn(&mut self) -> &mut sqlx::PgConnection {
        &mut self.tx
    }

    /// Appends an event. `audience = None` means every member sees it.
    pub async fn emit(
        &mut self,
        kind: &str,
        entity: Option<&str>,
        audience: Option<Vec<String>>,
        summary: impl Into<String>,
        data: Value,
    ) -> Result<i64> {
        self.ns.event_seq += 1;
        let seq = self.ns.event_seq;
        sqlx::query(
            "INSERT INTO events (namespace_id, seq, at, actor, kind, entity, audience, summary, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&self.ns.id)
        .bind(seq)
        .bind(self.now)
        .bind(&self.actor)
        .bind(kind)
        .bind(entity)
        .bind(audience.map(Json))
        .bind(summary.into())
        .bind(Json(data))
        .execute(&mut *self.tx)
        .await?;
        Ok(seq)
    }

    /// Records a line in the plan's decision log (and the matching event).
    pub async fn decide(
        &mut self,
        negotiation: Option<&str>,
        outcome: &str,
        summary: impl Into<String>,
    ) -> Result<i64> {
        let summary = summary.into();
        let seq = self
            .emit(
                "decision.recorded",
                negotiation,
                None,
                format!("decision ({outcome}): {summary}"),
                serde_json::json!({ "outcome": outcome, "negotiation_id": negotiation }),
            )
            .await?;
        sqlx::query(
            "INSERT INTO decisions (namespace_id, seq, negotiation_id, outcome, summary, actor, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&self.ns.id)
        .bind(seq)
        .bind(negotiation)
        .bind(outcome)
        .bind(&summary)
        .bind(&self.actor)
        .bind(self.now)
        .execute(&mut *self.tx)
        .await?;
        Ok(seq)
    }

    pub async fn task(&mut self, id: &str) -> Result<Task> {
        self.find_task(id).await?.ok_or_else(|| HubError::not_found(format!("task {id} not found")))
    }

    pub async fn find_task(&mut self, id: &str) -> Result<Option<Task>> {
        Ok(sqlx::query_as("SELECT * FROM tasks WHERE namespace_id = $1 AND id = $2")
            .bind(&self.ns.id)
            .bind(id.trim())
            .fetch_optional(&mut *self.tx)
            .await?)
    }

    pub async fn area(&mut self, name: &str) -> Result<Option<Area>> {
        Ok(sqlx::query_as("SELECT * FROM areas WHERE namespace_id = $1 AND name = $2")
            .bind(&self.ns.id)
            .bind(name)
            .fetch_optional(&mut *self.tx)
            .await?)
    }

    pub async fn areas(&mut self) -> Result<Vec<Area>> {
        Ok(sqlx::query_as("SELECT * FROM areas WHERE namespace_id = $1 ORDER BY name")
            .bind(&self.ns.id)
            .fetch_all(&mut *self.tx)
            .await?)
    }

    pub async fn area_owner(&mut self, area: Option<&str>) -> Result<Option<String>> {
        match area {
            None => Ok(None),
            Some(a) => Ok(self.area(a).await?.and_then(|a| a.owner)),
        }
    }

    pub async fn member(&mut self, id: &str) -> Result<Option<Member>> {
        Ok(sqlx::query_as("SELECT * FROM members WHERE namespace_id = $1 AND id = $2")
            .bind(&self.ns.id)
            .bind(id)
            .fetch_optional(&mut *self.tx)
            .await?)
    }

    /// A member who can work: active and not a viewer.
    pub async fn active_member(&mut self, id: &str) -> Result<Member> {
        match self.member(id).await? {
            Some(m) if m.active && m.role != Role::Viewer.as_str() => Ok(m),
            Some(m) if m.active => Err(HubError::invalid(format!("{id} is a viewer in this namespace (read-only)"))),
            _ => Err(HubError::invalid(format!("{id} is not an active member of this namespace"))),
        }
    }

    /// Members who can work (viewers excluded).
    pub async fn active_members(&mut self) -> Result<Vec<Member>> {
        Ok(sqlx::query_as("SELECT * FROM members WHERE namespace_id = $1 AND active AND role <> 'viewer' ORDER BY id")
            .bind(&self.ns.id)
            .fetch_all(&mut *self.tx)
            .await?)
    }

    /// Members who approve plan-level changes: leads and admins, or every working member if there are none.
    pub async fn approvers(&mut self) -> Result<Vec<String>> {
        let members = self.active_members().await?;
        let role = |m: &Member| Role::parse(&m.role).unwrap_or(Role::Member);
        let leads: Vec<String> = members.iter().filter(|m| role(m).approves_plan()).map(|m| m.id.clone()).collect();
        Ok(if leads.is_empty() { members.into_iter().map(|m| m.id).collect() } else { leads })
    }

    pub fn next_task_id(&mut self) -> String {
        let id = format!("T-{}", self.ns.next_task_no);
        self.ns.next_task_no += 1;
        id
    }

    pub fn next_negotiation_id(&mut self) -> String {
        let id = format!("N-{}", self.ns.next_negotiation_no);
        self.ns.next_negotiation_no += 1;
        id
    }

    pub fn next_milestone_id(&mut self) -> String {
        let id = format!("M-{}", self.ns.next_milestone_no);
        self.ns.next_milestone_no += 1;
        id
    }

    pub fn bump_plan(&mut self) {
        self.ns.plan_version += 1;
    }

    /// Fails if an escalated negotiation has frozen any of `targets`.
    pub async fn ensure_not_frozen(&mut self, targets: &[String]) -> Result<()> {
        if targets.is_empty() {
            return Ok(());
        }
        let frozen: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM negotiations
             WHERE namespace_id = $1 AND status = 'escalated' AND targets ?| $2 LIMIT 1",
        )
        .bind(&self.ns.id)
        .bind(targets)
        .fetch_optional(&mut *self.tx)
        .await?;
        match frozen {
            Some((id,)) => Err(HubError::conflict(
                format!("frozen: negotiation {id} on this object is escalated to the humans; wait for their decision"),
                serde_json::json!({ "negotiation_id": id }),
            )),
            None => Ok(()),
        }
    }

    /// Fails if a negotiation that is not closed already covers any of `targets`.
    pub async fn ensure_no_open_negotiation(&mut self, targets: &[String], except: Option<&str>) -> Result<()> {
        if targets.is_empty() {
            return Ok(());
        }
        let open: Option<(String, String)> = sqlx::query_as(
            "SELECT id, status FROM negotiations
             WHERE namespace_id = $1 AND status <> 'closed' AND targets ?| $2 AND id <> $3 LIMIT 1",
        )
        .bind(&self.ns.id)
        .bind(targets)
        .bind(except.unwrap_or(""))
        .fetch_optional(&mut *self.tx)
        .await?;
        match open {
            Some((id, status)) => Err(HubError::conflict(
                format!(
                    "negotiation {id} ({status}) already covers this; answer or counter there instead of opening a new one"
                ),
                serde_json::json!({ "negotiation_id": id }),
            )),
            None => Ok(()),
        }
    }
}

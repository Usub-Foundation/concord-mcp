//! Operator actions (CLI): namespaces, members, tokens, forced resolution.

use serde_json::{Value, json};
use sqlx::types::Json;

use super::{MAX_NOTE, MAX_TEXT, MAX_TITLE, check_len, non_empty, valid_slug};
use crate::access;
use crate::error::{HubError, Result};
use crate::hub::Hub;
use crate::model::{Change, Role};

#[derive(Debug, Clone)]
pub struct NamespaceSettings {
    pub max_rounds: i32,
    pub turn_timeout: time::Duration,
    pub escalation_timeout: time::Duration,
    pub lease: time::Duration,
}

impl Default for NamespaceSettings {
    fn default() -> Self {
        NamespaceSettings {
            max_rounds: 4,
            turn_timeout: time::Duration::minutes(30),
            escalation_timeout: time::Duration::hours(24),
            lease: time::Duration::hours(2),
        }
    }
}

pub async fn create_namespace(
    hub: &Hub,
    id: &str,
    name: &str,
    goal: &str,
    s: &NamespaceSettings,
    by: &str,
) -> Result<()> {
    valid_slug("namespace", id)?;
    let name = non_empty("name", name, MAX_TITLE)?;
    check_len("goal", goal, MAX_TEXT)?;
    if !(1..=20).contains(&s.max_rounds) {
        return Err(HubError::invalid("max_rounds must be between 1 and 20"));
    }
    let now = hub.now();
    let r = sqlx::query(
        "INSERT INTO namespaces (id, name, goal, max_rounds, turn_timeout_secs, escalation_timeout_secs, lease_secs, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(name)
    .bind(goal.trim())
    .bind(s.max_rounds)
    .bind(s.turn_timeout.whole_seconds())
    .bind(s.escalation_timeout.whole_seconds())
    .bind(s.lease.whole_seconds())
    .bind(now)
    .execute(&hub.pool)
    .await?;
    if r.rows_affected() == 0 {
        return Err(HubError::conflict(format!("namespace {id} already exists"), json!({})));
    }
    hub.write(id, by, |op| {
        Box::pin(async move {
            op.emit("namespace.created", None, None, format!("namespace {} created", op.ns.id), json!({})).await?;
            Ok(())
        })
    })
    .await
}

pub struct NewMember {
    pub id: String,
    pub display_name: String,
    pub human: String,
    pub lead: bool,
    /// Explicit role; `None` = lead if `lead` or the namespace has no approver yet, else member.
    pub role: Option<Role>,
    /// (area name, path globs) owned by the new member.
    pub areas: Vec<(String, Vec<String>)>,
}

/// Adds a member to `ns`: creates the principal if needed, grants access and assigns areas.
/// Returns a fresh token when the principal was created here (an existing principal keeps its
/// tokens, which now reach `ns` as well).
pub async fn add_member(hub: &Hub, ns: &str, m: NewMember) -> Result<Option<String>> {
    valid_slug("member", &m.id)?;
    check_len("human", &m.human, MAX_TITLE)?;
    for (a, _) in &m.areas {
        valid_slug("area", a)?;
    }
    let exists: Option<(bool,)> = sqlx::query_as("SELECT active FROM members WHERE namespace_id = $1 AND id = $2")
        .bind(ns)
        .bind(&m.id)
        .fetch_optional(&hub.pool)
        .await?;
    if exists.is_some_and(|(active,)| active) {
        return Err(HubError::conflict(format!("member {} already exists in {ns}", m.id), json!({})));
    }
    let created = access::actor_by_id(&hub.pool, &m.id).await?.is_none();
    if created {
        let display = non_empty("name", if m.display_name.is_empty() { &m.id } else { &m.display_name }, MAX_TITLE)?;
        access::create_principal(
            hub,
            &access::NewPrincipal {
                id: m.id.clone(),
                display_name: display,
                human: m.human.clone(),
                is_admin: false,
                grants: vec![],
            },
        )
        .await?;
    }
    let (has_approver,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM members WHERE namespace_id = $1 AND role IN ('lead', 'admin') AND active)",
    )
    .bind(ns)
    .fetch_one(&hub.pool)
    .await?;
    // The first member becomes a lead so plan-level changes always have an approver.
    let role = m.role.unwrap_or(if m.lead || !has_approver { Role::Lead } else { Role::Member });
    access::set_grant(hub, &m.id, ns, role).await?;
    let actor =
        access::actor_by_id(&hub.pool, &m.id).await?.ok_or_else(|| HubError::Internal("principal vanished".into()))?;
    access::enter(hub, &actor, ns).await?;
    if !m.areas.is_empty() {
        let (id, areas) = (m.id.clone(), m.areas.clone());
        hub.write(ns, "operator", move |op| {
            Box::pin(async move {
                for (area, paths) in &areas {
                    let current = op.area(area).await?;
                    if let Some(owner) = current.as_ref().and_then(|a| a.owner.clone())
                        && owner != id
                    {
                        return Err(HubError::conflict(
                            format!("area {area} is already owned by {owner}; re-own it through a proposal"),
                            json!({}),
                        ));
                    }
                    super::changes::apply(
                        op,
                        "operator",
                        &[Change::SetArea {
                            name: area.clone(),
                            owner: Some(id.clone()),
                            shared: false,
                            paths: if paths.is_empty() { None } else { Some(paths.clone()) },
                            description: None,
                        }],
                        None,
                    )
                    .await?;
                }
                Ok(())
            })
        })
        .await?;
    }
    if created {
        let (_, token) = access::issue_token(hub, &m.id, &format!("first token ({ns})"), "operator").await?;
        return Ok(Some(token));
    }
    Ok(None)
}

/// Removes a member from `ns` by revoking their grant there.
pub async fn remove_member(hub: &Hub, ns: &str, member: &str) -> Result<Value> {
    let actor = access::actor_by_id(&hub.pool, member)
        .await?
        .ok_or_else(|| HubError::not_found(format!("{member} not found")))?;
    if actor.reaches_all() && !actor.grants.iter().any(|g| g.namespace == ns) {
        return Err(HubError::conflict(
            format!("{member} reaches every namespace (grant '*' or hub admin); change their grants instead"),
            json!({}),
        ));
    }
    access::remove_grant(hub, member, ns).await?;
    Ok(json!({ "removed": member, "namespace": ns }))
}

/// Closes a membership: tasks go back to the pool, owned areas become shared, sessions end.
pub async fn close_membership(hub: &Hub, ns: &str, member: &str) -> Result<Value> {
    let member = member.to_string();
    hub.write(ns, "operator", move |op| {
        Box::pin(async move {
            let r = sqlx::query("UPDATE members SET active = FALSE WHERE namespace_id = $1 AND id = $2 AND active")
                .bind(&op.ns.id)
                .bind(&member)
                .execute(&mut *op.tx)
                .await?;
            if r.rows_affected() == 0 {
                return Ok(json!({ "removed": member, "released_tasks": [] }));
            }
            let released: Vec<(String,)> = sqlx::query_as(
                "UPDATE tasks SET status = 'open', assignee = NULL, lease_until = NULL, version = version + 1, updated_at = $3
                 WHERE namespace_id = $1 AND assignee = $2 AND status IN ('claimed', 'in_progress', 'blocked', 'review')
                 RETURNING id",
            )
            .bind(&op.ns.id)
            .bind(&member)
            .bind(op.now)
            .fetch_all(&mut *op.tx)
            .await?;
            sqlx::query("UPDATE areas SET owner = NULL WHERE namespace_id = $1 AND owner = $2")
                .bind(&op.ns.id)
                .bind(&member)
                .execute(&mut *op.tx)
                .await?;
            sqlx::query("DELETE FROM mcp_sessions WHERE namespace_id = $1 AND member_id = $2")
                .bind(&op.ns.id)
                .bind(&member)
                .execute(&mut *op.tx)
                .await?;
            let ids: Vec<String> = released.into_iter().map(|(id,)| id).collect();
            op.emit(
                "member.removed",
                Some(&member),
                None,
                format!("{member} left; released {} task(s), their areas are shared now", ids.len()),
                json!({ "released": ids }),
            )
            .await?;
            Ok(json!({ "removed": member, "released_tasks": ids }))
        })
    })
    .await
}

pub async fn resolve(
    hub: &Hub,
    ns: &str,
    negotiation: &str,
    round: Option<i32>,
    note: &str,
    by: &str,
) -> Result<Value> {
    check_len("note", note, MAX_NOTE)?;
    let (negotiation, note) = (negotiation.to_string(), note.to_string());
    hub.write(ns, by, move |op| {
        Box::pin(async move { super::negotiate::admin_resolve(op, &negotiation, round, &note).await })
    })
    .await
}

pub async fn list_namespaces(hub: &Hub) -> Result<Vec<Value>> {
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT w.id, w.name, (SELECT COUNT(*) FROM members m WHERE m.namespace_id = w.id AND m.active),
                (SELECT COUNT(*) FROM tasks t WHERE t.namespace_id = w.id AND t.status NOT IN ('done','cancelled','rejected'))
         FROM namespaces w ORDER BY w.id",
    )
    .fetch_all(&hub.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, members, open)| json!({ "id": id, "name": name, "members": members, "open_tasks": open }))
        .collect())
}

type MemberRow = (String, String, String, String, Json<Vec<String>>);

pub async fn list_members(hub: &Hub, ns: &str) -> Result<Vec<Value>> {
    let rows: Vec<MemberRow> = sqlx::query_as(
        "SELECT m.id, m.display_name, m.human, m.role,
                COALESCE((SELECT jsonb_agg(a.name ORDER BY a.name) FROM areas a WHERE a.namespace_id = m.namespace_id AND a.owner = m.id), '[]')
         FROM members m WHERE m.namespace_id = $1 AND m.active ORDER BY m.id",
    )
    .bind(ns)
    .fetch_all(&hub.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, human, role, areas)| json!({ "id": id, "name": name, "human": human, "role": role, "areas": areas.0 }))
        .collect())
}

//! Who may do what: principals, their grants and tokens, and membership in namespaces.
//!
//! A principal (a person or their agent) authenticates with a token and reaches namespaces
//! through grants: a namespace id or `*` (every namespace, including future ones), each with a
//! role. An explicit grant for a namespace wins over `*`. Hub admins reach everything as admin.

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::{HubError, Result};
use crate::hub::Hub;
use crate::model::{Caller, Role, ser_opt_ts, ser_ts};
use crate::ops::{non_empty, valid_slug};
use crate::token;

pub const ALL: &str = "*";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grant {
    pub namespace: String,
    pub role: Role,
}

/// An authenticated principal with everything needed for permission checks.
#[derive(Debug, Clone)]
pub struct Actor {
    pub principal: String,
    pub display_name: String,
    pub human: String,
    pub is_admin: bool,
    pub grants: Vec<Grant>,
    pub token_id: Option<String>,
}

impl Actor {
    /// The operator running the CLI on the hub machine.
    pub fn operator() -> Actor {
        Actor {
            principal: "admin".into(),
            display_name: "hub operator".into(),
            human: String::new(),
            is_admin: true,
            grants: vec![],
            token_id: None,
        }
    }

    pub fn role_in(&self, ns: &str) -> Option<Role> {
        if let Some(g) = self.grants.iter().find(|g| g.namespace == ns) {
            return Some(g.role);
        }
        if let Some(g) = self.grants.iter().find(|g| g.namespace == ALL) {
            return Some(g.role);
        }
        self.is_admin.then_some(Role::Admin)
    }

    pub fn reaches_all(&self) -> bool {
        self.is_admin || self.grants.iter().any(|g| g.namespace == ALL)
    }

    pub fn can_read(&self, ns: &str) -> bool {
        self.role_in(ns).is_some()
    }

    pub fn can_admin(&self, ns: &str) -> bool {
        self.role_in(ns).is_some_and(|r| r.manages())
    }

    /// May give someone access to `ns` (`*` needs a hub admin).
    pub fn can_grant(&self, ns: &str) -> bool {
        self.is_admin || (ns != ALL && self.can_admin(ns))
    }

    /// May manage another principal: hub admins always; namespace admins only principals whose
    /// every grant lies in namespaces they administer.
    pub fn can_manage(&self, target_is_admin: bool, target_grants: &[Grant]) -> bool {
        if self.is_admin {
            return true;
        }
        !target_is_admin && !target_grants.is_empty() && target_grants.iter().all(|g| self.can_grant(&g.namespace))
    }
}

fn role_of(s: &str) -> Role {
    Role::parse(s).unwrap_or(Role::Viewer)
}

async fn grants_of(pool: &PgPool, principal: &str) -> Result<Vec<Grant>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT namespace, role FROM grants WHERE principal_id = $1 ORDER BY namespace")
            .bind(principal)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(namespace, role)| Grant { namespace, role: role_of(&role) }).collect())
}

/// Looks up an active principal by raw bearer token. Returns `None` for unknown or revoked tokens.
pub async fn actor_by_token(hub: &Hub, raw: &str) -> Result<Option<Actor>> {
    let row: Option<(String, String, String, bool, String)> = sqlx::query_as(
        "SELECT p.id, p.display_name, p.human, p.is_admin, t.id FROM tokens t JOIN principals p ON p.id = t.principal_id
         WHERE t.token_hash = $1 AND t.revoked_at IS NULL AND p.active",
    )
    .bind(token::hash(raw))
    .fetch_optional(&hub.pool)
    .await?;
    let Some((principal, display_name, human, is_admin, token_id)) = row else { return Ok(None) };
    sqlx::query("UPDATE tokens SET last_used_at = $2 WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < $2 - interval '1 minute')")
        .bind(&token_id)
        .bind(hub.now())
        .execute(&hub.pool)
        .await?;
    let grants = grants_of(&hub.pool, &principal).await?;
    Ok(Some(Actor { principal, display_name, human, is_admin, grants, token_id: Some(token_id) }))
}

pub async fn actor_by_id(pool: &PgPool, principal: &str) -> Result<Option<Actor>> {
    let row: Option<(String, String, String, bool)> =
        sqlx::query_as("SELECT id, display_name, human, is_admin FROM principals WHERE id = $1 AND active")
            .bind(principal)
            .fetch_optional(pool)
            .await?;
    let Some((principal, display_name, human, is_admin)) = row else { return Ok(None) };
    let grants = grants_of(pool, &principal).await?;
    Ok(Some(Actor { principal, display_name, human, is_admin, grants, token_id: None }))
}

#[derive(Debug, Clone, Serialize)]
pub struct NamespaceAccess {
    pub id: String,
    pub name: String,
    pub role: Role,
}

/// Every namespace the actor reaches, with its role there.
pub async fn namespaces_of(pool: &PgPool, actor: &Actor) -> Result<Vec<NamespaceAccess>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, name FROM namespaces ORDER BY id").fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, name)| actor.role_in(&id).map(|role| NamespaceAccess { id, name, role }))
        .collect())
}

/// Makes sure the actor is a member of `ns` with the role its grants give, and returns the caller.
/// The first access through a `*` grant creates the membership and announces it.
pub async fn enter(hub: &Hub, actor: &Actor, ns: &str) -> Result<Caller> {
    let role =
        actor.role_in(ns).ok_or_else(|| HubError::not_found(format!("namespace {ns} not found or not accessible")))?;
    let (principal, display, human) = (actor.principal.clone(), actor.display_name.clone(), actor.human.clone());
    let current: Option<(String, bool)> =
        sqlx::query_as("SELECT role, active FROM members WHERE namespace_id = $1 AND id = $2")
            .bind(ns)
            .bind(&principal)
            .fetch_optional(&hub.pool)
            .await?;
    if current.as_ref().is_none_or(|(r, active)| r != role.as_str() || !active) {
        let exists: Option<(String,)> =
            sqlx::query_as("SELECT id FROM namespaces WHERE id = $1").bind(ns).fetch_optional(&hub.pool).await?;
        if exists.is_none() {
            return Err(HubError::not_found(format!("namespace {ns} not found or not accessible")));
        }
        hub.write(ns, &principal.clone(), move |op| {
            Box::pin(async move {
                let inserted: (bool,) = sqlx::query_as(
                    "INSERT INTO members (namespace_id, id, display_name, human, role, created_at)
                     VALUES ($1, $2, $3, $4, $5, $6)
                     ON CONFLICT (namespace_id, id) DO UPDATE SET role = EXCLUDED.role, active = TRUE
                     RETURNING (xmax = 0)",
                )
                .bind(&op.ns.id)
                .bind(&principal)
                .bind(&display)
                .bind(&human)
                .bind(role.as_str())
                .bind(op.now)
                .fetch_one(&mut *op.tx)
                .await?;
                let (kind, summary) = if inserted.0 {
                    ("member.joined", format!("{principal} joined as {role}"))
                } else {
                    ("member.role_changed", format!("{principal} is now {role}"))
                };
                op.emit(kind, Some(&principal), None, summary, json!({ "role": role })).await?;
                Ok(())
            })
        })
        .await?;
    }
    sqlx::query("UPDATE members SET last_seen_at = $3 WHERE namespace_id = $1 AND id = $2")
        .bind(ns)
        .bind(&actor.principal)
        .bind(hub.now())
        .execute(&hub.pool)
        .await?;
    Ok(Caller { namespace: ns.to_string(), member: actor.principal.clone(), role })
}

#[derive(Debug, Clone)]
pub struct NewPrincipal {
    pub id: String,
    pub display_name: String,
    pub human: String,
    pub is_admin: bool,
    pub grants: Vec<Grant>,
}

pub async fn create_principal(hub: &Hub, p: &NewPrincipal) -> Result<()> {
    valid_slug("principal", &p.id)?;
    if matches!(p.id.as_str(), "system" | "operator") || p.id.starts_with("connector") {
        return Err(HubError::invalid("this id is reserved"));
    }
    let display = non_empty("name", if p.display_name.trim().is_empty() { &p.id } else { &p.display_name }, 200)?;
    let mut tx = hub.pool.begin().await?;
    let r = sqlx::query(
        "INSERT INTO principals (id, display_name, human, is_admin, created_at) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&p.id)
    .bind(display)
    .bind(p.human.trim())
    .bind(p.is_admin)
    .bind(hub.now())
    .execute(&mut *tx)
    .await?;
    if r.rows_affected() == 0 {
        return Err(HubError::conflict(format!("principal {} already exists", p.id), json!({})));
    }
    for g in &p.grants {
        check_grant_target(&mut tx, &g.namespace).await?;
        sqlx::query("INSERT INTO grants (principal_id, namespace, role, created_at) VALUES ($1, $2, $3, $4)")
            .bind(&p.id)
            .bind(&g.namespace)
            .bind(g.role.as_str())
            .bind(hub.now())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn check_grant_target(conn: &mut sqlx::PgConnection, ns: &str) -> Result<()> {
    if ns == ALL {
        return Ok(());
    }
    let exists: Option<(String,)> =
        sqlx::query_as("SELECT id FROM namespaces WHERE id = $1").bind(ns).fetch_optional(conn).await?;
    match exists {
        Some(_) => Ok(()),
        None => Err(HubError::invalid(format!("namespace {ns} does not exist"))),
    }
}

/// Gives `principal` a role in `ns` (or `*`), replacing a previous grant there. An existing
/// membership follows the new role.
pub async fn set_grant(hub: &Hub, principal: &str, ns: &str, role: Role) -> Result<()> {
    let mut conn = hub.pool.acquire().await?;
    check_grant_target(&mut conn, ns).await?;
    drop(conn);
    principal_exists(&hub.pool, principal).await?;
    sqlx::query(
        "INSERT INTO grants (principal_id, namespace, role, created_at) VALUES ($1, $2, $3, $4)
         ON CONFLICT (principal_id, namespace) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(principal)
    .bind(ns)
    .bind(role.as_str())
    .bind(hub.now())
    .execute(&hub.pool)
    .await?;
    resync_memberships(hub, principal).await
}

/// Removes the grant for `ns`; if that ends the principal's access there, the membership is closed.
pub async fn remove_grant(hub: &Hub, principal: &str, ns: &str) -> Result<()> {
    let r = sqlx::query("DELETE FROM grants WHERE principal_id = $1 AND namespace = $2")
        .bind(principal)
        .bind(ns)
        .execute(&hub.pool)
        .await?;
    if r.rows_affected() == 0 {
        return Err(HubError::not_found(format!("{principal} has no grant for {ns}")));
    }
    resync_memberships(hub, principal).await
}

/// Brings every membership of `principal` in line with its grants: roles follow, memberships
/// without access are closed (tasks released, areas shared).
pub async fn resync_memberships(hub: &Hub, principal: &str) -> Result<()> {
    let actor = actor_by_id(&hub.pool, principal).await?;
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT namespace_id, role FROM members WHERE id = $1 AND active")
        .bind(principal)
        .fetch_all(&hub.pool)
        .await?;
    for (ns, role) in rows {
        match actor.as_ref().and_then(|a| a.role_in(&ns)) {
            Some(new) if new.as_str() == role => {}
            Some(_) => {
                if let Some(a) = &actor {
                    enter(hub, a, &ns).await?;
                }
            }
            None => {
                crate::ops::admin::close_membership(hub, &ns, principal).await?;
            }
        }
    }
    Ok(())
}

async fn principal_exists(pool: &PgPool, principal: &str) -> Result<()> {
    let found: Option<(String,)> =
        sqlx::query_as("SELECT id FROM principals WHERE id = $1").bind(principal).fetch_optional(pool).await?;
    found.map(|_| ()).ok_or_else(|| HubError::not_found(format!("principal {principal} not found")))
}

/// Issues a new token. Returns (token id, raw token); the raw token is shown once.
pub async fn issue_token(hub: &Hub, principal: &str, label: &str, created_by: &str) -> Result<(String, String)> {
    principal_exists(&hub.pool, principal).await?;
    let raw = token::generate();
    let id = format!("tok_{}", &token::generate()[4..16]);
    sqlx::query(
        "INSERT INTO tokens (id, principal_id, token_hash, label, created_by, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&id)
    .bind(principal)
    .bind(token::hash(&raw))
    .bind(label.trim().chars().take(100).collect::<String>())
    .bind(created_by)
    .bind(hub.now())
    .execute(&hub.pool)
    .await?;
    Ok((id, raw))
}

/// Revokes a token; its MCP sessions end too. Returns the owner.
pub async fn revoke_token(hub: &Hub, token_id: &str) -> Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("UPDATE tokens SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL RETURNING principal_id")
            .bind(token_id)
            .bind(hub.now())
            .fetch_optional(&hub.pool)
            .await?;
    let (principal,) = row.ok_or_else(|| HubError::not_found(format!("active token {token_id} not found")))?;
    Ok(principal)
}

pub async fn token_owner(pool: &PgPool, token_id: &str) -> Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT principal_id FROM tokens WHERE id = $1").bind(token_id).fetch_optional(pool).await?;
    row.map(|r| r.0).ok_or_else(|| HubError::not_found(format!("token {token_id} not found")))
}

/// Deactivates a principal: tokens revoked, UI sessions dropped, memberships closed.
pub async fn deactivate(hub: &Hub, principal: &str) -> Result<()> {
    principal_exists(&hub.pool, principal).await?;
    let now = hub.now();
    sqlx::query("UPDATE principals SET active = FALSE WHERE id = $1").bind(principal).execute(&hub.pool).await?;
    sqlx::query("UPDATE tokens SET revoked_at = $2 WHERE principal_id = $1 AND revoked_at IS NULL")
        .bind(principal)
        .bind(now)
        .execute(&hub.pool)
        .await?;
    sqlx::query("DELETE FROM ui_sessions WHERE principal_id = $1").bind(principal).execute(&hub.pool).await?;
    sqlx::query("DELETE FROM mcp_sessions WHERE member_id = $1").bind(principal).execute(&hub.pool).await?;
    resync_memberships(hub, principal).await
}

pub async fn set_admin(hub: &Hub, principal: &str, is_admin: bool) -> Result<()> {
    principal_exists(&hub.pool, principal).await?;
    if !is_admin {
        let (others,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM principals WHERE is_admin AND active AND id <> $1")
                .bind(principal)
                .fetch_one(&hub.pool)
                .await?;
        if others == 0 {
            return Err(HubError::conflict("this is the last hub admin", json!({})));
        }
    }
    sqlx::query("UPDATE principals SET is_admin = $2 WHERE id = $1")
        .bind(principal)
        .bind(is_admin)
        .execute(&hub.pool)
        .await?;
    resync_memberships(hub, principal).await
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TokenView {
    pub id: String,
    pub label: String,
    pub created_by: String,
    #[serde(serialize_with = "ser_ts")]
    pub created_at: OffsetDateTime,
    #[serde(serialize_with = "ser_opt_ts")]
    pub last_used_at: Option<OffsetDateTime>,
    #[serde(serialize_with = "ser_opt_ts")]
    pub revoked_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrincipalView {
    pub id: String,
    pub display_name: String,
    pub human: String,
    pub is_admin: bool,
    pub active: bool,
    pub grants: Vec<Grant>,
    pub tokens: Vec<TokenView>,
}

pub async fn principal_view(pool: &PgPool, id: &str) -> Result<PrincipalView> {
    let row: Option<(String, String, String, bool, bool)> =
        sqlx::query_as("SELECT id, display_name, human, is_admin, active FROM principals WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let (id, display_name, human, is_admin, active) =
        row.ok_or_else(|| HubError::not_found(format!("principal {id} not found")))?;
    let grants = grants_of(pool, &id).await?;
    let tokens: Vec<TokenView> = sqlx::query_as(
        "SELECT id, label, created_by, created_at, last_used_at, revoked_at FROM tokens WHERE principal_id = $1
         ORDER BY created_at DESC",
    )
    .bind(&id)
    .fetch_all(pool)
    .await?;
    Ok(PrincipalView { id, display_name, human, is_admin, active, grants, tokens })
}

pub async fn principals(pool: &PgPool) -> Result<Vec<PrincipalView>> {
    let ids: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM principals ORDER BY active DESC, id").fetch_all(pool).await?;
    let mut out = Vec::with_capacity(ids.len());
    for (id,) in ids {
        out.push(principal_view(pool, &id).await?);
    }
    Ok(out)
}

/// Ready-to-use connection details for every namespace a principal reaches.
pub async fn connections(hub: &Hub, principal: &str, raw_token: &str) -> Result<Value> {
    let actor = actor_by_id(&hub.pool, principal).await?.ok_or_else(|| HubError::not_found("principal not active"))?;
    let base = hub.public_url();
    let list: Vec<Value> = namespaces_of(&hub.pool, &actor)
        .await?
        .into_iter()
        .map(|n| {
            let url = format!("{base}/mcp/{}", n.id);
            json!({
                "namespace": n.id,
                "role": n.role,
                "url": url,
                "claude_code": format!("claude mcp add --transport http concord-{} {url} --header \"Authorization: Bearer {raw_token}\"", n.id),
            })
        })
        .collect();
    Ok(json!({ "connections": list, "all_namespaces": actor.reaches_all() }))
}

/// Parses "ns:role" (role defaults to member), e.g. "shop:lead" or "*:viewer".
pub fn parse_grant(s: &str) -> Result<Grant> {
    let (ns, role) = s.split_once(':').unwrap_or((s, "member"));
    let role = Role::parse(role.trim())
        .ok_or_else(|| HubError::invalid(format!("unknown role {role:?}: viewer, member, lead or admin")))?;
    let ns = ns.trim();
    if ns != ALL {
        valid_slug("namespace", ns)?;
    }
    Ok(Grant { namespace: ns.to_string(), role })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(admin: bool, grants: &[(&str, Role)]) -> Actor {
        Actor {
            principal: "x".into(),
            display_name: "x".into(),
            human: String::new(),
            is_admin: admin,
            grants: grants.iter().map(|(n, r)| Grant { namespace: n.to_string(), role: *r }).collect(),
            token_id: None,
        }
    }

    #[test]
    fn explicit_grant_beats_wildcard() {
        let a = actor(false, &[("*", Role::Member), ("secret", Role::Viewer)]);
        assert_eq!(a.role_in("shop"), Some(Role::Member));
        assert_eq!(a.role_in("secret"), Some(Role::Viewer));
        assert!(a.reaches_all());
        let b = actor(false, &[("shop", Role::Lead)]);
        assert_eq!(b.role_in("shop"), Some(Role::Lead));
        assert_eq!(b.role_in("other"), None);
        assert_eq!(actor(true, &[]).role_in("anything"), Some(Role::Admin));
    }

    #[test]
    fn who_manages_whom() {
        let ns_admin = actor(false, &[("shop", Role::Admin), ("blog", Role::Member)]);
        let g = |n: &str| Grant { namespace: n.into(), role: Role::Member };
        assert!(ns_admin.can_manage(false, &[g("shop")]));
        assert!(!ns_admin.can_manage(false, &[g("shop"), g("blog")]), "blog is not administered by them");
        assert!(!ns_admin.can_manage(false, &[g("*")]), "wildcard needs a hub admin");
        assert!(!ns_admin.can_manage(true, &[g("shop")]), "hub admins are out of reach");
        assert!(!ns_admin.can_manage(false, &[]), "principals without grants belong to hub admins");
        assert!(actor(true, &[]).can_manage(true, &[g("*")]));
        assert!(ns_admin.can_grant("shop") && !ns_admin.can_grant("blog") && !ns_admin.can_grant("*"));
    }

    #[test]
    fn grant_parsing() {
        assert_eq!(parse_grant("shop:lead").unwrap(), Grant { namespace: "shop".into(), role: Role::Lead });
        assert_eq!(parse_grant("*:viewer").unwrap(), Grant { namespace: "*".into(), role: Role::Viewer });
        assert_eq!(parse_grant("shop").unwrap().role, Role::Member);
        assert!(parse_grant("shop:owner").is_err());
        assert!(parse_grant("Bad NS:member").is_err());
    }
}

//! REST API for the web UI and automation: `/api/v1/...` plus `/ui/login` and `/ui/logout`.
//!
//! Authentication: `Authorization: Bearer cnc_...` or the UI session cookie. Requests that change
//! something through the cookie must carry `X-Concord-UI: 1` (a custom header a cross-site form
//! cannot send), which together with `SameSite=Strict` closes CSRF.

pub mod connectors;

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::access::{self, Actor, Grant};
use crate::clock::parse_duration;
use crate::error::HubError;
use crate::http::{Gate, SESSION_COOKIE, bearer, json_error, json_response, session_cookie, status_of};
use crate::model::{NewTask, Role, TaskKind, TaskStatus};
use crate::ops::{self, admin, import, negotiate::NegotiationFilter, tasks::TaskFilter};
use crate::token;

pub struct ApiError(pub HubError);

impl From<HubError> for ApiError {
    fn from(e: HubError) -> Self {
        ApiError(e)
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError(HubError::Db(e))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        json_response(status_of(&self.0), self.0.to_json())
    }
}

pub type ApiResult = Result<Json<Value>, ApiError>;

fn forbidden(msg: &str) -> ApiError {
    ApiError(HubError::forbidden(msg))
}

/// The authenticated principal of an API request.
pub struct Auth(pub Actor);

impl FromRequestParts<Arc<Gate>> for Auth {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, gate: &Arc<Gate>) -> Result<Self, Self::Rejection> {
        let found = if let Some(raw) = bearer(&parts.headers) {
            gate.actor_by_token(&raw).await
        } else if let Some(sid) = session_cookie(&parts.headers) {
            let safe = matches!(parts.method, Method::GET | Method::HEAD);
            let marked = parts.headers.get("x-concord-ui").is_some_and(|v| v == "1");
            if !safe && !marked {
                return Err(json_error(StatusCode::FORBIDDEN, "csrf", "missing X-Concord-UI header"));
            }
            gate.actor_by_session(&sid).await
        } else {
            return Err(json_error(StatusCode::UNAUTHORIZED, "unauthorized", "sign in or send a bearer token"));
        };
        let actor = match found {
            Ok(Some(a)) => a,
            Ok(None) => {
                return Err(json_error(StatusCode::UNAUTHORIZED, "unauthorized", "invalid or expired credentials"));
            }
            Err(e) => return Err(ApiError(e).into_response()),
        };
        if !gate.allow(&actor.principal) {
            return Err(json_error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "slow down"));
        }
        Ok(Auth(actor))
    }
}

fn need_read(a: &Actor, ns: &str) -> Result<(), ApiError> {
    if a.can_read(ns) {
        Ok(())
    } else {
        Err(ApiError(HubError::not_found(format!("namespace {ns} not found or not accessible"))))
    }
}

fn need_admin(a: &Actor, ns: &str) -> Result<(), ApiError> {
    need_read(a, ns)?;
    if a.can_admin(ns) { Ok(()) } else { Err(forbidden("only admins of this namespace can do that")) }
}

pub fn router() -> Router<Arc<Gate>> {
    Router::new()
        .route("/ui/login", post(login))
        .route("/ui/logout", post(logout))
        .route("/api/v1/me", get(me))
        .route("/api/v1/namespaces", get(list_namespaces).post(create_namespace))
        .route("/api/v1/namespaces/{ns}", get(get_namespace).patch(patch_namespace))
        .route("/api/v1/namespaces/{ns}/plan.md", get(plan_md))
        .route("/api/v1/namespaces/{ns}/tasks", get(list_tasks).post(create_task))
        .route("/api/v1/namespaces/{ns}/tasks/import", post(import_tasks))
        .route("/api/v1/namespaces/{ns}/negotiations", get(list_negotiations))
        .route("/api/v1/namespaces/{ns}/negotiations/{id}", get(get_negotiation))
        .route("/api/v1/namespaces/{ns}/negotiations/{id}/resolve", post(resolve_negotiation))
        .route("/api/v1/namespaces/{ns}/events", get(events))
        .route("/api/v1/namespaces/{ns}/members", get(list_members).post(add_member))
        .route("/api/v1/namespaces/{ns}/members/{id}", put(set_member_role).delete(remove_member))
        .route("/api/v1/principals", get(list_principals).post(create_principal))
        .route("/api/v1/principals/{id}", get(get_principal).patch(patch_principal))
        .route("/api/v1/principals/{id}/grants/{ns}", put(put_grant).delete(delete_grant))
        .route("/api/v1/principals/{id}/tokens", post(issue_token))
        .route("/api/v1/tokens/{id}", delete(revoke_token))
        .merge(connectors::router())
}

// ---------- session ----------

#[derive(Deserialize)]
struct LoginBody {
    token: String,
}

fn cookie(gate: &Gate, value: &str, max_age: i64) -> HeaderValue {
    let secure = if gate.cfg.secure_cookies { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

async fn login(State(gate): State<Arc<Gate>>, Json(body): Json<LoginBody>) -> Response {
    let raw = body.token.trim().to_string();
    if !raw.starts_with(token::TOKEN_PREFIX) || !gate.allow("ui-login") {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized", "this is not a valid token");
    }
    let actor = match gate.actor_by_token(&raw).await {
        Ok(Some(a)) => a,
        Ok(None) => return json_error(StatusCode::UNAUTHORIZED, "unauthorized", "this is not a valid token"),
        Err(e) => return ApiError(e).into_response(),
    };
    let sid = token::generate();
    let now = gate.hub.now();
    let stored =
        sqlx::query("INSERT INTO ui_sessions (id_hash, principal_id, created_at, expires_at) VALUES ($1, $2, $3, $4)")
            .bind(token::hash(&sid))
            .bind(&actor.principal)
            .bind(now)
            .bind(now + time::Duration::hours(12))
            .execute(&gate.hub.pool)
            .await;
    if let Err(e) = stored {
        return ApiError(HubError::Db(e)).into_response();
    }
    let mut r = json_response(StatusCode::OK, json!({ "principal": actor.principal }));
    r.headers_mut().insert(header::SET_COOKIE, cookie(&gate, &sid, 12 * 3600));
    r
}

async fn logout(State(gate): State<Arc<Gate>>, headers: axum::http::HeaderMap) -> Response {
    if let Some(sid) = session_cookie(&headers) {
        let _ = sqlx::query("DELETE FROM ui_sessions WHERE id_hash = $1")
            .bind(token::hash(&sid))
            .execute(&gate.hub.pool)
            .await;
        gate.forget();
    }
    let mut r = json_response(StatusCode::OK, json!({ "signed_out": true }));
    r.headers_mut().insert(header::SET_COOKIE, cookie(&gate, "", 0));
    r
}

async fn me(State(gate): State<Arc<Gate>>, Auth(a): Auth) -> ApiResult {
    let namespaces = access::namespaces_of(&gate.hub.pool, &a).await?;
    Ok(Json(json!({
        "principal": a.principal,
        "display_name": a.display_name,
        "human": a.human,
        "is_admin": a.is_admin,
        "grants": a.grants,
        "namespaces": namespaces,
        "public_url": gate.hub.public_url(),
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

// ---------- namespaces ----------

async fn list_namespaces(State(gate): State<Arc<Gate>>, Auth(a): Auth) -> ApiResult {
    let accessible = access::namespaces_of(&gate.hub.pool, &a).await?;
    let ids: Vec<String> = accessible.iter().map(|n| n.id.clone()).collect();
    let stats: Vec<(String, String, i64, i64, i64, Option<time::OffsetDateTime>)> = sqlx::query_as(
        "SELECT n.id, n.goal,
                (SELECT COUNT(*) FROM members m WHERE m.namespace_id = n.id AND m.active),
                (SELECT COUNT(*) FROM tasks t WHERE t.namespace_id = n.id AND t.status NOT IN ('done','cancelled','rejected')),
                (SELECT COUNT(*) FROM negotiations g WHERE g.namespace_id = n.id AND g.status <> 'closed'),
                (SELECT MAX(e.at) FROM events e WHERE e.namespace_id = n.id)
         FROM namespaces n WHERE n.id = ANY($1) ORDER BY n.id",
    )
    .bind(&ids)
    .fetch_all(&gate.hub.pool)
    .await?;
    let list: Vec<Value> = accessible
        .iter()
        .map(|n| {
            let s = stats.iter().find(|s| s.0 == n.id);
            json!({
                "id": n.id,
                "name": n.name,
                "role": n.role,
                "goal": s.map(|s| s.1.clone()).unwrap_or_default(),
                "members": s.map(|s| s.2).unwrap_or(0),
                "open_tasks": s.map(|s| s.3).unwrap_or(0),
                "open_negotiations": s.map(|s| s.4).unwrap_or(0),
                "last_activity_at": s.and_then(|s| s.5).map(crate::clock::rfc3339),
                "mcp_url": format!("{}/mcp/{}", gate.hub.public_url(), n.id),
            })
        })
        .collect();
    Ok(Json(json!({ "namespaces": list, "can_create": a.is_admin })))
}

#[derive(Deserialize)]
struct NewNamespace {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    max_rounds: Option<i32>,
    #[serde(default)]
    turn_timeout: Option<String>,
    #[serde(default)]
    escalation_timeout: Option<String>,
    #[serde(default)]
    lease: Option<String>,
}

fn dur(v: &Option<String>, default: time::Duration) -> Result<time::Duration, ApiError> {
    match v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => parse_duration(s).map_err(|e| ApiError(HubError::invalid(e))),
        None => Ok(default),
    }
}

async fn create_namespace(State(gate): State<Arc<Gate>>, Auth(a): Auth, Json(b): Json<NewNamespace>) -> ApiResult {
    if !a.is_admin {
        return Err(forbidden("only hub admins create namespaces"));
    }
    let d = admin::NamespaceSettings::default();
    let s = admin::NamespaceSettings {
        max_rounds: b.max_rounds.unwrap_or(d.max_rounds),
        turn_timeout: dur(&b.turn_timeout, d.turn_timeout)?,
        escalation_timeout: dur(&b.escalation_timeout, d.escalation_timeout)?,
        lease: dur(&b.lease, d.lease)?,
    };
    let name = if b.name.trim().is_empty() { b.id.clone() } else { b.name.clone() };
    admin::create_namespace(&gate.hub, &b.id, &name, &b.goal, &s, &a.principal).await?;
    gate.forget();
    Ok(Json(json!({ "created": b.id, "mcp_url": format!("{}/mcp/{}", gate.hub.public_url(), b.id) })))
}

async fn get_namespace(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path(ns): Path<String>) -> ApiResult {
    need_read(&a, &ns)?;
    let mut conn = gate.hub.pool.acquire().await?;
    let p = ops::plan::load(&mut conn, &ns, false).await?;
    let mut v = ops::plan::to_json(&p);
    v["your_role"] = json!(a.role_in(&ns));
    v["mcp_url"] = json!(format!("{}/mcp/{}", gate.hub.public_url(), ns));
    Ok(Json(v))
}

#[derive(Deserialize)]
struct PatchNamespace {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    max_rounds: Option<i32>,
    #[serde(default)]
    turn_timeout: Option<String>,
    #[serde(default)]
    escalation_timeout: Option<String>,
    #[serde(default)]
    lease: Option<String>,
}

async fn patch_namespace(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(ns): Path<String>,
    Json(b): Json<PatchNamespace>,
) -> ApiResult {
    need_admin(&a, &ns)?;
    let secs = |v: &Option<String>| -> Result<Option<i64>, ApiError> {
        match v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Ok(Some(parse_duration(s).map_err(|e| ApiError(HubError::invalid(e)))?.whole_seconds())),
            None => Ok(None),
        }
    };
    let (turn, esc, lease) = (secs(&b.turn_timeout)?, secs(&b.escalation_timeout)?, secs(&b.lease)?);
    if let Some(r) = b.max_rounds
        && !(1..=20).contains(&r)
    {
        return Err(ApiError(HubError::invalid("max_rounds must be between 1 and 20")));
    }
    let by = a.principal.clone();
    let v = gate
        .hub
        .write(&ns, &by, move |op| {
            Box::pin(async move {
                if let Some(name) = &b.name {
                    let name = ops::non_empty("name", name, ops::MAX_TITLE)?;
                    sqlx::query("UPDATE namespaces SET name = $2 WHERE id = $1").bind(&op.ns.id).bind(&name).execute(&mut *op.tx).await?;
                    op.ns.name = name;
                }
                sqlx::query(
                    "UPDATE namespaces SET max_rounds = COALESCE($2, max_rounds), turn_timeout_secs = COALESCE($3, turn_timeout_secs),
                            escalation_timeout_secs = COALESCE($4, escalation_timeout_secs), lease_secs = COALESCE($5, lease_secs)
                     WHERE id = $1",
                )
                .bind(&op.ns.id)
                .bind(b.max_rounds)
                .bind(turn)
                .bind(esc)
                .bind(lease)
                .execute(&mut *op.tx)
                .await?;
                if let Some(goal) = &b.goal {
                    ops::check_len("goal", goal, ops::MAX_TEXT)?;
                    op.ns.goal = goal.trim().to_string();
                    op.bump_plan();
                    op.emit("plan.goal_set", Some("plan:goal"), None, "plan goal replaced by a namespace admin", json!({ "goal": op.ns.goal }))
                        .await?;
                }
                op.emit("namespace.updated", None, None, format!("namespace settings changed by {}", op.actor), json!({})).await?;
                Ok(json!({ "updated": op.ns.id }))
            })
        })
        .await?;
    Ok(Json(v))
}

async fn plan_md(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path(ns): Path<String>) -> Result<Response, ApiError> {
    need_read(&a, &ns)?;
    let mut conn = gate.hub.pool.acquire().await?;
    let p = ops::plan::load(&mut conn, &ns, false).await?;
    let mut r = ops::plan::to_markdown(&p).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/markdown; charset=utf-8"));
    Ok(r)
}

// ---------- tasks ----------

#[derive(Deserialize, Default)]
struct TaskQuery {
    /// Comma-separated statuses.
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    area: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    milestone: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    include_closed: bool,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

async fn list_tasks(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(ns): Path<String>,
    Query(q): Query<TaskQuery>,
) -> ApiResult {
    need_read(&a, &ns)?;
    let status =
        q.status.as_deref().map(|s| s.split(',').filter_map(|x| TaskStatus::parse(x.trim())).collect::<Vec<_>>());
    let f = TaskFilter {
        status,
        area: q.area,
        assignee: q.assignee,
        milestone: q.milestone,
        kind: q.kind.as_deref().and_then(TaskKind::parse),
        mine: false,
        include_closed: q.include_closed,
        limit: q.limit,
        offset: q.offset,
    };
    let mut conn = gate.hub.pool.acquire().await?;
    Ok(Json(ops::tasks::list(&mut conn, &ns, &a.principal, &f).await?))
}

#[derive(Deserialize)]
struct CreateBody {
    #[serde(flatten)]
    task: NewTask,
    #[serde(default)]
    rationale: String,
}

async fn create_task(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(ns): Path<String>,
    Json(b): Json<CreateBody>,
) -> ApiResult {
    need_read(&a, &ns)?;
    let caller = gate.enter(&a, &ns).await?;
    let v = gate
        .hub
        .write(&ns, &caller.member.clone(), move |op| {
            Box::pin(async move {
                ops::tasks::create_many(op, &caller.member, caller.role, vec![b.task], &b.rationale).await
            })
        })
        .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct ImportBody {
    #[serde(default)]
    format: import::Format,
    text: String,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    rationale: String,
}

async fn import_tasks(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(ns): Path<String>,
    Json(b): Json<ImportBody>,
) -> ApiResult {
    need_read(&a, &ns)?;
    let parsed = import::parse(b.format, &b.text)?;
    if b.dry_run {
        return Ok(Json(json!({ "dry_run": true, "tasks": parsed.tasks, "warnings": parsed.warnings })));
    }
    let caller = gate.enter(&a, &ns).await?;
    let warnings = parsed.warnings.clone();
    let mut v = gate
        .hub
        .write(&ns, &caller.member.clone(), move |op| {
            Box::pin(async move {
                ops::tasks::create_many(op, &caller.member, caller.role, parsed.tasks, &b.rationale).await
            })
        })
        .await?;
    v["warnings"] = json!(warnings);
    Ok(Json(v))
}

// ---------- negotiations, events ----------

#[derive(Deserialize, Default)]
struct NegQuery {
    #[serde(default)]
    status: NegotiationFilter,
    #[serde(default)]
    limit: Option<i64>,
}

async fn list_negotiations(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(ns): Path<String>,
    Query(q): Query<NegQuery>,
) -> ApiResult {
    need_read(&a, &ns)?;
    let mut conn = gate.hub.pool.acquire().await?;
    Ok(Json(ops::negotiate::list(&mut conn, &ns, &a.principal, None, q.status, false, q.limit.unwrap_or(50)).await?))
}

async fn get_negotiation(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path((ns, id)): Path<(String, String)>,
) -> ApiResult {
    need_read(&a, &ns)?;
    let mut conn = gate.hub.pool.acquire().await?;
    Ok(Json(ops::negotiate::list(&mut conn, &ns, &a.principal, Some(&id), NegotiationFilter::All, false, 1).await?))
}

#[derive(Deserialize)]
struct ResolveBody {
    #[serde(default)]
    round: Option<i32>,
    #[serde(default)]
    note: String,
}

async fn resolve_negotiation(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path((ns, id)): Path<(String, String)>,
    Json(b): Json<ResolveBody>,
) -> ApiResult {
    need_admin(&a, &ns)?;
    Ok(Json(admin::resolve(&gate.hub, &ns, &id, b.round, &b.note, &a.principal).await?))
}

#[derive(Deserialize, Default)]
struct EventQuery {
    #[serde(default)]
    before: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn events(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(ns): Path<String>,
    Query(q): Query<EventQuery>,
) -> ApiResult {
    need_read(&a, &ns)?;
    let rows: Vec<crate::model::Event> = sqlx::query_as(
        "SELECT seq, at, actor, kind, entity, summary, data FROM events
         WHERE namespace_id = $1 AND (audience IS NULL OR audience ? $2) AND ($3::bigint IS NULL OR seq < $3)
         ORDER BY seq DESC LIMIT $4",
    )
    .bind(&ns)
    .bind(&a.principal)
    .bind(q.before)
    .bind(q.limit.unwrap_or(50).clamp(1, 200))
    .fetch_all(&gate.hub.pool)
    .await?;
    Ok(Json(json!({ "events": rows })))
}

// ---------- members of a namespace ----------

async fn list_members(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path(ns): Path<String>) -> ApiResult {
    need_read(&a, &ns)?;
    let mut conn = gate.hub.pool.acquire().await?;
    let p = ops::plan::load(&mut conn, &ns, false).await?;
    let wildcard: Vec<(String, String)> =
        sqlx::query_as("SELECT principal_id, role FROM grants WHERE namespace = '*' ORDER BY principal_id")
            .fetch_all(&gate.hub.pool)
            .await?;
    Ok(Json(json!({
        "members": ops::plan::team(&p),
        "areas": p.areas,
        "all_namespace_access": wildcard.into_iter().map(|(p, r)| json!({ "principal": p, "role": r })).collect::<Vec<_>>(),
        "can_manage": a.can_admin(&ns),
    })))
}

#[derive(Deserialize)]
struct AreaSpec {
    name: String,
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Deserialize)]
struct AddMemberBody {
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    human: String,
    #[serde(default)]
    role: Option<Role>,
    #[serde(default)]
    areas: Vec<AreaSpec>,
}

async fn add_member(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(ns): Path<String>,
    Json(b): Json<AddMemberBody>,
) -> ApiResult {
    need_admin(&a, &ns)?;
    let token = admin::add_member(
        &gate.hub,
        &ns,
        admin::NewMember {
            id: b.id.clone(),
            display_name: b.display_name,
            human: b.human,
            lead: false,
            role: Some(b.role.unwrap_or(Role::Member)),
            areas: b.areas.into_iter().map(|x| (x.name, x.paths)).collect(),
        },
    )
    .await?;
    gate.forget();
    let mut v = json!({ "added": b.id, "namespace": ns, "new_principal": token.is_some() });
    if let Some(t) = token {
        v["token"] = json!(t);
        v["connections"] = access::connections(&gate.hub, &b.id, &t).await?;
    }
    Ok(Json(v))
}

#[derive(Deserialize)]
struct RoleBody {
    role: Role,
}

async fn set_member_role(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path((ns, id)): Path<(String, String)>,
    Json(b): Json<RoleBody>,
) -> ApiResult {
    need_admin(&a, &ns)?;
    access::set_grant(&gate.hub, &id, &ns, b.role).await?;
    gate.forget();
    Ok(Json(json!({ "principal": id, "namespace": ns, "role": b.role })))
}

async fn remove_member(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path((ns, id)): Path<(String, String)>,
) -> ApiResult {
    need_admin(&a, &ns)?;
    let v = admin::remove_member(&gate.hub, &ns, &id).await?;
    gate.forget();
    Ok(Json(v))
}

// ---------- principals, grants, tokens ----------

async fn list_principals(State(gate): State<Arc<Gate>>, Auth(a): Auth) -> ApiResult {
    let all = access::principals(&gate.hub.pool).await?;
    let visible: Vec<_> =
        all.into_iter().filter(|p| p.id == a.principal || a.can_manage(p.is_admin, &p.grants)).collect();
    let manageable: Vec<String> = access::namespaces_of(&gate.hub.pool, &a)
        .await?
        .into_iter()
        .filter(|n| n.role.manages())
        .map(|n| n.id)
        .collect();
    Ok(Json(
        json!({ "principals": visible, "is_admin": a.is_admin, "you": a.principal, "namespaces_you_manage": manageable }),
    ))
}

#[derive(Deserialize)]
struct GrantSpec {
    namespace: String,
    role: Role,
}

#[derive(Deserialize)]
struct NewPrincipalBody {
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    human: String,
    #[serde(default)]
    is_admin: bool,
    #[serde(default)]
    grants: Vec<GrantSpec>,
    #[serde(default = "yes")]
    issue_token: bool,
    #[serde(default)]
    label: String,
}

fn yes() -> bool {
    true
}

async fn create_principal(State(gate): State<Arc<Gate>>, Auth(a): Auth, Json(b): Json<NewPrincipalBody>) -> ApiResult {
    if b.is_admin && !a.is_admin {
        return Err(forbidden("only hub admins create hub admins"));
    }
    if b.grants.is_empty() && !a.is_admin {
        return Err(forbidden("give access to at least one namespace you manage"));
    }
    for g in &b.grants {
        if !a.can_grant(&g.namespace) {
            return Err(forbidden(&format!("you cannot give access to {}", g.namespace)));
        }
    }
    let grants: Vec<Grant> = b.grants.iter().map(|g| Grant { namespace: g.namespace.clone(), role: g.role }).collect();
    access::create_principal(
        &gate.hub,
        &access::NewPrincipal {
            id: b.id.clone(),
            display_name: b.display_name,
            human: b.human,
            is_admin: b.is_admin,
            grants,
        },
    )
    .await?;
    let mut v = json!({ "principal": access::principal_view(&gate.hub.pool, &b.id).await? });
    if b.issue_token {
        let label = if b.label.trim().is_empty() { "first token".to_string() } else { b.label };
        let (id, raw) = access::issue_token(&gate.hub, &b.id, &label, &a.principal).await?;
        v["token_id"] = json!(id);
        v["token"] = json!(raw);
        v["connections"] = access::connections(&gate.hub, &b.id, &raw).await?;
    }
    Ok(Json(v))
}

async fn target(gate: &Gate, a: &Actor, id: &str) -> Result<access::PrincipalView, ApiError> {
    let p = access::principal_view(&gate.hub.pool, id).await?;
    if p.id != a.principal && !a.can_manage(p.is_admin, &p.grants) {
        return Err(ApiError(HubError::not_found(format!("principal {id} not found"))));
    }
    Ok(p)
}

async fn get_principal(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!({ "principal": target(&gate, &a, &id).await? })))
}

#[derive(Deserialize)]
struct PatchPrincipal {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    human: Option<String>,
    #[serde(default)]
    is_admin: Option<bool>,
    #[serde(default)]
    active: Option<bool>,
}

async fn patch_principal(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(id): Path<String>,
    Json(b): Json<PatchPrincipal>,
) -> ApiResult {
    let p = target(&gate, &a, &id).await?;
    let managing = p.id != a.principal || a.is_admin;
    if (b.is_admin.is_some() || b.active.is_some()) && !managing {
        return Err(forbidden("you cannot change your own access"));
    }
    if let Some(name) = &b.display_name {
        let name = ops::non_empty("name", name, ops::MAX_TITLE)?;
        sqlx::query("UPDATE principals SET display_name = $2 WHERE id = $1")
            .bind(&id)
            .bind(name)
            .execute(&gate.hub.pool)
            .await?;
    }
    if let Some(human) = &b.human {
        ops::check_len("human", human, ops::MAX_TITLE)?;
        sqlx::query("UPDATE principals SET human = $2 WHERE id = $1")
            .bind(&id)
            .bind(human.trim())
            .execute(&gate.hub.pool)
            .await?;
    }
    if let Some(admin_flag) = b.is_admin {
        if !a.is_admin {
            return Err(forbidden("only hub admins change admin rights"));
        }
        access::set_admin(&gate.hub, &id, admin_flag).await?;
    }
    if b.active == Some(false) {
        if id == a.principal {
            return Err(forbidden("you cannot deactivate yourself"));
        }
        access::deactivate(&gate.hub, &id).await?;
    }
    gate.forget();
    Ok(Json(json!({ "principal": access::principal_view(&gate.hub.pool, &id).await? })))
}

async fn put_grant(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path((id, ns)): Path<(String, String)>,
    Json(b): Json<RoleBody>,
) -> ApiResult {
    if !a.can_grant(&ns) {
        return Err(forbidden(&format!("you cannot give access to {ns}")));
    }
    access::set_grant(&gate.hub, &id, &ns, b.role).await?;
    gate.forget();
    Ok(Json(json!({ "principal": access::principal_view(&gate.hub.pool, &id).await? })))
}

async fn delete_grant(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path((id, ns)): Path<(String, String)>,
) -> ApiResult {
    if !a.can_grant(&ns) {
        return Err(forbidden(&format!("you cannot change access to {ns}")));
    }
    access::remove_grant(&gate.hub, &id, &ns).await?;
    gate.forget();
    Ok(Json(json!({ "principal": access::principal_view(&gate.hub.pool, &id).await? })))
}

#[derive(Deserialize, Default)]
struct TokenBody {
    #[serde(default)]
    label: String,
}

async fn issue_token(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(id): Path<String>,
    body: Option<Json<TokenBody>>,
) -> ApiResult {
    target(&gate, &a, &id).await?;
    let label = body.map(|Json(b)| b.label).unwrap_or_default();
    let (token_id, raw) = access::issue_token(&gate.hub, &id, &label, &a.principal).await?;
    Ok(Json(json!({
        "token_id": token_id,
        "token": raw,
        "connections": access::connections(&gate.hub, &id, &raw).await?,
    })))
}

async fn revoke_token(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path(id): Path<String>) -> ApiResult {
    let owner = access::token_owner(&gate.hub.pool, &id).await?;
    target(&gate, &a, &owner).await?;
    access::revoke_token(&gate.hub, &id).await?;
    sqlx::query("DELETE FROM mcp_sessions WHERE member_id = $1").bind(&owner).execute(&gate.hub.pool).await?;
    gate.forget();
    Ok(Json(json!({ "revoked": id })))
}

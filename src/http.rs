//! HTTP surface: MCP (`/mcp`, `/mcp/{namespace}`), REST API (`/api/v1`), web UI (`/`), `/healthz`,
//! plus the shared authentication gate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::access::{self, Actor};
use crate::error::HubError;
use crate::hub::Hub;
use crate::mcp::ConcordServer;
use crate::model::Caller;
use crate::session_store::PgSessionStore;
use crate::token;

#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Values accepted in the `Host` header (`host` or `host:port`). Loopback if empty.
    pub allowed_hosts: Vec<String>,
    /// Browser origins allowed to call MCP. Empty: MCP requests carrying an Origin are refused.
    pub allowed_origins: Vec<String>,
    /// Sustained requests per second per principal, and burst size.
    pub rate_per_sec: f64,
    pub burst: f64,
    pub max_body_bytes: usize,
    /// Mark the UI session cookie `Secure` (the public URL is https).
    pub secure_cookies: bool,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig {
            allowed_hosts: vec![],
            allowed_origins: vec![],
            rate_per_sec: 10.0,
            burst: 60.0,
            max_body_bytes: 256 * 1024,
            secure_cookies: false,
        }
    }
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

pub const SESSION_COOKIE: &str = "concord_session";
const CACHE_TTL: Duration = Duration::from_secs(15);

/// Authentication state shared by MCP, REST and UI routes.
pub struct Gate {
    pub hub: Arc<Hub>,
    pub sessions: PgSessionStore,
    pub cfg: HttpConfig,
    actors: Mutex<HashMap<String, (Option<Actor>, Instant)>>,
    members: Mutex<HashMap<(String, String), (Caller, Instant)>>,
    session_owner: Mutex<HashMap<String, (String, String)>>,
    buckets: Mutex<HashMap<String, Bucket>>,
}

fn cached<K: std::hash::Hash + Eq, V: Clone>(map: &Mutex<HashMap<K, (V, Instant)>>, key: &K) -> Option<V> {
    let map = map.lock().expect("cache");
    map.get(key).filter(|(_, at)| at.elapsed() < CACHE_TTL).map(|(v, _)| v.clone())
}

fn remember<K: std::hash::Hash + Eq, V>(map: &Mutex<HashMap<K, (V, Instant)>>, key: K, value: V) {
    let mut map = map.lock().expect("cache");
    if map.len() > 50_000 {
        map.clear();
    }
    map.insert(key, (value, Instant::now()));
}

impl Gate {
    pub fn new(hub: Arc<Hub>, sessions: PgSessionStore, cfg: HttpConfig) -> Gate {
        Gate {
            hub,
            sessions,
            cfg,
            actors: Mutex::new(HashMap::new()),
            members: Mutex::new(HashMap::new()),
            session_owner: Mutex::new(HashMap::new()),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Drops cached identities on this instance (call after access changes). Other instances
    /// pick changes up within the cache TTL.
    pub fn forget(&self) {
        self.actors.lock().expect("cache").clear();
        self.members.lock().expect("cache").clear();
    }

    pub async fn actor_by_token(&self, raw: &str) -> Result<Option<Actor>, HubError> {
        let key = format!("t:{}", token::hash(raw));
        if let Some(a) = cached(&self.actors, &key) {
            return Ok(a);
        }
        let actor = access::actor_by_token(&self.hub, raw).await?;
        remember(&self.actors, key, actor.clone());
        Ok(actor)
    }

    pub async fn actor_by_session(&self, sid: &str) -> Result<Option<Actor>, HubError> {
        let hash = token::hash(sid);
        let key = format!("s:{hash}");
        if let Some(a) = cached(&self.actors, &key) {
            return Ok(a);
        }
        let now = self.hub.now();
        let row: Option<(String,)> = sqlx::query_as(
            "UPDATE ui_sessions SET expires_at = GREATEST(expires_at, $2 + interval '12 hours')
             WHERE id_hash = $1 AND expires_at > $2 RETURNING principal_id",
        )
        .bind(&hash)
        .bind(now)
        .fetch_optional(&self.hub.pool)
        .await?;
        let actor = match row {
            Some((p,)) => access::actor_by_id(&self.hub.pool, &p).await?,
            None => None,
        };
        remember(&self.actors, key, actor.clone());
        Ok(actor)
    }

    /// The actor as a member of `ns` (membership is created on first access).
    pub async fn enter(&self, actor: &Actor, ns: &str) -> Result<Caller, HubError> {
        let key = (actor.principal.clone(), ns.to_string());
        if let Some(c) = cached(&self.members, &key) {
            return Ok(c);
        }
        let caller = access::enter(&self.hub, actor, ns).await?;
        remember(&self.members, key, caller.clone());
        Ok(caller)
    }

    pub fn allow(&self, key: &str) -> bool {
        let mut b = self.buckets.lock().expect("buckets");
        if b.len() > 50_000 {
            b.clear();
        }
        let now = Instant::now();
        let e = b.entry(key.to_string()).or_insert(Bucket { tokens: self.cfg.burst, at: now });
        e.tokens = (e.tokens + now.duration_since(e.at).as_secs_f64() * self.cfg.rate_per_sec).min(self.cfg.burst);
        e.at = now;
        if e.tokens >= 1.0 {
            e.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    async fn session_owner(&self, sid: &str) -> Result<Option<(String, String)>, sqlx::Error> {
        if let Some(o) = self.session_owner.lock().expect("session cache").get(sid) {
            return Ok(Some(o.clone()));
        }
        let owner = self.sessions.owner(sid).await?;
        if let Some(o) = &owner {
            let mut cache = self.session_owner.lock().expect("session cache");
            if cache.len() > 100_000 {
                cache.clear();
            }
            cache.insert(sid.to_string(), o.clone());
        }
        Ok(owner)
    }
}

pub fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::trim)
        .filter(|t| t.starts_with(token::TOKEN_PREFIX))
        .map(String::from)
}

pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == SESSION_COOKIE && !v.is_empty()).then(|| v.to_string())
    })
}

pub fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    json_response(status, serde_json::json!({ "error": { "code": code, "message": message } }))
}

pub fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    let mut r = (status, body.to_string()).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

pub fn status_of(e: &HubError) -> StatusCode {
    match e {
        HubError::NotFound(_) => StatusCode::NOT_FOUND,
        HubError::Conflict { .. } => StatusCode::CONFLICT,
        HubError::Forbidden(_) => StatusCode::FORBIDDEN,
        HubError::Invalid(_) => StatusCode::BAD_REQUEST,
        HubError::Unauthorized => StatusCode::UNAUTHORIZED,
        HubError::Db(_) | HubError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn unauthorized() -> Response {
    let mut r = json_error(StatusCode::UNAUTHORIZED, "unauthorized", "missing or invalid bearer token");
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"concord\""));
    r
}

fn unavailable(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "auth lookup failed");
    json_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "try again")
}

/// Namespace for `/mcp` without one: the only namespace the principal reaches explicitly.
#[allow(clippy::result_large_err)]
async fn default_namespace(gate: &Gate, actor: &Actor) -> Result<String, Response> {
    let explicit: Vec<&str> = actor.grants.iter().map(|g| g.namespace.as_str()).filter(|n| *n != access::ALL).collect();
    if !actor.reaches_all() && explicit.len() == 1 {
        return Ok(explicit[0].to_string());
    }
    let list = access::namespaces_of(&gate.hub.pool, actor).await.map_err(unavailable)?;
    let base = gate.hub.public_url();
    let options: Vec<serde_json::Value> =
        list.iter().map(|n| serde_json::json!({ "namespace": n.id, "url": format!("{base}/mcp/{}", n.id) })).collect();
    Err(json_response(
        StatusCode::BAD_REQUEST,
        serde_json::json!({ "error": {
            "code": "choose_namespace",
            "message": "this token reaches several namespaces: connect to /mcp/<namespace>",
            "namespaces": options,
        }}),
    ))
}

async fn mcp_guard(State(gate): State<Arc<Gate>>, mut req: Request, next: Next) -> Response {
    if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok())
        && !gate.cfg.allowed_origins.iter().any(|a| a == origin)
    {
        return json_error(StatusCode::FORBIDDEN, "forbidden", "browser origin not allowed");
    }
    let Some(raw) = bearer(req.headers()) else { return unauthorized() };
    let actor = match gate.actor_by_token(&raw).await {
        Ok(Some(a)) => a,
        Ok(None) => return unauthorized(),
        Err(e) => return unavailable(e),
    };
    if !gate.allow(&actor.principal) {
        let mut r = json_error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "slow down");
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return r;
    }
    let from_path = req.uri().path().strip_prefix("/mcp").unwrap_or("").trim_matches('/').to_string();
    let ns = if from_path.is_empty() {
        match default_namespace(&gate, &actor).await {
            Ok(ns) => ns,
            Err(r) => return r,
        }
    } else {
        from_path
    };
    let caller = match gate.enter(&actor, &ns).await {
        Ok(c) => c,
        Err(HubError::NotFound(m)) => return json_error(StatusCode::NOT_FOUND, "not_found", &m),
        Err(e) => return unavailable(e),
    };
    let sid = req.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()).map(String::from);
    if let Some(sid) = &sid {
        match gate.session_owner(sid).await {
            Ok(Some((ns, m))) if ns != caller.namespace || m != caller.member => {
                return json_error(StatusCode::NOT_FOUND, "not_found", "unknown session");
            }
            Ok(_) => {}
            Err(e) => return unavailable(e),
        }
    }
    req.extensions_mut().insert(caller.clone());
    req.extensions_mut().insert(actor);
    let resp = next.run(req).await;
    if let Some(new_sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok())
        && sid.as_deref() != Some(new_sid)
        && let Err(e) = gate.sessions.bind(new_sid, &caller.namespace, &caller.member).await
    {
        tracing::error!(error = %e, "binding session failed");
    }
    resp
}

async fn healthz(State(gate): State<Arc<Gate>>) -> Response {
    match sqlx::query("SELECT 1").execute(&gate.hub.pool).await {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response(),
    }
}

pub fn router(hub: Arc<Hub>, cfg: HttpConfig, cancel: CancellationToken) -> Router {
    let sessions = PgSessionStore::new(hub.pool.clone());
    let mut mcp = StreamableHttpServerConfig::default()
        .with_cancellation_token(cancel)
        .with_max_request_body_bytes(cfg.max_body_bytes)
        .with_sse_keep_alive(Some(Duration::from_secs(15)));
    if !cfg.allowed_hosts.is_empty() {
        mcp = mcp.with_allowed_hosts(cfg.allowed_hosts.clone());
    }
    mcp.session_store = Some(Arc::new(sessions.clone()));
    let factory_hub = hub.clone();
    let service = StreamableHttpService::new(
        move || Ok(ConcordServer::new(factory_hub.clone())),
        Arc::new(LocalSessionManager::default()),
        mcp,
    );
    let gate = Arc::new(Gate::new(hub, sessions, cfg));
    let mcp_routes = Router::new()
        .route_service("/mcp", service.clone())
        .route_service("/mcp/{namespace}", service)
        .route_layer(middleware::from_fn_with_state(gate.clone(), mcp_guard));
    Router::new()
        .merge(mcp_routes)
        .merge(crate::api::router())
        .merge(crate::ui::router())
        .route("/healthz", get(healthz))
        .with_state(gate)
}

//! Tracker connector endpoints. Reading needs access to the namespace; changing needs its admin.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{ApiError, ApiResult, Auth};
use crate::connectors::engine::{self, NewConnector, PatchConnector};
use crate::error::HubError;
use crate::http::Gate;

fn read(a: &crate::access::Actor, ns: &str) -> Result<(), ApiError> {
    if a.can_read(ns) {
        Ok(())
    } else {
        Err(ApiError(HubError::not_found(format!("namespace {ns} not found or not accessible"))))
    }
}

fn manage(a: &crate::access::Actor, ns: &str) -> Result<(), ApiError> {
    read(a, ns)?;
    if a.can_admin(ns) {
        Ok(())
    } else {
        Err(ApiError(HubError::forbidden("only admins of this namespace manage connectors")))
    }
}

pub fn router() -> Router<Arc<Gate>> {
    Router::new()
        .route("/api/v1/namespaces/{ns}/connectors", get(list).post(create))
        .route("/api/v1/namespaces/{ns}/connectors/{id}", get(get_one).patch(patch).delete(remove))
        .route("/api/v1/namespaces/{ns}/connectors/{id}/test", post(test))
        .route("/api/v1/namespaces/{ns}/connectors/{id}/sync", post(sync))
        .route("/api/v1/namespaces/{ns}/connectors/{id}/attach", post(attach))
}

async fn list(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path(ns): Path<String>) -> ApiResult {
    read(&a, &ns)?;
    Ok(Json(json!({ "connectors": engine::list(&gate.hub.pool, &ns).await?, "can_manage": a.can_admin(&ns) })))
}

async fn create(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path(ns): Path<String>,
    Json(b): Json<NewConnector>,
) -> ApiResult {
    manage(&a, &ns)?;
    Ok(Json(json!({ "connector": engine::create(&gate.hub, &ns, &a.principal, b).await? })))
}

async fn get_one(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path((ns, id)): Path<(String, String)>) -> ApiResult {
    read(&a, &ns)?;
    let row = engine::load(&gate.hub.pool, &ns, &id).await?;
    let mut v = json!({ "connector": row.view() });
    v["activity"] = engine::links(&gate.hub.pool, &ns, &id).await?;
    Ok(Json(v))
}

async fn patch(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path((ns, id)): Path<(String, String)>,
    Json(b): Json<PatchConnector>,
) -> ApiResult {
    manage(&a, &ns)?;
    Ok(Json(json!({ "connector": engine::update(&gate.hub, &ns, &id, b).await? })))
}

async fn remove(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path((ns, id)): Path<(String, String)>) -> ApiResult {
    manage(&a, &ns)?;
    engine::delete(&gate.hub, &ns, &id).await?;
    Ok(Json(json!({ "removed": id })))
}

async fn test(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path((ns, id)): Path<(String, String)>) -> ApiResult {
    manage(&a, &ns)?;
    Ok(Json(engine::test(&gate.hub, &ns, &id).await?))
}

async fn sync(State(gate): State<Arc<Gate>>, Auth(a): Auth, Path((ns, id)): Path<(String, String)>) -> ApiResult {
    manage(&a, &ns)?;
    Ok(Json(engine::sync_one(&gate.hub, &ns, &id, true).await?))
}

#[derive(Deserialize)]
struct AttachBody {
    reference: String,
}

async fn attach(
    State(gate): State<Arc<Gate>>,
    Auth(a): Auth,
    Path((ns, id)): Path<(String, String)>,
    Json(b): Json<AttachBody>,
) -> ApiResult {
    read(&a, &ns)?;
    let caller = gate.enter(&a, &ns).await?;
    if !caller.role.approves_plan() {
        return Err(ApiError(HubError::forbidden("leads and admins attach tracker issues")));
    }
    Ok(Json(engine::attach(&gate.hub, &ns, &id, &b.reference, &a.principal).await?))
}

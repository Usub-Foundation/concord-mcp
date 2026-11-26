//! Namespaces, grants, tokens, the REST API and the web UI session.

mod common;

use common::{Env, NS};
use concord_mcp::access::{self, Grant, NewPrincipal};
use concord_mcp::model::Role;
use concord_mcp::ops::admin;
use rmcp::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use sqlx::PgPool;

struct Api {
    http: reqwest::Client,
    base: String,
}

enum Cred<'a> {
    Bearer(&'a str),
    Cookie(&'a str, bool),
    Nothing,
}

impl Api {
    fn new(env: &Env) -> Api {
        Api { http: reqwest::Client::new(), base: format!("http://{}", env.addr) }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        cred: Cred<'_>,
        body: Option<Value>,
    ) -> (u16, Value, reqwest::header::HeaderMap) {
        let mut r = self.http.request(method.parse().unwrap(), format!("{}{path}", self.base));
        match cred {
            Cred::Bearer(t) => r = r.header("authorization", format!("Bearer {t}")),
            Cred::Cookie(c, ui) => {
                r = r.header("cookie", format!("concord_session={c}"));
                if ui {
                    r = r.header("x-concord-ui", "1");
                }
            }
            Cred::Nothing => {}
        }
        if let Some(b) = body {
            r = r.header("content-type", "application/json").body(b.to_string());
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)), headers)
    }
}

async fn principal(env: &Env, id: &str, is_admin: bool, grants: &[(&str, Role)]) -> String {
    access::create_principal(
        &env.hub,
        &NewPrincipal {
            id: id.into(),
            display_name: id.into(),
            human: id.into(),
            is_admin,
            grants: grants.iter().map(|(n, r)| Grant { namespace: n.to_string(), role: *r }).collect(),
        },
    )
    .await
    .unwrap();
    access::issue_token(&env.hub, id, "test", "test").await.unwrap().1
}

async fn mcp_status(env: &Env, path: &str, token: &str) -> u16 {
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } }
    });
    reqwest::Client::new()
        .post(format!("http://{}{path}", env.addr))
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(init.to_string())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[sqlx::test(migrations = "./migrations")]
async fn tokens_reach_only_granted_namespaces(pool: PgPool) {
    let env = Env::new(pool).await;
    admin::create_namespace(&env.hub, "secret", "Secret", "", &Default::default(), "operator").await.unwrap();
    let alice = env.tokens["alice"].clone();
    assert_eq!(mcp_status(&env, "/mcp/secret", &alice).await, 404);
    assert_eq!(mcp_status(&env, "/mcp/demo", &alice).await, 200);
    assert_eq!(mcp_status(&env, "/mcp", &alice).await, 200, "one namespace: /mcp picks it");
    let api = Api::new(&env);
    let (s, _, _) = api.call("GET", "/api/v1/namespaces/secret", Cred::Bearer(&alice), None).await;
    assert_eq!(s, 404);
    let (s, v, _) = api.call("GET", "/api/v1/namespaces", Cred::Bearer(&alice), None).await;
    assert_eq!(s, 200);
    let ids: Vec<&str> = v["namespaces"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![NS]);
    let (s, _, _) = api.call("GET", "/api/v1/namespaces", Cred::Nothing, None).await;
    assert_eq!(s, 401);

    // An all-namespaces token must pick one explicitly.
    let auditor = principal(&env, "auditor", false, &[("*", Role::Viewer)]).await;
    assert_eq!(mcp_status(&env, "/mcp", &auditor).await, 400);
    assert_eq!(mcp_status(&env, "/mcp/secret", &auditor).await, 200);
}

#[sqlx::test(migrations = "./migrations")]
async fn wildcard_viewers_join_lazily_and_stay_read_only(pool: PgPool) {
    let env = Env::new(pool).await;
    let auditor = principal(&env, "auditor", false, &[("*", Role::Viewer)]).await;
    admin::create_namespace(&env.hub, "late", "Created later", "", &Default::default(), "operator").await.unwrap();
    let cfg =
        StreamableHttpClientTransportConfig::with_uri(format!("http://{}/mcp/late", env.addr)).auth_header(auditor);
    let client = ().serve(StreamableHttpClientTransport::from_config(cfg)).await.unwrap();
    let agent = common::Agent { id: "auditor".into(), client };
    let who = agent.ok("whoami", json!({})).await;
    assert_eq!(who["your_role"], "viewer");
    let mine: Vec<&str> =
        who["your_namespaces"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap()).collect();
    assert_eq!(mine, vec!["demo", "late"]);
    let e = agent.err("create_task", json!({ "title": "should fail" })).await;
    assert_eq!(e["code"], "forbidden");
    agent.ok("get_plan", json!({})).await;

    // In demo the viewer is never asked to approve anything.
    let bob = env.agent("bob").await;
    let alice = env.agent("alice").await;
    alice.ok("update_plan", json!({ "ops": [{ "op": "set_goal", "goal": "v1" }] })).await;
    let v = bob.ok("update_plan", json!({ "ops": [{ "op": "set_goal", "goal": "v2" }] })).await;
    assert_eq!(v["proposal"]["negotiation"]["awaiting"], json!(["alice"]));
}

#[sqlx::test(migrations = "./migrations")]
async fn ui_session_csrf_and_revocation(pool: PgPool) {
    let env = Env::new(pool).await;
    let root = principal(&env, "root", true, &[]).await;
    let api = Api::new(&env);
    let (s, _, _) = api.call("POST", "/ui/login", Cred::Nothing, Some(json!({ "token": "cnc_nope" }))).await;
    assert_eq!(s, 401);
    let (s, _, h) = api.call("POST", "/ui/login", Cred::Nothing, Some(json!({ "token": root }))).await;
    assert_eq!(s, 200);
    let set = h.get("set-cookie").unwrap().to_str().unwrap().to_string();
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"), "{set}");
    let sid = set.split(';').next().unwrap().split_once('=').unwrap().1.to_string();

    let (s, me, _) = api.call("GET", "/api/v1/me", Cred::Cookie(&sid, false), None).await;
    assert_eq!(s, 200);
    assert_eq!(me["is_admin"], true);
    let body = json!({ "id": "shop", "name": "Shop", "goal": "Sell things" });
    let (s, v, _) = api.call("POST", "/api/v1/namespaces", Cred::Cookie(&sid, false), Some(body.clone())).await;
    assert_eq!(s, 403, "cookie without X-Concord-UI must be refused: {v}");
    assert_eq!(v["error"]["code"], "csrf");
    let (s, _, _) = api.call("POST", "/api/v1/namespaces", Cred::Cookie(&sid, true), Some(body)).await;
    assert_eq!(s, 200);

    // Issue a token for a new employee limited to shop, then revoke it.
    let (s, v, _) = api
        .call(
            "POST",
            "/api/v1/principals",
            Cred::Cookie(&sid, true),
            Some(json!({ "id": "carol", "grants": [{ "namespace": "shop", "role": "member" }] })),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let carol = v["token"].as_str().unwrap().to_string();
    let tok_id = v["token_id"].as_str().unwrap().to_string();
    assert_eq!(v["connections"]["connections"][0]["namespace"], "shop");
    assert_eq!(mcp_status(&env, "/mcp/shop", &carol).await, 200);
    assert_eq!(mcp_status(&env, "/mcp/demo", &carol).await, 404);
    let (s, _, _) = api.call("DELETE", &format!("/api/v1/tokens/{tok_id}"), Cred::Cookie(&sid, true), None).await;
    assert_eq!(s, 200);
    assert_eq!(mcp_status(&env, "/mcp/shop", &carol).await, 401);

    let (s, _, _) = api.call("POST", "/ui/logout", Cred::Cookie(&sid, true), None).await;
    assert_eq!(s, 200);
    let (s, _, _) = api.call("GET", "/api/v1/me", Cred::Cookie(&sid, false), None).await;
    assert_eq!(s, 401);
}

#[sqlx::test(migrations = "./migrations")]
async fn namespace_admins_manage_only_their_namespaces(pool: PgPool) {
    let env = Env::new(pool).await;
    admin::create_namespace(&env.hub, "other", "Other", "", &Default::default(), "operator").await.unwrap();
    let lead = principal(&env, "nsadmin", false, &[("demo", Role::Admin)]).await;
    let api = Api::new(&env);
    let b = |grants: Value| Some(json!({ "id": "dave", "grants": grants }));
    let (s, _, _) = api
        .call("POST", "/api/v1/principals", Cred::Bearer(&lead), b(json!([{ "namespace": "other", "role": "member" }])))
        .await;
    assert_eq!(s, 403);
    let (s, _, _) = api
        .call("POST", "/api/v1/principals", Cred::Bearer(&lead), b(json!([{ "namespace": "*", "role": "viewer" }])))
        .await;
    assert_eq!(s, 403);
    let (s, _, _) = api
        .call("POST", "/api/v1/principals", Cred::Bearer(&lead), b(json!([{ "namespace": "demo", "role": "member" }])))
        .await;
    assert_eq!(s, 200);
    let (s, _, _) = api.call("POST", "/api/v1/namespaces", Cred::Bearer(&lead), Some(json!({ "id": "mine" }))).await;
    assert_eq!(s, 403, "only hub admins create namespaces");
    let (s, _, _) = api
        .call("PUT", "/api/v1/namespaces/demo/members/bob", Cred::Bearer(&lead), Some(json!({ "role": "lead" })))
        .await;
    assert_eq!(s, 200);
    let (s, _, _) = api
        .call("PUT", "/api/v1/namespaces/other/members/bob", Cred::Bearer(&lead), Some(json!({ "role": "lead" })))
        .await;
    assert_eq!(s, 404);
    let (_, v, _) = api.call("GET", "/api/v1/principals", Cred::Bearer(&lead), None).await;
    let ids: Vec<&str> = v["principals"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"dave") && ids.contains(&"nsadmin") && ids.contains(&"bob"), "{ids:?}");
    // A member cannot manage anyone.
    let (s, _, _) = api
        .call(
            "PUT",
            "/api/v1/namespaces/demo/members/alice",
            Cred::Bearer(&env.tokens["bob"]),
            Some(json!({ "role": "viewer" })),
        )
        .await;
    assert_eq!(s, 403);
}

#[sqlx::test(migrations = "./migrations")]
async fn humans_and_agents_import_task_lists(pool: PgPool) {
    let env = Env::new(pool).await;
    let api = Api::new(&env);
    let backlog = "- [ ] Checkout page #p1 @frontend\n  Cart summary and pay button\n- [ ] Rate limit login #bug @backend\n- [x] Old done thing\n";
    let path = format!("/api/v1/namespaces/{NS}/tasks/import");
    let (s, v, _) = api
        .call("POST", &path, Cred::Bearer(&env.tokens["alice"]), Some(json!({ "text": backlog, "dry_run": true })))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(v["warnings"].as_array().unwrap().len(), 1);

    // alice is a lead: a human planner's tasks open directly, even in bob's area.
    let (s, v, _) = api.call("POST", &path, Cred::Bearer(&env.tokens["alice"]), Some(json!({ "text": backlog }))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["count"], 2);
    assert!(v["created"].as_array().unwrap().iter().all(|t| t["status"] == "open"));
    assert_eq!(v["proposals"], json!([]));

    // bob is a member: a task in alice's area becomes a proposal.
    let (s, v, _) = api
        .call(
            "POST",
            &format!("/api/v1/namespaces/{NS}/tasks"),
            Cred::Bearer(&env.tokens["bob"]),
            Some(json!({ "title": "Expose order totals", "area": "backend" })),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["created"][0]["status"], "proposed");
    assert_eq!(v["proposals"].as_array().unwrap().len(), 1);

    // An agent always follows the agent rules, even when its principal is a lead.
    let agent = env.agent("alice").await;
    let v = agent
        .ok("import_tasks", json!({ "text": "- Polish checkout spacing @frontend\n- Index orders table @backend", "rationale": "from the sprint notes" }))
        .await;
    assert_eq!(v["count"], 2);
    assert_eq!(v["proposals"].as_array().unwrap().len(), 1, "{v}");
}

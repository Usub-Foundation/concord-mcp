//! Tracker connectors against fake tracker servers (no real credentials needed).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use common::{Env, NS};
use concord_mcp::access::{self, NewPrincipal};
use concord_mcp::connectors::{self, Config, Kind, engine};
use serde_json::{Value, json};
use sqlx::PgPool;

#[derive(Debug, Clone)]
struct Rec {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Value,
}

type Responder = Arc<dyn Fn(&Rec) -> (u16, Value) + Send + Sync>;

#[derive(Clone)]
struct Fake {
    log: Arc<Mutex<Vec<Rec>>>,
    base: String,
}

impl Fake {
    async fn start(respond: Responder) -> Fake {
        let log: Arc<Mutex<Vec<Rec>>> = Arc::default();
        let l = log.clone();
        let app = Router::new().fallback(move |req: Request| {
            let (l, respond) = (l.clone(), respond.clone());
            async move {
                let (parts, body) = req.into_parts();
                let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap_or_default();
                let rec = Rec {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_string(),
                    query: parts.uri.query().unwrap_or("").to_string(),
                    headers: parts
                        .headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect(),
                    body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                };
                let (status, v) = respond(&rec);
                l.lock().unwrap().push(rec);
                let mut r: Response =
                    (axum::http::StatusCode::from_u16(status).unwrap(), Body::from(v.to_string())).into_response();
                r.headers_mut().insert("content-type", "application/json".parse().unwrap());
                r
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Fake { log, base }
    }

    fn calls(&self, method: &str, path_part: &str) -> Vec<Rec> {
        self.log.lock().unwrap().iter().filter(|r| r.method == method && r.path.contains(path_part)).cloned().collect()
    }
}

fn gh_issue(n: u64, title: &str, labels: &[&str], state: &str, updated: &str) -> Value {
    json!({
        "number": n, "title": title, "body": format!("Body of {title}"), "state": state,
        "labels": labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(),
        "html_url": format!("https://github.com/acme/shop/issues/{n}"), "updated_at": updated,
    })
}

struct GitHubFake {
    issues: Mutex<Vec<Value>>,
    fail_next_comment: Mutex<bool>,
}

#[sqlx::test(migrations = "./migrations")]
async fn github_end_to_end(pool: PgPool) {
    let env = Env::new(pool).await;
    let state = Arc::new(GitHubFake {
        issues: Mutex::new(vec![
            gh_issue(1, "Cart returns 500", &["agents", "bug", "backend"], "open", "2030-01-01T08:00:00Z"),
            gh_issue(2, "New landing copy", &["agents"], "open", "2030-01-01T08:10:00Z"),
            gh_issue(3, "Old closed thing", &["agents"], "closed", "2030-01-01T07:00:00Z"),
            gh_issue(5, "Untagged but important", &[], "open", "2030-01-01T08:20:00Z"),
            json!({ "number": 4, "title": "a PR", "pull_request": {}, "state": "open", "labels": [{ "name": "agents" }] }),
        ]),
        fail_next_comment: Mutex::new(true),
    });
    let s = state.clone();
    let fake = Fake::start(Arc::new(move |r: &Rec| {
        let issues = s.issues.lock().unwrap();
        let num = |p: &str| p.split('/').nth(5).and_then(|n| n.parse::<u64>().ok());
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/repos/acme/shop") => (200, json!({ "full_name": "acme/shop" })),
            ("GET", "/repos/acme/shop/issues") => {
                let open_only = r.query.contains("state=open");
                let want_label = r.query.contains("labels=agents");
                let list: Vec<Value> = issues
                    .iter()
                    .filter(|i| !open_only || i["state"] == "open")
                    .filter(|i| !want_label || i["labels"].as_array().unwrap().iter().any(|l| l["name"] == "agents"))
                    .cloned()
                    .collect();
                (200, json!(list))
            }
            ("GET", p) if p.starts_with("/repos/acme/shop/issues/") => {
                match issues.iter().find(|i| Some(i["number"].as_u64().unwrap()) == num(p)) {
                    Some(i) => (200, i.clone()),
                    None => (404, json!({ "message": "Not Found" })),
                }
            }
            ("POST", p) if p.ends_with("/comments") => {
                let mut fail = s.fail_next_comment.lock().unwrap();
                if *fail && r.body["body"].as_str().unwrap_or("").starts_with("Done") {
                    *fail = false;
                    (502, json!({ "message": "bad gateway" }))
                } else {
                    (201, json!({ "id": 1 }))
                }
            }
            ("PATCH", _) => (200, json!({ "state": "closed" })),
            _ => (404, json!({ "message": "unexpected" })),
        }
    }))
    .await;

    // A hub admin sets the connector up through the API.
    access::create_principal(
        &env.hub,
        &NewPrincipal {
            id: "root".into(),
            display_name: "root".into(),
            human: String::new(),
            is_admin: true,
            grants: vec![],
        },
    )
    .await
    .unwrap();
    let root = access::issue_token(&env.hub, "root", "t", "t").await.unwrap().1;
    let http = reqwest::Client::new();
    let api = |method: reqwest::Method, path: String, body: Option<Value>| {
        let mut r = http.request(method, format!("http://{}{path}", env.addr)).bearer_auth(&root);
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send()
    };
    let resp = api(
        reqwest::Method::POST,
        format!("/api/v1/namespaces/{NS}/connectors"),
        Some(json!({
            "kind": "github",
            "config": {
                "base_url": fake.base, "project": "acme/shop", "labels": ["agents"],
                "area_by_label": { "backend": "backend" }, "writeback": { "comment_on_claim": true }
            },
            "secret": "ghp_testtoken123456"
        })),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    let id = v["connector"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["connector"]["credential"], "…3456");
    assert!(!v.to_string().contains("ghp_testtoken123456"), "the credential never comes back");

    let t: Value = api(reqwest::Method::POST, format!("/api/v1/namespaces/{NS}/connectors/{id}/test"), None)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(t["ok"], true, "{t}");
    assert_eq!(t["detail"], "GitHub repository acme/shop");
    let auth = &fake.calls("GET", "/repos/acme/shop")[0].headers;
    assert_eq!(auth["authorization"], "Bearer ghp_testtoken123456");
    assert_eq!(auth["x-github-api-version"], "2026-03-10");
    assert!(auth["user-agent"].starts_with("concord-mcp/"));

    // First sync: open issues with the label; the PR and the closed issue are skipped.
    let r: Value = api(reqwest::Method::POST, format!("/api/v1/namespaces/{NS}/connectors/{id}/sync"), None)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["report"]["imported"], 2, "{r}");
    let alice = env.agent("alice").await;
    let tasks = alice.ok("list_tasks", json!({})).await;
    let list = tasks["tasks"].as_array().unwrap();
    let cart = list.iter().find(|t| t["title"] == "Cart returns 500").unwrap();
    assert_eq!(cart["kind"], "bug");
    assert_eq!(cart["area"], "backend");
    assert!(cart["description"].as_str().unwrap().contains("GitHub #1: https://github.com/acme/shop/issues/1"));
    let cart_id = cart["id"].as_str().unwrap().to_string();

    // Attach an untagged issue by URL; attaching again finds the same task.
    let a: Value = api(
        reqwest::Method::POST,
        format!("/api/v1/namespaces/{NS}/connectors/{id}/attach"),
        Some(json!({ "reference": "https://github.com/acme/shop/issues/5" })),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(a["already_linked"], false, "{a}");
    let again: Value = api(
        reqwest::Method::POST,
        format!("/api/v1/namespaces/{NS}/connectors/{id}/attach"),
        Some(json!({ "reference": "#5" })),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(again["already_linked"], true);

    // bob claims and finishes the bug: the tracker hears about it.
    let bob = env.agent("bob").await;
    bob.ok("claim_task", json!({ "task_id": cart_id })).await;
    bob.ok("set_task_status", json!({ "task_id": cart_id, "action": "start" })).await;
    bob.ok("set_task_status", json!({ "task_id": cart_id, "action": "done" })).await;
    let delivered = engine::deliver_due(&env.hub).await.unwrap();
    assert_eq!(delivered, 2, "claim comment and close go through; the done comment hits a 502");
    assert_eq!(fake.calls("PATCH", "/issues/1").len(), 1);
    assert_eq!(fake.calls("PATCH", "/issues/1")[0].body, json!({ "state": "closed", "state_reason": "completed" }));
    env.advance_minutes(2);
    assert_eq!(engine::deliver_due(&env.hub).await.unwrap(), 1, "the failed comment is retried");
    env.advance_minutes(60);
    assert_eq!(engine::deliver_due(&env.hub).await.unwrap(), 0, "nothing is sent twice");
    let comments: Vec<String> =
        fake.calls("POST", "/issues/1/comments").iter().map(|r| r.body["body"].as_str().unwrap().to_string()).collect();
    assert_eq!(comments.iter().filter(|c| c.starts_with("bob took this on")).count(), 1);
    assert_eq!(
        comments.iter().filter(|c| c.starts_with("Done in concord by bob")).count(),
        2,
        "one 502, one delivered"
    );

    // The tracker closes issue #2 and renames #5: the next sync follows.
    {
        let mut issues = state.issues.lock().unwrap();
        issues[1]["state"] = json!("closed");
        issues[1]["state_reason"] = json!("not_planned");
        issues[1]["updated_at"] = json!("2030-01-01T09:30:00Z");
        issues[3]["title"] = json!("Untagged but important (renamed)");
        issues[3]["labels"] = json!([{ "name": "agents" }]);
        issues[3]["updated_at"] = json!("2030-01-01T09:31:00Z");
    }
    let r: Value = api(reqwest::Method::POST, format!("/api/v1/namespaces/{NS}/connectors/{id}/sync"), None)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["report"]["closed"], 1, "{r}");
    assert_eq!(r["report"]["updated"], 1, "{r}");
    let all = alice.ok("list_tasks", json!({ "include_closed": true })).await;
    let landing = all["tasks"].as_array().unwrap().iter().find(|t| t["title"] == "New landing copy").unwrap().clone();
    assert_eq!(landing["status"], "cancelled");
    assert!(all["tasks"].as_array().unwrap().iter().any(|t| t["title"] == "Untagged but important (renamed)"));
    let since_q = fake.calls("GET", "/repos/acme/shop/issues").last().unwrap().query.clone();
    assert!(since_q.contains("since=") && since_q.contains("state=all"), "{since_q}");

    // Reopened in the tracker: the task opens again.
    {
        let mut issues = state.issues.lock().unwrap();
        issues[1]["state"] = json!("open");
        issues[1]["updated_at"] = json!("2030-01-01T10:00:00Z");
    }
    let r: Value = api(reqwest::Method::POST, format!("/api/v1/namespaces/{NS}/connectors/{id}/sync"), None)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["report"]["reopened"], 1, "{r}");

    // Members can read connectors but not change them.
    let resp = http
        .post(format!("http://{}/api/v1/namespaces/{NS}/connectors/{id}/sync", env.addr))
        .bearer_auth(&env.tokens["bob"])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

fn cfg(v: Value) -> Config {
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn gitlab_contract() {
    let issue = json!({
        "iid": 7, "title": "Fix login", "description": "steps", "state": "opened", "labels": ["agents", "p1"],
        "web_url": "https://git.acme.dev/acme/web/-/issues/7", "updated_at": "2030-01-01T08:00:00.000Z"
    });
    let i2 = issue.clone();
    let fake = Fake::start(Arc::new(move |r: &Rec| match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/api/v4/projects/acme%2Fweb/issues") => (200, json!([i2.clone()])),
        ("GET", "/api/v4/projects/acme%2Fweb/issues/7") => (200, i2.clone()),
        ("POST", "/api/v4/projects/acme%2Fweb/issues/7/notes") => (201, json!({})),
        ("PUT", "/api/v4/projects/acme%2Fweb/issues/7") => (200, json!({})),
        _ => (404, json!({})),
    }))
    .await;
    let t = connectors::build(
        Kind::Gitlab,
        &cfg(json!({ "base_url": fake.base, "project": "acme/web", "labels": ["agents"] })),
        "glpat-secret",
        connectors::http_client(),
    )
    .unwrap();
    let list = t.list_changed(None).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].key, "#7");
    assert_eq!(list[0].priority, Some(concord_mcp::model::Priority::P1));
    let rec = &fake.calls("GET", "/issues")[0];
    assert_eq!(rec.headers["private-token"], "glpat-secret");
    assert!(rec.query.contains("state=opened") && rec.query.contains("labels=agents"));
    assert_eq!(t.get(&format!("{}/acme/web/-/work_items/7", fake.base)).await.unwrap().title, "Fix login");
    t.comment("7", "#7", "hello").await.unwrap();
    assert_eq!(fake.calls("POST", "/notes")[0].body["body"], "hello");
    t.complete("7", "#7").await.unwrap();
    assert!(fake.calls("PUT", "/issues/7")[0].query.contains("state_event=close"));
}

#[tokio::test]
async fn jira_contract() {
    let issue = json!({
        "id": "10001", "key": "ENG-12",
        "fields": {
            "summary": "Rate limit login", "labels": ["agents"], "priority": { "name": "High" },
            "status": { "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
            "updated": "2030-01-01T08:00:00.000+0000",
            "description": { "type": "doc", "version": 1, "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "Too many tries" }] }] }
        }
    });
    let i2 = issue.clone();
    let fake = Fake::start(Arc::new(move |r: &Rec| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/rest/api/3/search/jql") => (200, json!({ "issues": [i2.clone()], "isLast": true })),
        ("GET", "/rest/api/3/issue/ENG-12") => (200, i2.clone()),
        ("POST", "/rest/api/3/issue/ENG-12/comment") => (201, json!({})),
        ("GET", "/rest/api/3/issue/ENG-12/transitions") => (
            200,
            json!({ "transitions": [
                { "id": "11", "to": { "statusCategory": { "key": "indeterminate" } } },
                { "id": "31", "to": { "statusCategory": { "key": "done" } } }
            ]}),
        ),
        ("POST", "/rest/api/3/issue/ENG-12/transitions") => (204, json!({})),
        _ => (404, json!({})),
    }))
    .await;
    let t = connectors::build(
        Kind::Jira,
        &cfg(json!({ "base_url": fake.base, "project": "ENG", "labels": ["agents"], "email": "ops@acme.dev" })),
        "jira-token",
        connectors::http_client(),
    )
    .unwrap();
    let list = t.list_changed(None).await.unwrap();
    assert_eq!(list[0].key, "ENG-12");
    assert_eq!(list[0].body, "Too many tries");
    assert!(list[0].open);
    let search = &fake.calls("POST", "/search/jql")[0];
    assert!(search.headers["authorization"].starts_with("Basic "));
    assert!(search.body["jql"].as_str().unwrap().contains("project = \"ENG\""));
    assert!(search.body["fields"].as_array().unwrap().iter().any(|f| f == "summary"));
    assert_eq!(t.get(&format!("{}/browse/ENG-12", fake.base)).await.unwrap().title, "Rate limit login");
    t.comment("10001", "ENG-12", "done here").await.unwrap();
    assert_eq!(fake.calls("POST", "/comment")[0].body["body"]["type"], "doc");
    t.complete("10001", "ENG-12").await.unwrap();
    assert_eq!(fake.calls("POST", "/transitions")[0].body, json!({ "transition": { "id": "31" } }));
}

#[tokio::test]
async fn linear_contract() {
    let node = json!({
        "id": "uuid-1", "identifier": "ENG-5", "title": "Dark mode", "description": "md", "url": "https://linear.app/acme/issue/ENG-5/dark-mode",
        "priority": 3, "updatedAt": "2030-01-01T08:00:00.000Z", "state": { "name": "Todo", "type": "unstarted" }, "labels": { "nodes": [{ "name": "agents" }] }
    });
    let n2 = node.clone();
    let fake = Fake::start(Arc::new(move |r: &Rec| {
        let q = r.body["query"].as_str().unwrap_or("");
        let data = if q.contains("issues(") {
            json!({ "issues": { "nodes": [n2.clone()], "pageInfo": { "hasNextPage": false } } })
        } else if q.contains("issue(id") {
            json!({ "issue": n2.clone() })
        } else if q.contains("commentCreate") {
            json!({ "commentCreate": { "success": true } })
        } else if q.contains("workflowStates") {
            json!({ "workflowStates": { "nodes": [{ "id": "done-2", "position": 2.0 }, { "id": "done-1", "position": 1.0 }] } })
        } else if q.contains("issueUpdate") {
            json!({ "issueUpdate": { "success": true } })
        } else {
            json!({})
        };
        (200, json!({ "data": data }))
    }))
    .await;
    let t = connectors::build(
        Kind::Linear,
        &cfg(json!({ "base_url": fake.base, "project": "eng", "labels": ["agents"] })),
        "lin_api_key",
        connectors::http_client(),
    )
    .unwrap();
    let list = t.list_changed(None).await.unwrap();
    assert_eq!(list[0].key, "ENG-5");
    let first = &fake.calls("POST", "/")[0];
    assert_eq!(first.headers["authorization"], "lin_api_key", "personal keys go without Bearer");
    assert_eq!(first.body["variables"]["filter"]["team"]["key"]["eq"], "ENG");
    assert_eq!(t.get("https://linear.app/acme/issue/ENG-5/dark-mode").await.unwrap().title, "Dark mode");
    t.comment("uuid-1", "ENG-5", "on it").await.unwrap();
    t.complete("uuid-1", "ENG-5").await.unwrap();
    let update = fake
        .calls("POST", "/")
        .into_iter()
        .find(|r| r.body["query"].as_str().unwrap().contains("issueUpdate"))
        .unwrap();
    assert_eq!(update.body["variables"]["state"], "done-1", "the first completed state");
}

#[tokio::test]
async fn plane_contract() {
    let item = json!({
        "id": "wi-1", "sequence_id": 12, "name": "Export CSV", "description_stripped": "as a file",
        "state": { "id": "st-todo", "name": "Todo", "group": "unstarted" }, "labels": [{ "name": "agents" }],
        "priority": "high", "updated_at": "2030-01-01T08:00:00Z"
    });
    let i2 = item.clone();
    let fake = Fake::start(Arc::new(move |r: &Rec| match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/api/v1/workspaces/acme/projects/") => (200, json!({ "results": [{ "id": "p-uuid", "identifier": "WEB", "name": "Web" }] })),
        ("GET", "/api/v1/workspaces/acme/projects/p-uuid/") => (200, json!({ "id": "p-uuid", "identifier": "WEB", "name": "Web" })),
        ("GET", "/api/v1/workspaces/acme/projects/p-uuid/work-items/") => (200, json!({ "results": [i2.clone()], "next_page_results": false })),
        ("GET", "/api/v1/workspaces/acme/projects/p-uuid/states/") => (200, json!([
            { "id": "st-todo", "name": "Todo", "group": "unstarted" }, { "id": "st-done", "name": "Done", "group": "completed" }
        ])),
        ("GET", "/api/v1/workspaces/acme/projects/p-uuid/labels/") => (200, json!([])),
        ("GET", "/api/v1/workspaces/acme/work-items/WEB-12/") => (200, i2.clone()),
        ("POST", "/api/v1/workspaces/acme/projects/p-uuid/work-items/wi-1/comments/") => (201, json!({})),
        ("PATCH", "/api/v1/workspaces/acme/projects/p-uuid/work-items/wi-1/") => (200, json!({})),
        _ => (404, json!({})),
    }))
    .await;
    let t = connectors::build(
        Kind::Plane,
        &cfg(json!({ "base_url": fake.base, "project": "acme/WEB", "labels": ["agents"], "web_url": "https://plane.acme.dev" })),
        "plane_api_key",
        connectors::http_client(),
    )
    .unwrap();
    let list = t.list_changed(None).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].key, "WEB-12");
    assert_eq!(list[0].url, "https://plane.acme.dev/acme/browse/WEB-12/");
    assert_eq!(fake.calls("GET", "/projects/")[0].headers["x-api-key"], "plane_api_key");
    assert_eq!(t.get("https://plane.acme.dev/acme/browse/WEB-12/").await.unwrap().title, "Export CSV");
    t.comment("wi-1", "WEB-12", "a < b").await.unwrap();
    assert_eq!(fake.calls("POST", "/comments/")[0].body["comment_html"], "<p>a &lt; b</p>");
    t.complete("wi-1", "WEB-12").await.unwrap();
    assert_eq!(fake.calls("PATCH", "/work-items/wi-1/")[0].body, json!({ "state": "st-done" }));
}

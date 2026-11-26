//! End-to-end: two (or three) real MCP clients with their own tokens against one hub.
//! Needs Postgres: DATABASE_URL=postgres://user:pass@127.0.0.1:5432/postgres (see scripts/test-db.sh).

mod common;

use std::time::{Duration, Instant};

use common::{Env, NS};
use rmcp::model::{
    ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult, ElicitationAction, Implementation,
    ProtocolVersion,
};
use rmcp::service::RequestContext;
use rmcp::{ClientHandler, ErrorData, RoleClient};
use serde_json::{Value, json};
use sqlx::PgPool;

fn task_id(v: &Value) -> String {
    v["task"]["id"].as_str().expect("task id").to_string()
}

fn neg_id(v: &Value) -> String {
    v["proposal"]["negotiation"]["id"]
        .as_str()
        .or_else(|| v["negotiation"]["id"].as_str())
        .or_else(|| v["id"].as_str())
        .expect("negotiation id")
        .to_string()
}

async fn task_status<H: ClientHandler>(a: &common::Agent<H>, id: &str) -> Value {
    a.ok("get_task", json!({ "task_id": id })).await["task"].clone()
}

#[sqlx::test(migrations = "./migrations")]
async fn http_auth_sessions_and_isolation(pool: PgPool) {
    let mut env = Env::new(pool).await;
    let http = reqwest::Client::new();
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "raw", "version": "1" } }
    });
    let post = |token: Option<String>, body: Value, sid: Option<String>| {
        let mut r = http
            .post(env.url())
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(t) = token {
            r = r.header("authorization", format!("Bearer {t}"));
        }
        if let Some(s) = sid {
            r = r.header("mcp-session-id", s).header("mcp-protocol-version", "2025-11-25");
        }
        r.send()
    };

    assert_eq!(post(None, init.clone(), None).await.unwrap().status(), 401);
    assert_eq!(post(Some("cnc_wrong".into()), init.clone(), None).await.unwrap().status(), 401);
    let evil = http
        .post(env.url())
        .header("authorization", format!("Bearer {}", env.tokens["alice"]))
        .header("origin", "https://evil.example")
        .header("content-type", "application/json")
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(evil.status(), 403);

    let r = post(Some(env.tokens["alice"].clone()), init.clone(), None).await.unwrap();
    assert_eq!(r.status(), 200);
    let sid = r.headers().get("mcp-session-id").expect("session id").to_str().unwrap().to_string();
    let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
    let hijack = post(Some(env.tokens["bob"].clone()), list.clone(), Some(sid.clone())).await.unwrap();
    assert_eq!(hijack.status(), 404, "a session must not be usable with another member's token");

    let plan = http
        .get(format!("http://{}/api/v1/namespaces/demo/plan.md", env.addr))
        .header("authorization", format!("Bearer {}", env.tokens["bob"]))
        .send()
        .await
        .unwrap();
    assert_eq!(plan.status(), 200);
    assert!(plan.text().await.unwrap().contains("# Demo project"));
    let health = http.get(format!("http://{}/healthz", env.addr)).send().await.unwrap();
    assert_eq!(health.status(), 200);

    // Another namespace sees nothing of this one.
    let alice = env.agent("alice").await;
    alice.ok("create_task", json!({ "title": "secret backend work", "area": "backend" })).await;
    concord_mcp::ops::admin::create_namespace(&env.hub, "other", "Other", "", &Default::default(), "operator")
        .await
        .unwrap();
    env.add_member("other", "carol", vec![]).await;
    let carol = env.agent("carol").await;
    let tasks = carol.ok("list_tasks", json!({})).await;
    assert_eq!(tasks["tasks"].as_array().unwrap().len(), 0);
    let e = carol.err("get_task", json!({ "task_id": "T-1" })).await;
    assert_eq!(e["code"], "not_found");
    let who = carol.ok("whoami", json!({})).await;
    assert_eq!(who["namespace"]["id"], "other");
    assert_eq!(who["you"]["role"], "lead");

    let tools = alice.client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 17, "tools: {:?}", tools.iter().map(|t| t.name.to_string()).collect::<Vec<_>>());
}

#[sqlx::test(migrations = "./migrations")]
async fn claims_are_exclusive(pool: PgPool) {
    let env = Env::new(pool).await;
    let (alice, bob) = (env.agent("alice").await, env.agent("bob").await);
    for i in 0..5 {
        let t = task_id(&alice.ok("create_task", json!({ "title": format!("Race {i}") })).await);
        let (a, b) = tokio::join!(
            alice.try_call("claim_task", json!({ "task_id": t })),
            bob.try_call("claim_task", json!({ "task_id": t }))
        );
        let winners = [a.0, b.0].iter().filter(|is_err| !**is_err).count();
        assert_eq!(winners, 1, "exactly one claim wins: {a:?} {b:?}");
        let loser = if a.0 { &a.1 } else { &b.1 };
        assert_eq!(loser["error"]["code"], "conflict");
        assert!(loser["error"]["details"]["holder"].is_string());
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn bugs_filed_into_partner_area(pool: PgPool) {
    let env = Env::new(pool).await;
    let (alice, bob) = (env.agent("alice").await, env.agent("bob").await);

    // 1. Accepted explicitly.
    let r = bob
        .ok(
            "create_task",
            json!({
                "title": "500 on empty cart", "kind": "bug", "paths": ["server/api/cart.rs"],
                "repro": "POST /cart with []", "evidence": "panic at cart.rs:42", "rationale": "blocks checkout UI"
            }),
        )
        .await;
    let t1 = task_id(&r);
    assert_eq!(r["task"]["status"], "proposed");
    assert_eq!(r["task"]["area"], "backend");
    let n1 = neg_id(&r);
    let s = alice.ok("sync", json!({})).await;
    let awaiting = s["attention"]["negotiations_awaiting_you"].as_array().unwrap();
    assert_eq!(awaiting.len(), 1);
    assert_eq!(awaiting[0]["id"], n1.as_str());
    assert_eq!(awaiting[0]["you_must_answer"], true);
    let v = alice.ok("respond", json!({ "negotiation_id": n1, "round": 1, "action": "accept" })).await;
    assert_eq!(v["status"], "closed");
    assert_eq!(v["outcome"], "agreed");
    assert_eq!(task_status(&alice, &t1).await["status"], "open");

    // 2. Silence counts as consent for a safe change.
    let r = bob.ok("create_task", json!({ "title": "Typo in error text", "kind": "bug", "area": "backend" })).await;
    let t2 = task_id(&r);
    env.advance_minutes(31);
    let rep = env.tick().await;
    assert_eq!(rep.negotiations_settled, 1);
    assert_eq!(task_status(&alice, &t2).await["status"], "open");

    // 3. The owner's rejection stands when the proposer does not counter.
    let r = bob.ok("create_task", json!({ "title": "Rewrite backend in another language", "area": "backend" })).await;
    let (t3, n3) = (task_id(&r), neg_id(&r));
    alice.ok("respond", json!({ "negotiation_id": n3, "round": 1, "action": "reject", "note": "out of scope" })).await;
    env.advance_minutes(31);
    env.tick().await;
    assert_eq!(task_status(&alice, &t3).await["status"], "rejected");
    let n = alice.ok("list_negotiations", json!({ "negotiation_id": n3 })).await;
    assert_eq!(n["negotiation"]["outcome"], "policy");

    // Stale round answers are refused.
    let r = bob.ok("create_task", json!({ "title": "Add an index", "area": "backend" })).await;
    let n4 = neg_id(&r);
    let e = alice.err("respond", json!({ "negotiation_id": n4, "round": 7, "action": "accept" })).await;
    assert_eq!(e["code"], "conflict");
}

#[sqlx::test(migrations = "./migrations")]
async fn counter_offer_splits_the_work(pool: PgPool) {
    let env = Env::new(pool).await;
    let (alice, bob) = (env.agent("alice").await, env.agent("bob").await);
    let t =
        task_id(&bob.ok("create_task", json!({ "title": "Checkout page", "area": "frontend", "claim": true })).await);
    let r = alice
        .ok(
            "propose",
            json!({ "changes": [{ "op": "assign", "task_id": t, "to": "alice" }], "rationale": "I know the payment API" }),
        )
        .await;
    assert_eq!(r["result"], "proposed");
    let n = r["negotiation"]["id"].as_str().unwrap().to_string();
    assert_eq!(r["negotiation"]["risk"], "safe");
    let v = bob
        .ok(
            "respond",
            json!({
                "negotiation_id": n, "round": 1, "action": "counter", "note": "let's split it",
                "changes": [{ "op": "split", "task_id": t, "parts": [
                    { "title": "Checkout UI", "assignee": "bob" },
                    { "title": "Checkout payment calls", "assignee": "alice" }
                ]}]
            }),
        )
        .await;
    assert_eq!(v["round"], 2);
    assert_eq!(v["awaiting"], json!(["alice"]));
    let v = alice.ok("respond", json!({ "negotiation_id": n, "round": 2, "action": "accept" })).await;
    assert_eq!(v["outcome"], "agreed");
    assert_eq!(v["applied_round"], 2);
    assert_eq!(task_status(&alice, &t).await["status"], "cancelled");
    let mine = alice.ok("list_tasks", json!({ "mine": true })).await;
    let titles: Vec<&str> = mine["tasks"].as_array().unwrap().iter().map(|t| t["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["Checkout payment calls"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn area_owner_wins_a_deadlock(pool: PgPool) {
    let env = Env::new(pool).await;
    let (alice, bob) = (env.agent("alice").await, env.agent("bob").await);
    let t = task_id(&alice.ok("create_task", json!({ "title": "Auth middleware", "area": "backend" })).await);
    bob.ok("claim_task", json!({ "task_id": t })).await; // allowed: claims are open to everyone
    let r = alice.ok("propose", json!({ "changes": [{ "op": "assign", "task_id": t, "to": "alice" }] })).await;
    let n = r["negotiation"]["id"].as_str().unwrap().to_string();
    bob.ok("respond", json!({ "negotiation_id": n, "round": 1, "action": "reject", "note": "I already started" }))
        .await;
    env.advance_minutes(31);
    env.tick().await;
    let task = task_status(&alice, &t).await;
    assert_eq!(task["assignee"], "alice", "area owner > current assignee");
    let v = bob.ok("list_negotiations", json!({ "negotiation_id": n })).await;
    assert_eq!(v["negotiation"]["outcome"], "policy");
    let d = alice.ok("get_plan", json!({})).await;
    assert!(d["recent_decisions"].as_array().unwrap().iter().any(|x| x["negotiation_id"] == n.as_str()));
}

#[sqlx::test(migrations = "./migrations")]
async fn destructive_changes_go_to_humans(pool: PgPool) {
    let env = Env::new(pool).await;
    let (alice, bob) = (env.agent("alice").await, env.agent("bob").await);
    let t =
        task_id(&alice.ok("create_task", json!({ "title": "Payments retry", "area": "backend", "claim": true })).await);
    let r = bob
        .ok(
            "propose",
            json!({ "changes": [{ "op": "cancel_task", "task_id": t, "reason": "the provider retries for us" }] }),
        )
        .await;
    let n = r["negotiation"]["id"].as_str().unwrap().to_string();
    assert_eq!(r["negotiation"]["risk"], "destructive");
    env.advance_minutes(31);
    env.tick().await;

    let s = alice.ok("sync", json!({})).await;
    assert_eq!(s["attention"]["escalated_ask_your_human"][0]["id"], n.as_str());
    let e = alice.err("set_task_status", json!({ "task_id": t, "action": "start" })).await;
    assert!(e["message"].as_str().unwrap().contains("frozen"), "{e}");
    let e = alice.err("respond", json!({ "negotiation_id": n, "round": 1, "action": "accept" })).await;
    assert!(e["message"].as_str().unwrap().contains("human"), "{e}");
    bob.ok("respond", json!({ "negotiation_id": n, "round": 1, "action": "accept", "human_confirmed": true })).await;
    let v = alice
        .ok("respond", json!({ "negotiation_id": n, "round": 1, "action": "accept", "human_confirmed": true }))
        .await;
    assert_eq!(v["outcome"], "humans");
    assert_eq!(task_status(&alice, &t).await["status"], "cancelled");

    // Nobody decides: the escalation times out and nothing changes.
    let t2 = task_id(&alice.ok("create_task", json!({ "title": "Webhooks", "area": "backend" })).await);
    let r =
        bob.ok("propose", json!({ "changes": [{ "op": "cancel_task", "task_id": t2, "reason": "not needed" }] })).await;
    let n2 = r["negotiation"]["id"].as_str().unwrap().to_string();
    env.advance_minutes(31);
    env.tick().await;
    env.advance_minutes(24 * 60 + 1);
    env.tick().await;
    let v = alice.ok("list_negotiations", json!({ "negotiation_id": n2 })).await;
    assert_eq!(v["negotiation"]["outcome"], "escalation_timeout");
    assert_eq!(task_status(&alice, &t2).await["status"], "open");

    // The hub operator can always settle it.
    let r = bob.ok("propose", json!({ "changes": [{ "op": "cancel_task", "task_id": t2, "reason": "really" }] })).await;
    let n3 = r["negotiation"]["id"].as_str().unwrap().to_string();
    let v = concord_mcp::ops::admin::resolve(&env.hub, NS, &n3, None, "keep it", "operator").await.unwrap();
    assert_eq!(v["outcome"], "admin");
    assert_eq!(task_status(&alice, &t2).await["status"], "open");
}

#[sqlx::test(migrations = "./migrations")]
async fn versions_leases_and_next_task(pool: PgPool) {
    let env = Env::new(pool).await;
    let (alice, bob) = (env.agent("alice").await, env.agent("bob").await);
    let r = alice.ok("create_task", json!({ "title": "API schema", "area": "backend", "priority": "p1" })).await;
    let tb = task_id(&r);
    let e =
        alice.err("update_task", json!({ "task_id": tb, "expected_version": 99, "patch": { "priority": "p0" } })).await;
    assert_eq!(e["code"], "conflict");
    let ok =
        alice.ok("update_task", json!({ "task_id": tb, "expected_version": 1, "patch": { "priority": "p0" } })).await;
    assert_eq!(ok["result"], "applied");

    let tf = task_id(&bob.ok("create_task", json!({ "title": "Landing page", "area": "frontend" })).await);
    let ts = task_id(&bob.ok("create_task", json!({ "title": "CI pipeline" })).await);
    let a = alice.ok("next_task", json!({ "claim": true })).await;
    assert_eq!(a["claimed"]["task"]["id"], tb.as_str(), "own area first");
    let b = bob.ok("next_task", json!({ "claim": true })).await;
    assert_eq!(b["claimed"]["task"]["id"], tf.as_str());
    let a2 = alice.ok("next_task", json!({ "claim": true })).await;
    assert_eq!(a2["claimed"]["task"]["id"], ts.as_str(), "then shared");

    // A silent holder loses the task when the lease runs out.
    env.advance_minutes(121);
    let rep = env.tick().await;
    assert_eq!(rep.leases_expired, 3);
    assert_eq!(task_status(&alice, &tb).await["status"], "open");
    let s = bob.ok("sync", json!({ "since": 0 })).await;
    assert!(s["events"].as_array().unwrap().iter().any(|e| e["kind"] == "task.lease_expired"));

    // Dependencies gate start.
    let dep = task_id(&alice.ok("create_task", json!({ "title": "Deploy", "depends_on": [tb], "claim": true })).await);
    let e = alice.err("set_task_status", json!({ "task_id": dep, "action": "start" })).await;
    assert_eq!(e["code"], "conflict");
    alice.ok("claim_task", json!({ "task_id": tb })).await;
    alice.ok("set_task_status", json!({ "task_id": tb, "action": "start" })).await;
    let done = alice.ok("set_task_status", json!({ "task_id": tb, "action": "done" })).await;
    assert_eq!(done["now_unblocked"], json!([dep]));
    alice.ok("set_task_status", json!({ "task_id": dep, "action": "start" })).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn wait_for_updates_wakes_on_partner_activity(pool: PgPool) {
    let env = Env::new(pool).await;
    let (alice, bob) = (env.agent("alice").await, env.agent("bob").await);
    let cursor = alice.ok("sync", json!({})).await["cursor"].as_i64().unwrap();
    let started = Instant::now();
    let waiter = alice.try_call("wait_for_updates", json!({ "since": cursor, "timeout_secs": 30 }));
    let sender = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        bob.ok("send_message", json!({ "to": "alice", "text": "API is ready, can you test the cart?" })).await
    };
    let ((is_err, v), _) = tokio::join!(waiter, sender);
    assert!(!is_err, "{v}");
    assert_eq!(v["woke_up"], true);
    assert!(started.elapsed() < Duration::from_secs(10), "woke after {:?}", started.elapsed());
    let msg = v["events"].as_array().unwrap().iter().find(|e| e["kind"] == "message").expect("message event");
    assert_eq!(msg["data"]["message"]["from"], "bob");
    assert!(msg["data"]["message"]["untrusted_text"].as_str().unwrap().contains("cart"));
}

#[sqlx::test(migrations = "./migrations")]
async fn plan_changes_need_leads(pool: PgPool) {
    let env = Env::new(pool).await;
    let (alice, bob) = (env.agent("alice").await, env.agent("bob").await);
    let v = alice
        .ok(
            "update_plan",
            json!({ "ops": [
                { "op": "set_goal", "goal": "Ship the shop MVP" },
                { "op": "add_milestone", "title": "MVP" },
                { "op": "add_note", "text": "we use Postgres" }
            ]}),
        )
        .await;
    assert_eq!(v["applied"].as_array().unwrap().len(), 3);
    assert!(v["proposal"].is_null());
    // bob is not a lead: replacing the goal needs alice.
    let v = bob
        .ok(
            "update_plan",
            json!({ "ops": [
            { "op": "set_goal", "goal": "Ship the shop MVP with payments" },
            { "op": "add_milestone", "title": "Payments" }
        ], "rationale": "payments are in scope now" }),
        )
        .await;
    assert_eq!(v["applied"].as_array().unwrap().len(), 1);
    assert_eq!(v["proposal"]["negotiation"]["risk"], "destructive");
    assert_eq!(v["proposal"]["negotiation"]["awaiting"], json!(["alice"]));
    let md = alice.ok("get_plan", json!({ "format": "markdown" })).await;
    let md = md["markdown"].as_str().unwrap();
    assert!(md.contains("Ship the shop MVP") && md.contains("Payments") && md.contains("Open negotiations"), "{md}");
}

/// A client whose human answers forms: "accept".
#[derive(Clone)]
struct HumanSaysYes;

impl ClientHandler for HumanSaysYes {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(ClientCapabilities::builder().enable_elicitation().build(), Implementation::new("human", "1"))
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
    }

    async fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        Ok(ElicitResult::new(ElicitationAction::Accept)
            .with_content(json!({ "accept": true, "comment": "fine by me" })))
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn escalation_asks_the_human_through_a_form(pool: PgPool) {
    let env = Env::new(pool).await;
    let alice = env.agent_with("alice", HumanSaysYes).await;
    let bob = env.agent("bob").await;
    let t = task_id(&alice.ok("create_task", json!({ "title": "Old importer", "area": "backend" })).await);
    let r =
        bob.ok("propose", json!({ "changes": [{ "op": "cancel_task", "task_id": t, "reason": "replaced" }] })).await;
    let n = r["negotiation"]["id"].as_str().unwrap().to_string();
    env.advance_minutes(31);
    env.tick().await;
    bob.ok("respond", json!({ "negotiation_id": n, "round": 1, "action": "accept", "human_confirmed": true })).await;
    // alice's agent does not claim a human decision; the hub asks alice's human directly.
    let v = alice.ok("respond", json!({ "negotiation_id": n, "round": 1, "action": "accept" })).await;
    assert_eq!(v["outcome"], "humans", "{v:#}");
    assert_eq!(task_status(&alice, &t).await["status"], "cancelled");
}

#[sqlx::test(migrations = "./migrations")]
async fn two_hub_instances_share_state_and_wake_each_other(pool: PgPool) {
    let env = Env::new(pool).await;
    let second = env.second_instance().await;
    let alice = env.agent("alice").await; // instance 1
    let bob = env.agent_at("bob", second).await; // instance 2
    let t = task_id(&bob.ok("create_task", json!({ "title": "Shared via instance 2" })).await);
    assert_eq!(task_status(&alice, &t).await["title"], "Shared via instance 2");

    let cursor = alice.ok("sync", json!({})).await["cursor"].as_i64().unwrap();
    // Give alice's long-poll a moment to subscribe before bob writes.
    let waiter = alice.try_call("wait_for_updates", json!({ "since": cursor, "timeout_secs": 30 }));
    let writer = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        bob.ok("claim_task", json!({ "task_id": t })).await
    };
    let started = Instant::now();
    let ((is_err, v), _) = tokio::join!(waiter, writer);
    assert!(!is_err, "{v}");
    assert_eq!(v["woke_up"], true);
    // Woken by Postgres NOTIFY, not by the 5 s safety re-check.
    assert!(started.elapsed() < Duration::from_secs(4), "woke after {:?}", started.elapsed());
    assert!(v["events"].as_array().unwrap().iter().any(|e| e["kind"] == "task.claimed"));
}

#[sqlx::test(migrations = "./migrations")]
async fn many_agents_split_the_backlog_without_duplicates(pool: PgPool) {
    let mut env = Env::new(pool).await;
    let names: Vec<String> = (1..=20).map(|i| format!("agent{i:02}")).collect();
    for n in &names {
        env.add_member(NS, n, vec![]).await;
    }
    let alice = env.agent("alice").await;
    for i in 0..60 {
        alice.ok("create_task", json!({ "title": format!("Backlog item {i}") })).await;
    }
    let mut agents = vec![];
    for n in &names {
        agents.push(env.agent(n).await);
    }
    let runs = agents.iter().map(|a| async move {
        let mut got = vec![];
        loop {
            let v = a.ok("next_task", json!({ "claim": true })).await;
            match v["claimed"]["task"]["id"].as_str() {
                Some(id) => got.push(id.to_string()),
                None => return got,
            }
        }
    });
    let results = futures::future::join_all(runs).await;
    let mut all: Vec<String> = results.iter().flatten().cloned().collect();
    let total = all.len();
    all.sort();
    all.dedup();
    assert_eq!(total, 60, "every task claimed once");
    assert_eq!(all.len(), 60, "no task claimed twice");
    assert!(results.iter().filter(|r| !r.is_empty()).count() > 1, "work was spread across agents");
}

#[sqlx::test(migrations = "./migrations")]
async fn first_start_bootstrap_is_idempotent(pool: PgPool) {
    use concord_mcp::invite::{InitSpec, bootstrap, parse_members};
    let mut env = Env::new(pool).await;
    let dir = std::env::temp_dir().join(format!("concord-invites-{}", std::process::id()));
    let spec = InitSpec {
        namespace: "boot".into(),
        name: "Bootstrapped".into(),
        goal: "Ship it".into(),
        members: parse_members("dana*:api=api/** eve:ui=ui/**").unwrap(),
        settings: Default::default(),
    };
    let first = bootstrap(&env.hub, &spec, Some(&dir)).await.unwrap();
    assert_eq!(first.len(), 2);
    let again = bootstrap(&env.hub, &spec, Some(&dir)).await.unwrap();
    assert!(again.is_empty(), "a restart must not create members or tokens again");

    let path = first[0].1.clone().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(&format!("claude mcp add --transport http concord-boot {}/mcp/boot", env.hub.public_url())),
        "{text}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let token = text.split_whitespace().find(|w| w.starts_with("cnc_")).unwrap().trim_matches('"').to_string();
    env.tokens.insert("dana".into(), token);
    let dana = env.agent("dana").await;
    let who = dana.ok("whoami", json!({})).await;
    assert_eq!(who["namespace"]["id"], "boot");
    assert_eq!(who["namespace"]["goal"], "Ship it");
    assert_eq!(who["you"]["role"], "lead");
    assert_eq!(who["your_areas"][0]["name"], "api");
    let _ = std::fs::remove_dir_all(&dir);
}

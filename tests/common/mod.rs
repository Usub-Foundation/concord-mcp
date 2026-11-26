#![allow(dead_code)]
//! Test harness: a real hub on a random local port, talking to real MCP clients.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use concord_mcp::clock::ManualClock;
use concord_mcp::http::{self, HttpConfig};
use concord_mcp::ops::admin::{self, NamespaceSettings, NewMember};
use concord_mcp::sweeper::TickReport;
use concord_mcp::{Hub, notify};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::Value;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

pub const NS: &str = "demo";

pub struct Env {
    pub hub: Arc<Hub>,
    pub clock: Arc<ManualClock>,
    pub addr: SocketAddr,
    pub tokens: HashMap<String, String>,
    cancel: CancellationToken,
    pool: PgPool,
}

async fn spawn_instance(pool: PgPool, clock: Arc<ManualClock>, cancel: &CancellationToken) -> (Arc<Hub>, SocketAddr) {
    let hub = Arc::new(Hub::new(pool.clone(), clock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = HttpConfig {
        allowed_hosts: vec![addr.to_string(), "127.0.0.1".into(), "localhost".into()],
        rate_per_sec: 10_000.0,
        burst: 10_000.0,
        ..HttpConfig::default()
    };
    let app = http::router(hub.clone(), cfg, cancel.child_token());
    let stop = cancel.child_token();
    tokio::spawn(async move {
        axum::serve(listener, app).with_graceful_shutdown(async move { stop.cancelled().await }).await.unwrap();
    });
    tokio::spawn(notify::listen(pool, hub.notifier.clone(), cancel.child_token()));
    (hub, addr)
}

impl Drop for Env {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Env {
    /// Namespace `demo` with alice (lead, area backend = server/**) and bob (area frontend = web/**).
    pub async fn new(pool: PgPool) -> Env {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let clock = Arc::new(ManualClock::new(time::macros::datetime!(2030-01-01 09:00 UTC)));
        let cancel = CancellationToken::new();
        let (hub, addr) = spawn_instance(pool.clone(), clock.clone(), &cancel).await;

        admin::create_namespace(&hub, NS, "Demo project", "", &NamespaceSettings::default(), "operator").await.unwrap();
        let mut tokens = HashMap::new();
        for (id, area, glob) in [("alice", "backend", "server/**"), ("bob", "frontend", "web/**")] {
            let t = admin::add_member(
                &hub,
                NS,
                NewMember {
                    id: id.into(),
                    display_name: format!("{id}'s agent"),
                    human: id.into(),
                    lead: false,
                    role: None,
                    areas: vec![(area.into(), vec![glob.into()])],
                },
            )
            .await
            .unwrap()
            .expect("new principal gets a token");
            tokens.insert(id.to_string(), t);
        }
        Env { hub, clock, addr, tokens, cancel, pool }
    }

    /// Another hub process on the same database (as behind a load balancer).
    pub async fn second_instance(&self) -> SocketAddr {
        spawn_instance(self.pool.clone(), self.clock.clone(), &self.cancel).await.1
    }

    pub async fn agent_at(&self, who: &str, addr: SocketAddr) -> Agent<()> {
        let cfg = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(self.tokens[who].clone());
        let client = ().serve(StreamableHttpClientTransport::from_config(cfg)).await.expect("connect");
        Agent { id: who.to_string(), client }
    }

    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    pub async fn agent(&self, who: &str) -> Agent<()> {
        self.agent_with(who, ()).await
    }

    pub async fn agent_with<H: ClientHandler>(&self, who: &str, handler: H) -> Agent<H> {
        let cfg = StreamableHttpClientTransportConfig::with_uri(self.url()).auth_header(self.tokens[who].clone());
        let client = handler.serve(StreamableHttpClientTransport::from_config(cfg)).await.expect("connect");
        Agent { id: who.to_string(), client }
    }

    pub fn advance_minutes(&self, m: i64) {
        self.clock.advance(time::Duration::minutes(m));
    }

    pub async fn tick(&self) -> TickReport {
        self.hub.tick().await.expect("tick")
    }

    pub async fn add_member(&mut self, ns: &str, id: &str, areas: Vec<(&str, &str)>) {
        let t = admin::add_member(
            &self.hub,
            ns,
            NewMember {
                id: id.into(),
                display_name: id.into(),
                human: id.into(),
                lead: false,
                role: None,
                areas: areas.into_iter().map(|(a, g)| (a.to_string(), vec![g.to_string()])).collect(),
            },
        )
        .await
        .unwrap()
        .expect("new principal gets a token");
        self.tokens.insert(id.to_string(), t);
    }
}

pub struct Agent<H: ClientHandler> {
    pub id: String,
    pub client: RunningService<RoleClient, H>,
}

impl<H: ClientHandler> Agent<H> {
    pub async fn try_call(&self, tool: &str, args: Value) -> (bool, Value) {
        let args = args.as_object().cloned().unwrap_or_default();
        let r = self
            .client
            .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
            .await
            .unwrap_or_else(|e| panic!("{} -> {tool}: transport error {e}", self.id));
        (r.is_error.unwrap_or(false), r.structured_content.unwrap_or(Value::Null))
    }

    /// Calls a tool and expects success.
    pub async fn ok(&self, tool: &str, args: Value) -> Value {
        let (is_err, v) = self.try_call(tool, args).await;
        assert!(!is_err, "{} -> {tool} failed: {v:#}", self.id);
        v
    }

    /// Calls a tool and expects a business error; returns `error`.
    pub async fn err(&self, tool: &str, args: Value) -> Value {
        let (is_err, v) = self.try_call(tool, args).await;
        assert!(is_err, "{} -> {tool} unexpectedly succeeded: {v:#}", self.id);
        v["error"].clone()
    }
}

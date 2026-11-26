use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use concord_mcp::access::{self, Grant};
use concord_mcp::clock::{SystemClock, parse_duration};
use concord_mcp::http::HttpConfig;
use concord_mcp::model::{NewTask, Priority, Role, TaskKind};
use concord_mcp::ops::{admin, import, plan, tasks};
use concord_mcp::{Hub, ServeOptions, invite};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "concord", version, about = "MCP hub for AI agents of different people working on one plan")]
struct Cli {
    /// Postgres connection string.
    #[arg(long, env = "DATABASE_URL", global = true)]
    database_url: Option<String>,
    /// Public base URL of the hub (e.g. https://concord.example.com), used in invites and hints.
    #[arg(long, env = "CONCORD_PUBLIC_URL", global = true)]
    public_url: Option<String>,
    /// Key that encrypts tracker credentials (base64 of 32 bytes). Default: --secret-key-file.
    #[arg(long, env = "CONCORD_SECRET_KEY", global = true, hide_env_values = true)]
    secret_key: Option<String>,
    /// File with that key; created on first use (keep it with the database backups).
    #[arg(long, env = "CONCORD_SECRET_KEY_FILE", global = true, default_value = "secret.key")]
    secret_key_file: std::path::PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the hub (MCP, REST API, web UI). Runs migrations first.
    Serve(ServeArgs),
    /// Apply database migrations and exit.
    Migrate,
    #[command(subcommand)]
    Namespace(NamespaceCmd),
    /// Members of one namespace (creates principals as needed).
    #[command(subcommand)]
    Member(MemberCmd),
    /// Accounts and their access to namespaces.
    #[command(subcommand)]
    Principal(PrincipalCmd),
    #[command(subcommand)]
    Token(TokenCmd),
    /// Add tasks by hand or import them from a file.
    #[command(subcommand)]
    Task(TaskCmd),
    /// Task-tracker connectors (configure them in the web UI or the API).
    #[command(subcommand)]
    Connector(ConnectorCmd),
    /// Print the plan of a namespace as markdown.
    Status { namespace: String },
    /// Exit 0 if the hub on this machine answers /healthz (for container health checks).
    Healthcheck {
        #[arg(long, env = "CONCORD_LISTEN", default_value = "127.0.0.1:8787")]
        addr: SocketAddr,
    },
    /// Force a decision on a negotiation (e.g. an escalated one nobody settles).
    Resolve {
        namespace: String,
        negotiation: String,
        /// Apply the offer of this round.
        #[arg(long, conflicts_with = "status_quo")]
        round: Option<i32>,
        /// Change nothing.
        #[arg(long)]
        status_quo: bool,
        #[arg(long, default_value = "")]
        note: String,
    },
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, env = "CONCORD_LISTEN", default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    /// Extra allowed Host header values (host or host:port). Repeatable.
    #[arg(long = "allowed-host", env = "CONCORD_ALLOWED_HOSTS", value_delimiter = ',')]
    allowed_hosts: Vec<String>,
    /// Browser origins allowed to call MCP. Repeatable. Default: none (the web UI is same-origin).
    #[arg(long = "allowed-origin", env = "CONCORD_ALLOWED_ORIGINS", value_delimiter = ',')]
    allowed_origins: Vec<String>,
    /// Requests per second per principal (sustained).
    #[arg(long, env = "CONCORD_RATE_LIMIT", default_value_t = 10.0)]
    rate_limit: f64,
    #[arg(long, env = "CONCORD_RATE_BURST", default_value_t = 60.0)]
    rate_burst: f64,
    /// How often leases, negotiation deadlines and tracker syncs are checked.
    #[arg(long, env = "CONCORD_SWEEP_EVERY", default_value = "5s")]
    sweep_every: String,
    #[arg(long, env = "CONCORD_DB_MAX_CONNECTIONS", default_value_t = 20)]
    db_max_connections: u32,
    /// On start, create this namespace if it does not exist (default "main" when members are given).
    #[arg(long, env = "CONCORD_INIT_NAMESPACE")]
    init_namespace: Option<String>,
    #[arg(long, env = "CONCORD_INIT_NAME")]
    init_name: Option<String>,
    #[arg(long, env = "CONCORD_INIT_GOAL", default_value = "")]
    init_goal: String,
    /// Members to create if missing: "alice*:backend=server/**,db/** bob:frontend=web/**" (* = lead).
    #[arg(long, env = "CONCORD_INIT_MEMBERS", default_value = "")]
    init_members: String,
    /// Create a hub admin (and print its token) when there is none.
    #[arg(long, env = "CONCORD_INIT_ADMIN", default_value_t = true, action = clap::ArgAction::Set)]
    init_admin: bool,
    /// Directory for invites (token + client config) of accounts created at start.
    #[arg(long, env = "CONCORD_INVITES_DIR")]
    invites_dir: Option<std::path::PathBuf>,
}

#[derive(Subcommand)]
enum NamespaceCmd {
    /// Create a namespace.
    Create {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "")]
        goal: String,
        /// Max offers per negotiation.
        #[arg(long, default_value_t = 4)]
        max_rounds: i32,
        /// Time each side gets to answer a proposal.
        #[arg(long, default_value = "30m")]
        turn_timeout: String,
        /// Time the humans get to settle an escalated negotiation.
        #[arg(long, default_value = "24h")]
        escalation_timeout: String,
        /// A claimed task goes back to the pool if its holder is silent this long.
        #[arg(long, default_value = "2h")]
        lease: String,
    },
    List,
}

#[derive(Subcommand)]
enum MemberCmd {
    /// Add a member to a namespace; creates the account (and prints its token) if it is new.
    Add {
        namespace: String,
        id: String,
        #[arg(long, default_value = "")]
        name: String,
        /// The person this agent works for.
        #[arg(long, default_value = "")]
        human: String,
        /// viewer, member, lead or admin. Default: member (lead if the namespace has no lead yet).
        #[arg(long)]
        role: Option<String>,
        /// Shortcut for --role lead.
        #[arg(long)]
        lead: bool,
        /// Area owned by this member: name or name=glob1,glob2. Repeatable.
        #[arg(long = "area")]
        areas: Vec<String>,
    },
    /// Remove a member from a namespace; their tasks go back to the pool.
    Remove {
        namespace: String,
        id: String,
    },
    List {
        namespace: String,
    },
}

#[derive(Subcommand)]
enum PrincipalCmd {
    /// Create an account with grants ("ns:role" or "*:role" for every namespace) and print its token.
    Add {
        id: String,
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long, default_value = "")]
        human: String,
        /// Hub admin: manages every namespace, account and token (and can sign in to the web UI as such).
        #[arg(long)]
        admin: bool,
        /// Access grant "namespace:role" or "*:role". Repeatable.
        #[arg(long = "grant")]
        grants: Vec<String>,
    },
    /// Give access: "namespace:role" or "*:role".
    Grant {
        id: String,
        grant: String,
    },
    /// Take access to one namespace (or "*") away.
    Revoke {
        id: String,
        namespace: String,
    },
    /// Deactivate an account: tokens revoked, memberships closed.
    Remove {
        id: String,
    },
    List,
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Issue another token for an account.
    Issue {
        principal: String,
        #[arg(long, default_value = "")]
        label: String,
    },
    /// Revoke a token by its id (tok_...).
    Revoke { token_id: String },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Add one task as the hub operator (opens directly in any area).
    Add {
        namespace: String,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long)]
        area: Option<String>,
        /// p0..p3
        #[arg(long)]
        priority: Option<String>,
        /// feature, bug, chore or followup
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        milestone: Option<String>,
    },
    /// Import tasks from a Markdown list, CSV or JSON file ("-" reads stdin).
    Import {
        namespace: String,
        file: String,
        /// auto, markdown, csv or json
        #[arg(long, default_value = "auto")]
        format: String,
        /// Only show what would be created.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ConnectorCmd {
    List {
        namespace: String,
    },
    /// Check the credential and settings.
    Test {
        namespace: String,
        id: String,
    },
    /// Pull changes from the tracker now.
    Sync {
        namespace: String,
        id: String,
    },
    /// Attach one issue (URL, key or number) as a task.
    Attach {
        namespace: String,
        id: String,
        reference: String,
    },
}

fn duration(s: &str) -> anyhow::Result<time::Duration> {
    parse_duration(s).map_err(anyhow::Error::msg)
}

async fn healthcheck(addr: SocketAddr) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut addr = addr;
    if addr.ip().is_unspecified() {
        addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let mut s = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr)).await??;
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut buf)).await??;
    if buf.starts_with(b"HTTP/1.1 200") {
        Ok(())
    } else {
        bail!("unhealthy: {}", String::from_utf8_lossy(&buf).lines().next().unwrap_or(""))
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split('/').next()?;
    (!authority.is_empty()).then(|| authority.to_string())
}

fn non_empty(v: &Option<String>) -> Option<String> {
    v.clone().filter(|s| !s.trim().is_empty())
}

fn print_connections(v: &serde_json::Value, token: &str) {
    println!("token (shown once): {token}\n");
    if let Some(list) = v["connections"].as_array() {
        for c in list {
            println!(
                "{} ({}):\n  {}\n",
                c["namespace"].as_str().unwrap_or(""),
                c["role"].as_str().unwrap_or(""),
                c["claude_code"].as_str().unwrap_or("")
            );
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn,rmcp=warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if let Cmd::Healthcheck { addr } = &cli.cmd {
        return healthcheck(*addr).await;
    }
    let url = cli.database_url.clone().context("set --database-url or DATABASE_URL")?;
    let max_conn = match &cli.cmd {
        Cmd::Serve(a) => a.db_max_connections,
        _ => 4,
    };
    let public = non_empty(&cli.public_url);
    let pool = concord_mcp::connect(&url, max_conn).await.context("connecting to Postgres")?;
    let mut hub = Hub::new(pool, Arc::new(SystemClock));
    if matches!(cli.cmd, Cmd::Serve(_) | Cmd::Connector(_)) {
        hub = hub.with_secrets(concord_mcp::connectors::crypto::SecretBox::load(
            cli.secret_key.as_deref(),
            &cli.secret_key_file,
        )?);
    }
    hub = match (&public, &cli.cmd) {
        (Some(p), _) => hub.with_public_url(p.clone()),
        (None, Cmd::Serve(a)) => hub.with_public_url(format!("http://{}", a.listen)),
        (None, _) => hub,
    };
    let hub = Arc::new(hub);
    hub.migrate().await?;
    let op = access::Actor::operator();

    match cli.cmd {
        Cmd::Migrate => println!("migrations applied"),
        Cmd::Healthcheck { .. } => unreachable!("handled before connecting"),
        Cmd::Serve(a) => {
            let mut hosts = a.allowed_hosts.clone();
            if let Some(h) = public.as_deref().and_then(host_of) {
                if let Some((name, _)) = h.split_once(':') {
                    hosts.push(name.to_string());
                }
                hosts.push(h);
            }
            if !hosts.is_empty() {
                hosts.extend(["localhost", "127.0.0.1"].map(String::from));
                hosts.push(a.listen.to_string());
            }
            let invites = a.invites_dir.clone().filter(|d| !d.as_os_str().is_empty());
            if a.init_admin {
                invite::ensure_admin(&hub, invites.as_deref()).await?;
            }
            let members = invite::parse_members(&a.init_members).map_err(anyhow::Error::msg)?;
            let init_namespace = non_empty(&a.init_namespace);
            if init_namespace.is_some() || !members.is_empty() {
                let namespace = init_namespace.unwrap_or_else(|| "main".into());
                let spec = invite::InitSpec {
                    name: non_empty(&a.init_name).unwrap_or_else(|| namespace.clone()),
                    namespace,
                    goal: a.init_goal.clone(),
                    members,
                    settings: admin::NamespaceSettings::default(),
                };
                invite::bootstrap(&hub, &spec, invites.as_deref()).await?;
            }
            let opts = ServeOptions {
                listen: a.listen,
                http: HttpConfig {
                    allowed_hosts: hosts,
                    allowed_origins: a.allowed_origins,
                    rate_per_sec: a.rate_limit,
                    burst: a.rate_burst,
                    secure_cookies: hub.public_url().starts_with("https://"),
                    ..HttpConfig::default()
                },
                sweep_every: Duration::from_secs(duration(&a.sweep_every)?.whole_seconds().max(1) as u64),
            };
            let cancel = CancellationToken::new();
            let c = cancel.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                tracing::info!("shutting down");
                c.cancel();
            });
            concord_mcp::serve(hub, opts, cancel).await?;
        }
        Cmd::Namespace(NamespaceCmd::Create {
            id,
            name,
            goal,
            max_rounds,
            turn_timeout,
            escalation_timeout,
            lease,
        }) => {
            let s = admin::NamespaceSettings {
                max_rounds,
                turn_timeout: duration(&turn_timeout)?,
                escalation_timeout: duration(&escalation_timeout)?,
                lease: duration(&lease)?,
            };
            admin::create_namespace(&hub, &id, name.as_deref().unwrap_or(&id), &goal, &s, "operator").await?;
            println!("namespace {id} created: {}/mcp/{id}", hub.public_url());
            println!("add people with: concord member add {id} <id> --human <name> --area <area>=<globs>");
        }
        Cmd::Namespace(NamespaceCmd::List) => {
            println!("{}", serde_json::to_string_pretty(&admin::list_namespaces(&hub).await?)?);
        }
        Cmd::Member(MemberCmd::Add { namespace, id, name, human, role, lead, areas }) => {
            let mut parsed = vec![];
            for a in areas {
                let (name, globs) = a.split_once('=').unwrap_or((a.as_str(), ""));
                let globs: Vec<String> =
                    globs.split(',').map(str::trim).filter(|g| !g.is_empty()).map(String::from).collect();
                parsed.push((name.trim().to_string(), globs));
            }
            let role = match role.as_deref() {
                Some(r) => Some(Role::parse(r).context("role must be viewer, member, lead or admin")?),
                None => None,
            };
            let token = admin::add_member(
                &hub,
                &namespace,
                admin::NewMember { id: id.clone(), display_name: name, human, lead, role, areas: parsed },
            )
            .await?;
            println!("{id} is a member of {namespace}");
            match token {
                Some(t) => print_connections(&access::connections(&hub, &id, &t).await?, &t),
                None => println!("existing account: its tokens now reach {}/mcp/{namespace}", hub.public_url()),
            }
        }
        Cmd::Member(MemberCmd::Remove { namespace, id }) => {
            println!("{}", serde_json::to_string_pretty(&admin::remove_member(&hub, &namespace, &id).await?)?);
        }
        Cmd::Member(MemberCmd::List { namespace }) => {
            println!("{}", serde_json::to_string_pretty(&admin::list_members(&hub, &namespace).await?)?);
        }
        Cmd::Principal(PrincipalCmd::Add { id, name, human, admin: is_admin, grants }) => {
            let grants: Vec<Grant> = grants.iter().map(|g| access::parse_grant(g)).collect::<Result<_, _>>()?;
            access::create_principal(
                &hub,
                &access::NewPrincipal { id: id.clone(), display_name: name, human, is_admin, grants },
            )
            .await?;
            let (_, t) = access::issue_token(&hub, &id, "first token", &op.principal).await?;
            println!("account {id} created{}", if is_admin { " (hub admin)" } else { "" });
            print_connections(&access::connections(&hub, &id, &t).await?, &t);
            if is_admin {
                println!("web UI: {}/ (sign in with the token)", hub.public_url());
            }
        }
        Cmd::Principal(PrincipalCmd::Grant { id, grant }) => {
            let g = access::parse_grant(&grant)?;
            access::set_grant(&hub, &id, &g.namespace, g.role).await?;
            println!("{}", serde_json::to_string_pretty(&access::principal_view(&hub.pool, &id).await?)?);
        }
        Cmd::Principal(PrincipalCmd::Revoke { id, namespace }) => {
            access::remove_grant(&hub, &id, &namespace).await?;
            println!("{}", serde_json::to_string_pretty(&access::principal_view(&hub.pool, &id).await?)?);
        }
        Cmd::Principal(PrincipalCmd::Remove { id }) => {
            access::deactivate(&hub, &id).await?;
            println!("{id} deactivated");
        }
        Cmd::Principal(PrincipalCmd::List) => {
            println!("{}", serde_json::to_string_pretty(&access::principals(&hub.pool).await?)?);
        }
        Cmd::Token(TokenCmd::Issue { principal, label }) => {
            let (id, t) = access::issue_token(&hub, &principal, &label, &op.principal).await?;
            println!("token id: {id}");
            print_connections(&access::connections(&hub, &principal, &t).await?, &t);
        }
        Cmd::Token(TokenCmd::Revoke { token_id }) => {
            let owner = access::revoke_token(&hub, &token_id).await?;
            println!("{token_id} of {owner} revoked");
        }
        Cmd::Task(TaskCmd::Add { namespace, title, description, area, priority, kind, milestone }) => {
            let task = NewTask {
                title,
                description,
                area,
                milestone,
                priority: match priority.as_deref() {
                    Some(p) => Some(Priority::parse(p).context("priority must be p0..p3")?),
                    None => None,
                },
                kind: match kind.as_deref() {
                    Some(k) => Some(TaskKind::parse(k).context("kind must be feature, bug, chore or followup")?),
                    None => None,
                },
                ..NewTask::default()
            };
            let v = hub
                .write(&namespace, "operator", move |op| {
                    Box::pin(async move { tasks::create_many(op, "operator", Role::Admin, vec![task], "").await })
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Task(TaskCmd::Import { namespace, file, format, dry_run }) => {
            let text = if file == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?
            };
            let format: import::Format = serde_json::from_value(serde_json::json!(format))
                .context("format must be auto, markdown, csv or json")?;
            let parsed = import::parse(format, &text)?;
            for w in &parsed.warnings {
                eprintln!("warning: {w}");
            }
            if dry_run {
                println!("{}", serde_json::to_string_pretty(&parsed.tasks)?);
                return Ok(());
            }
            let v = hub
                .write(&namespace, "operator", move |op| {
                    Box::pin(async move { tasks::create_many(op, "operator", Role::Admin, parsed.tasks, "").await })
                })
                .await?;
            println!("{} task(s) created in {namespace}", v["count"]);
        }
        Cmd::Connector(ConnectorCmd::List { namespace }) => {
            let list = concord_mcp::connectors::engine::list(&hub.pool, &namespace).await?;
            println!("{}", serde_json::to_string_pretty(&list)?);
        }
        Cmd::Connector(ConnectorCmd::Test { namespace, id }) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&concord_mcp::connectors::engine::test(&hub, &namespace, &id).await?)?
            );
        }
        Cmd::Connector(ConnectorCmd::Sync { namespace, id }) => {
            let v = concord_mcp::connectors::engine::sync_one(&hub, &namespace, &id, true).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Connector(ConnectorCmd::Attach { namespace, id, reference }) => {
            let v = concord_mcp::connectors::engine::attach(&hub, &namespace, &id, &reference, "operator").await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Status { namespace } => {
            let mut conn = hub.pool.acquire().await?;
            let p = plan::load(&mut conn, &namespace, false).await?;
            print!("{}", plan::to_markdown(&p));
        }
        Cmd::Resolve { namespace, negotiation, round, status_quo, note } => {
            if round.is_none() && !status_quo {
                bail!("pass --round <n> to apply an offer or --status-quo to change nothing");
            }
            let v = admin::resolve(&hub, &namespace, &negotiation, round, &note, "operator").await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

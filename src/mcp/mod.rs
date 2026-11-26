//! MCP server: tools, resources and prompts on top of the hub.

use std::sync::Arc;
use std::time::Duration;

use http::request::Parts;
use rmcp::handler::server::tool::{Extension, ToolRouter};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, GetPromptRequestParams, GetPromptResponse, GetPromptResult, Implementation, InitializeResult,
    ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, Prompt,
    PromptArgument, PromptMessage, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, Role as MsgRole, ServerCapabilities,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::{ElicitationMode, RequestContext};
use rmcp::{ErrorData, Peer, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::access::{self, Actor};
use crate::error::{HubError, Result as HubResult};
use crate::hub::Hub;
use crate::model::{Caller, Change, NegotiationStatus, Role, TaskPatch};
use crate::ops::{
    self,
    negotiate::Action,
    plan::PlanOp,
    tasks::{CreateTask, StatusAction, TaskFilter},
};

pub const INSTRUCTIONS: &str = include_str!("instructions.md");

#[derive(Clone)]
pub struct ConcordServer {
    hub: Arc<Hub>,
    tool_router: ToolRouter<Self>,
}

fn caller_of(parts: &Parts) -> Result<Caller, ErrorData> {
    parts.extensions.get::<Caller>().cloned().ok_or_else(|| ErrorData::invalid_request("unauthenticated", None))
}

fn caller_from_ctx(ctx: &RequestContext<RoleServer>) -> Result<Caller, ErrorData> {
    ctx.extensions
        .get::<Parts>()
        .map(caller_of)
        .unwrap_or_else(|| Err(ErrorData::invalid_request("unauthenticated", None)))
}

fn deny_viewer(c: &Caller) -> Option<CallToolResult> {
    (!c.role.can_write()).then(|| {
        CallToolResult::structured_error(
            HubError::forbidden("you are a viewer in this namespace (read-only)").to_json(),
        )
    })
}

fn reply(r: HubResult<Value>) -> Result<CallToolResult, ErrorData> {
    Ok(match r {
        Ok(v) => CallToolResult::structured(v),
        Err(e) => CallToolResult::structured_error(e.to_json()),
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SyncParams {
    /// The `cursor` from your previous sync. Omit on the first call.
    #[serde(default)]
    pub since: Option<i64>,
    /// Max events to return (default 100).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitParams {
    /// The `cursor` from your last sync.
    pub since: i64,
    /// How long to wait, 1-55 seconds (default 25).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanFormat {
    #[default]
    Json,
    Markdown,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPlanParams {
    #[serde(default)]
    pub format: PlanFormat,
    /// Also list cancelled and rejected tasks.
    #[serde(default)]
    pub include_closed: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdatePlanParams {
    /// Changes, applied in order. What needs others' consent is bundled into one proposal.
    pub ops: Vec<PlanOp>,
    /// If set, fail when the plan changed since you read it.
    #[serde(default)]
    pub expected_plan_version: Option<i64>,
    /// Why; shown to whoever has to consent.
    #[serde(default)]
    pub rationale: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskIdParams {
    pub task_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NextTaskParams {
    /// Claim the best candidate atomically (recommended).
    #[serde(default)]
    pub claim: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateTaskParams {
    pub task_id: String,
    /// The task `version` you read; the edit fails if someone changed it meanwhile.
    pub expected_version: i64,
    pub patch: TaskPatch,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetStatusParams {
    pub task_id: String,
    pub action: StatusAction,
    /// Progress note, block reason, review feedback...
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub expected_version: Option<i64>,
    /// start even though dependencies are unfinished.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposeParams {
    /// Changes applied together if accepted.
    pub changes: Vec<Change>,
    /// Why; shown to the others.
    #[serde(default)]
    pub rationale: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RespondParams {
    pub negotiation_id: String,
    /// The round you are answering (the `round` of the current offer).
    pub round: i32,
    pub action: Action,
    /// For counter: your alternative offer.
    #[serde(default)]
    pub changes: Option<Vec<Change>>,
    /// Reason (required for reject).
    #[serde(default)]
    pub note: String,
    /// Only after your human made this decision (required while escalated).
    #[serde(default)]
    pub human_confirmed: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImportParams {
    /// The tasks as text: Markdown list, CSV (with a `title` column) or a JSON array of tasks.
    pub text: String,
    #[serde(default)]
    pub format: ops::import::Format,
    /// Only parse and show what would be created.
    #[serde(default)]
    pub dry_run: bool,
    /// Why these tasks are needed; shown to area owners who have to accept them.
    #[serde(default)]
    pub rationale: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListNegotiationsParams {
    /// Get one negotiation with its full history of offers.
    #[serde(default)]
    pub negotiation_id: Option<String>,
    #[serde(default)]
    pub status: ops::negotiate::NegotiationFilter,
    /// Only negotiations you take part in.
    #[serde(default)]
    pub mine: bool,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageParams {
    /// Member id; omit to write to everyone.
    #[serde(default)]
    pub to: Option<String>,
    pub text: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub negotiation_id: Option<String>,
}

/// Form shown to a human when an escalated negotiation needs their decision.
/// Elicitation forms allow only flat primitive fields.
#[derive(Debug, Deserialize, serde::Serialize, JsonSchema)]
pub struct HumanDecision {
    /// Accept the offer? (unchecked = reject)
    pub accept: bool,
    /// Optional comment for the other side
    #[serde(default)]
    pub comment: String,
}

rmcp::elicit_safe!(HumanDecision);

#[tool_router]
impl ConcordServer {
    pub fn new(hub: Arc<Hub>) -> Self {
        ConcordServer { hub, tool_router: Self::tool_router() }
    }

    async fn read<T, F>(&self, f: F) -> HubResult<T>
    where
        F: for<'a> FnOnce(&'a mut sqlx::PgConnection) -> futures::future::BoxFuture<'a, HubResult<T>>,
    {
        let mut conn = self.hub.pool.acquire().await?;
        f(&mut conn).await
    }

    #[tool(
        description = "Who you are in this namespace: your member id, role and areas, the team with their load, the goal, the negotiation rules and the current event cursor.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn whoami(&self, Extension(parts): Extension<Parts>) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        let actor = parts.extensions.get::<Actor>().cloned();
        let hub = self.hub.clone();
        reply(
            self.read(move |conn| {
                Box::pin(async move {
                    let mut v = ops::plan::whoami(conn, &c.namespace, &c.member).await?;
                    v["your_role"] = json!(c.role);
                    if let Some(a) = actor {
                        let base = hub.public_url().to_string();
                        let list: Vec<Value> = access::namespaces_of(&hub.pool, &a)
                            .await?
                            .into_iter()
                            .map(|n| json!({ "id": n.id, "name": n.name, "role": n.role, "mcp_url": format!("{base}/mcp/{}", n.id) }))
                            .collect();
                        v["your_namespaces"] = json!(list);
                    }
                    Ok(v)
                })
            })
            .await,
        )
    }

    #[tool(
        description = "What changed since `since` and what needs you now: negotiations waiting for your answer, escalations for your human, your tasks, reviews, team load and hints. Call it at the start of every work session and before picking new work. Keeps your task leases alive.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn sync(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SyncParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        reply(ops::sync::sync(&self.hub, &c, p.since, p.limit).await)
    }

    #[tool(
        description = "Wait (up to timeout_secs) until another member changes something you can see, then return the same result as sync. Use it instead of polling when you are waiting for an answer.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn wait_for_updates(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<WaitParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        reply(ops::sync::wait(&self.hub, &c, p.since, p.timeout_secs).await)
    }

    #[tool(
        description = "The shared plan: goal, team, areas (who owns what), milestones with their tasks, open negotiations and recent decisions. format=markdown gives a PLAN.md-style document.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_plan(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<GetPlanParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        let include_closed = p.include_closed;
        let data = self
            .read(move |conn| Box::pin(async move { ops::plan::load(conn, &c.namespace, include_closed).await }))
            .await;
        reply(data.map(|d| match p.format {
            PlanFormat::Json => ops::plan::to_json(&d),
            PlanFormat::Markdown => {
                json!({ "markdown": ops::plan::to_markdown(&d), "plan_version": d.ns.plan_version })
            }
        }))
    }

    #[tool(
        description = "Change the plan: set_goal, add_milestone, update_milestone, remove_milestone, set_area (create an area, change its paths/owner), add_note (decision log). What you may change alone is applied now; the rest becomes one proposal for the members who must consent.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn update_plan(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdatePlanParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        if let Some(r) = deny_viewer(&c) {
            return Ok(r);
        }
        let me = c.member.clone();
        reply(
            self.hub
                .write(&c.namespace, &c.member, move |op| {
                    Box::pin(
                        async move { ops::plan::update(op, &me, p.expected_plan_version, p.ops, &p.rationale).await },
                    )
                })
                .await,
        )
    }

    #[tool(
        description = "List tasks with filters (status, area, assignee, milestone, kind, mine). Default: everything not closed. Descriptions are shortened; use get_task for details.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_tasks(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(f): Parameters<TaskFilter>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        reply(
            self.read(|conn| Box::pin(async move { ops::tasks::list(conn, &c.namespace, &c.member, &f).await })).await,
        )
    }

    #[tool(
        description = "One task in full: text, status, version, unfinished dependencies, negotiations about it and its history.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_task(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        reply(
            self.read(|conn| Box::pin(async move { ops::tasks::get(conn, &c.namespace, &c.member, &p.task_id).await }))
                .await,
        )
    }

    #[tool(
        description = "Pick what to work on next: the best open task with no unfinished dependencies (your area first, then shared, then others'; by priority and milestone order). With claim=true it is claimed for you atomically, so two agents never get the same task.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn next_task(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NextTaskParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        let me = c.member.clone();
        if p.claim
            && let Some(r) = deny_viewer(&c)
        {
            return Ok(r);
        }
        if !p.claim {
            return reply(
                self.read(|conn| {
                    Box::pin(async move {
                        let active = ops::tasks::active_of(conn, &c.namespace, &me).await?;
                        let cands = ops::tasks::candidates(conn, &c.namespace, &me).await?;
                        Ok(next_view(&active, &cands, None))
                    })
                })
                .await,
            );
        }
        reply(
            self.hub
                .write(&c.namespace, &c.member, move |op| {
                    Box::pin(async move {
                        let ns = op.ns.id.clone();
                        let active = ops::tasks::active_of(&mut op.tx, &ns, &me).await?;
                        let cands = ops::tasks::candidates(&mut op.tx, &ns, &me).await?;
                        for (t, _) in &cands {
                            match ops::tasks::claim(op, &me, &t.id).await {
                                Ok(v) => return Ok(next_view(&active, &cands, Some(v))),
                                Err(HubError::Conflict { .. }) => continue,
                                Err(e) => return Err(e),
                            }
                        }
                        Ok(next_view(&active, &[], None))
                    })
                })
                .await,
        )
    }

    #[tool(
        description = "Add a task. In your own area (or a shared one) it is created at once. In someone else's area — e.g. a bug you found in their part (kind=bug, paths, repro, evidence) — it is filed as proposed and its owner accepts, counters or rejects (silence = consent). claim=true takes it yourself; assignee=<member> suggests someone (they consent).",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn create_task(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<CreateTask>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        if let Some(r) = deny_viewer(&c) {
            return Ok(r);
        }
        let me = c.member.clone();
        reply(
            self.hub
                .write(&c.namespace, &c.member, move |op| Box::pin(async move { ops::tasks::create(op, &me, p).await }))
                .await,
        )
    }

    #[tool(
        description = "Edit a task (title, description, acceptance, area, milestone, kind, priority, depends_on). Pass the version you read. If the area owner or the assignee is someone else, this becomes a proposal they answer.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn update_task(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdateTaskParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        if let Some(r) = deny_viewer(&c) {
            return Ok(r);
        }
        let me = c.member.clone();
        reply(
            self.hub
                .write(&c.namespace, &c.member, move |op| {
                    Box::pin(async move {
                        ops::tasks::update(op, &me, &p.task_id, p.expected_version, p.patch, &p.rationale).await
                    })
                })
                .await,
        )
    }

    #[tool(
        description = "Claim an open task for yourself (exclusive, with a lease). If someone else holds it you get a conflict with the holder and a hint to ask for a handoff.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn claim_task(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TaskIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        if let Some(r) = deny_viewer(&c) {
            return Ok(r);
        }
        let me = c.member.clone();
        reply(
            self.hub
                .write(&c.namespace, &c.member, move |op| {
                    Box::pin(async move { ops::tasks::claim(op, &me, &p.task_id).await })
                })
                .await,
        )
    }

    #[tool(
        description = "Move a task through its lifecycle: start, progress (note; renews your lease), submit (for review), approve / request_changes (reviewers), done, block (note), unblock, release (give it back), reopen.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn set_task_status(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SetStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        if let Some(r) = deny_viewer(&c) {
            return Ok(r);
        }
        let me = c.member.clone();
        reply(
            self.hub
                .write(&c.namespace, &c.member, move |op| {
                    Box::pin(async move {
                        ops::tasks::set_status(op, &me, &p.task_id, p.action, &p.note, p.expected_version, p.force)
                            .await
                    })
                })
                .await,
        )
    }

    #[tool(
        description = "Propose changes that need others' consent, applied atomically if accepted: assign (hand off / ask for / distribute tasks), split, cancel_task, update_task, accept_task, set_goal, add/update/remove_milestone, set_area. If nobody else has to consent it is applied at once. The answer tells you who must respond, the deadline and the rule used without agreement.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn propose(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ProposeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        if let Some(r) = deny_viewer(&c) {
            return Ok(r);
        }
        let me = c.member.clone();
        reply(
            self.hub
                .write(&c.namespace, &c.member, move |op| {
                    Box::pin(async move { ops::negotiate::propose_or_apply(op, &me, p.changes, &p.rationale).await })
                })
                .await,
        )
    }

    #[tool(
        description = "Answer a negotiation: accept, reject (with a reason), counter (your alternative `changes`), or withdraw (proposer). Pass the current `round`. While escalated, answer only with your human's decision and human_confirmed=true; if your client supports forms the hub asks your human directly.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn respond(
        &self,
        Extension(parts): Extension<Parts>,
        peer: Peer<RoleServer>,
        Parameters(mut p): Parameters<RespondParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        if let Some(r) = deny_viewer(&c) {
            return Ok(r);
        }
        if !p.human_confirmed
            && p.action != Action::Withdraw
            && let Some(d) = self.ask_human(&c, &peer, &p.negotiation_id).await
        {
            p.action = if d.accept { Action::Accept } else { Action::Reject };
            p.human_confirmed = true;
            p.changes = None;
            if !d.comment.trim().is_empty() {
                p.note = d.comment;
            } else if p.action == Action::Reject && p.note.trim().is_empty() {
                p.note = "rejected by my human".into();
            }
        }
        let me = c.member.clone();
        reply(
            self.hub
                .write(&c.namespace, &c.member, move |op| {
                    Box::pin(async move {
                        ops::negotiate::respond(
                            op,
                            &me,
                            &p.negotiation_id,
                            p.round,
                            p.action,
                            p.changes,
                            &p.note,
                            p.human_confirmed,
                        )
                        .await
                    })
                })
                .await,
        )
    }

    #[tool(
        description = "Add many tasks at once from text your human gave you: a Markdown list (`- [ ] Title`, indented description, tags #bug #p1 @area !M-2), CSV with a title column, or a JSON array. Same rules as create_task for each task (someone else's area becomes a proposal). Use dry_run=true to preview.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn import_tasks(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ImportParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        if let Some(r) = deny_viewer(&c) {
            return Ok(r);
        }
        let parsed = match ops::import::parse(p.format, &p.text) {
            Ok(x) => x,
            Err(e) => return reply(Err(e)),
        };
        if p.dry_run {
            return reply(Ok(json!({ "dry_run": true, "tasks": parsed.tasks, "warnings": parsed.warnings })));
        }
        let me = c.member.clone();
        // Agents follow the agent rules even if their principal is a lead: only humans plan directly.
        let role = if c.role.can_write() { Role::Member } else { c.role };
        let warnings = parsed.warnings.clone();
        let out = self
            .hub
            .write(&c.namespace, &c.member, move |op| {
                Box::pin(async move { ops::tasks::create_many(op, &me, role, parsed.tasks, &p.rationale).await })
            })
            .await
            .map(|mut v| {
                v["warnings"] = json!(warnings);
                v
            });
        reply(out)
    }

    #[tool(
        description = "List negotiations (default: open and escalated) or get one with its full offer history (negotiation_id).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_negotiations(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ListNegotiationsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        reply(
            self.read(|conn| {
                Box::pin(async move {
                    ops::negotiate::list(
                        conn,
                        &c.namespace,
                        &c.member,
                        p.negotiation_id.as_deref(),
                        p.status,
                        p.mine,
                        p.limit.unwrap_or(50),
                    )
                    .await
                })
            })
            .await,
        )
    }

    #[tool(
        description = "Send a short message to one member (to) or everyone. For coordination and questions; agreements about work go through tasks and proposals.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn send_message(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SendMessageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let c = caller_of(&parts)?;
        if let Some(r) = deny_viewer(&c) {
            return Ok(r);
        }
        let me = c.member.clone();
        reply(
            self.hub
                .write(&c.namespace, &c.member, move |op| {
                    Box::pin(async move {
                        ops::sync::send_message(
                            op,
                            &me,
                            p.to.as_deref(),
                            &p.text,
                            p.task_id.as_deref(),
                            p.negotiation_id.as_deref(),
                        )
                        .await
                    })
                })
                .await,
        )
    }
}

impl ConcordServer {
    /// For an escalated negotiation that waits for this member, asks the human directly
    /// through a form when the client supports it. Any failure falls back to the agent.
    async fn ask_human(&self, c: &Caller, peer: &Peer<RoleServer>, id: &str) -> Option<HumanDecision> {
        if !peer.supported_elicitation_modes().contains(&ElicitationMode::Form) {
            tracing::debug!(member = %c.member, "client cannot show forms; the agent has to ask its human");
            return None;
        }
        let (cc, nid) = (c.clone(), id.to_string());
        let view = self
            .read(move |conn| {
                Box::pin(async move {
                    ops::negotiate::list(conn, &cc.namespace, &cc.member, Some(&nid), Default::default(), false, 1)
                        .await
                })
            })
            .await
            .ok()?;
        let n = &view["negotiation"];
        if n["status"].as_str() != Some(NegotiationStatus::Escalated.as_str()) || n["you_must_answer"] != json!(true) {
            return None;
        }
        let offer = &n["current_offer"];
        let changes: Vec<Change> = serde_json::from_value(offer["changes"].clone()).ok()?;
        let message = format!(
            "Your agent's team cannot agree and needs your decision.\n\n{} (by {}), round {}:\n{}\n\nWhy it was escalated: {}\n\nAccept this change?",
            n["id"].as_str().unwrap_or(id),
            offer["author"].as_str().unwrap_or("?"),
            offer["round"],
            ops::negotiate::describe(&changes),
            n["rule"].as_str().unwrap_or(""),
        );
        match peer.elicit_with_timeout::<HumanDecision>(message, Some(Duration::from_secs(300))).await {
            Ok(Some(d)) => Some(d),
            Ok(None) => None,
            Err(e) => {
                tracing::debug!(member = %c.member, error = %e, "asking the human through a form failed");
                None
            }
        }
    }
}

fn next_view(
    active: &[crate::model::Task],
    cands: &[(crate::model::Task, &'static str)],
    claimed: Option<Value>,
) -> Value {
    let suggestions: Vec<Value> = cands
        .iter()
        .take(5)
        .map(|(t, why)| {
            let mut v = ops::tasks::brief(t);
            v["why"] = json!(why);
            v
        })
        .collect();
    let hint = match (&claimed, suggestions.is_empty()) {
        (Some(_), _) => "Claimed. Start it with set_task_status(action=start).",
        (None, true) => {
            "Nothing is ready. Look for missing work (create_task), review others' tasks, or wait_for_updates."
        }
        (None, false) => "Claim one with claim_task, or call next_task(claim=true).",
    };
    json!({
        "your_active_tasks": active.iter().map(ops::tasks::brief).collect::<Vec<_>>(),
        "claimed": claimed,
        "suggestions": suggestions,
        "hint": hint,
    })
}

const KICKOFF: &str = "You are starting (or joining) a shared project with agents of other people through concord.\n\
1. Call whoami and get_plan.\n\
2. If the goal is empty, set it with update_plan (set_goal) from what your human told you.\n\
3. Make sure every member has an area matching their human's part: update_plan set_area with path globs (your own area directly; giving an area to someone else is a proposal they accept).\n\
4. Add milestones, then the tasks for your area. For partners' areas, file the tasks you know they need with create_task; they accept or adjust them.\n\
5. Distribute: take your next task with next_task(claim=true); suggest work for others with propose (assign).\n\
6. Send everyone a short summary with send_message, then follow the protocol: sync, claim, report, answer negotiations.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ConcordServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().enable_resources().enable_prompts().build())
            .with_server_info(Implementation::new("concord-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new("concord://plan", "plan")
                .with_title("Shared plan (markdown)")
                .with_mime_type("text/markdown"),
            Resource::new("concord://inbox", "inbox")
                .with_title("What needs your attention (same as sync)")
                .with_mime_type("application/json"),
        ]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("concord://tasks/{task_id}", "task")
                .with_title("One task with history")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let c = caller_from_ctx(&ctx)?;
        let uri = request.uri.clone();
        let body: HubResult<(String, &'static str)> = if uri == "concord://plan" {
            self.read(move |conn| Box::pin(async move { ops::plan::load(conn, &c.namespace, false).await }))
                .await
                .map(|p| (ops::plan::to_markdown(&p), "text/markdown"))
        } else if uri == "concord://inbox" {
            ops::sync::sync(&self.hub, &c, None, Some(20)).await.map(|v| (v.to_string(), "application/json"))
        } else if let Some(id) = uri.strip_prefix("concord://tasks/") {
            let id = id.to_string();
            self.read(|conn| Box::pin(async move { ops::tasks::get(conn, &c.namespace, &c.member, &id).await }))
                .await
                .map(|v| (v.to_string(), "application/json"))
        } else {
            return Err(ErrorData::resource_not_found(format!("unknown resource {uri}"), None));
        };
        match body {
            Ok((text, mime)) => {
                Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri).with_mime_type(mime)]).into())
            }
            Err(HubError::NotFound(m)) => Err(ErrorData::resource_not_found(m, None)),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![
            Prompt::new("concord_protocol", Some("How to work with other agents through concord"), None),
            Prompt::new(
                "kickoff",
                Some("Set up the shared plan with your partners: goal, areas, milestones, first tasks"),
                Some(vec![
                    PromptArgument::new("goal").with_description("What the humans want to build").with_required(false),
                ]),
            ),
        ]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let text = match request.name.as_str() {
            "concord_protocol" => INSTRUCTIONS.to_string(),
            "kickoff" => {
                let goal = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("goal"))
                    .and_then(|g| g.as_str())
                    .map(|g| format!("\n\nThe goal from your human: {g}"))
                    .unwrap_or_default();
                format!("{KICKOFF}{goal}\n\nProtocol:\n{INSTRUCTIONS}")
            }
            other => return Err(ErrorData::invalid_params(format!("unknown prompt {other}"), None)),
        };
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(MsgRole::User, text)]).into())
    }
}

//! Data model shared by the hub, the MCP layer and the CLI.

use std::collections::BTreeMap;

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize, Serializer};
use sqlx::types::Json;
use time::OffsetDateTime;

use crate::clock::rfc3339;

macro_rules! str_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $var:ident => $s:literal),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($(#[$vmeta])* $var),* }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$var),*];
            pub fn as_str(&self) -> &'static str {
                match self { $($name::$var => $s),* }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some($name::$var),)* _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

str_enum!(
    /// Lifecycle of a task.
    TaskStatus {
        /// Filed into someone else's area; waits for the owner's acceptance.
        Proposed => "proposed",
        Open => "open",
        Claimed => "claimed",
        InProgress => "in_progress",
        Review => "review",
        Blocked => "blocked",
        Done => "done",
        Cancelled => "cancelled",
        /// The owner declined a proposed task.
        Rejected => "rejected",
    }
);

impl TaskStatus {
    /// Statuses that hold an assignee and a lease.
    pub fn is_held(&self) -> bool {
        matches!(self, TaskStatus::Claimed | TaskStatus::InProgress | TaskStatus::Blocked | TaskStatus::Review)
    }
    pub fn is_closed(&self) -> bool {
        matches!(self, TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::Rejected)
    }
    /// A dependency in one of these statuses no longer blocks anything.
    pub fn resolves_dependency(&self) -> bool {
        matches!(self, TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::Rejected)
    }
}

str_enum!(
    TaskKind {
        Feature => "feature",
        Bug => "bug",
        Chore => "chore",
        Followup => "followup",
    }
);

str_enum!(
    Priority {
        P0 => "p0",
        P1 => "p1",
        P2 => "p2",
        P3 => "p3",
    }
);

str_enum!(
    /// Role of a principal inside one namespace.
    Role {
        /// Reads everything, changes nothing, never a party to a negotiation.
        Viewer => "viewer",
        /// Works on tasks, files proposals, answers negotiations.
        Member => "member",
        /// Also approves plan-level changes (goal, removing milestones, re-owning areas).
        Lead => "lead",
        /// Also manages the namespace: settings, members and their access, tracker connectors.
        Admin => "admin",
    }
);

impl Role {
    pub fn rank(&self) -> u8 {
        match self {
            Role::Viewer => 0,
            Role::Member => 1,
            Role::Lead => 2,
            Role::Admin => 3,
        }
    }
    pub fn can_write(&self) -> bool {
        self.rank() >= Role::Member.rank()
    }
    pub fn approves_plan(&self) -> bool {
        self.rank() >= Role::Lead.rank()
    }
    pub fn manages(&self) -> bool {
        *self == Role::Admin
    }
}

str_enum!(
    /// How a disagreement over a proposal is settled when the parties do not agree in time.
    Risk {
        /// Settled automatically: silence counts as consent, then area owner, then the
        /// current assignee, then the status quo.
        Safe => "safe",
        /// Never applied by silence or by rule: goes to the humans behind the agents.
        Destructive => "destructive",
    }
);

impl Risk {
    pub fn max(self, other: Risk) -> Risk {
        if self == Risk::Destructive || other == Risk::Destructive { Risk::Destructive } else { Risk::Safe }
    }
}

str_enum!(
    NegotiationStatus {
        Open => "open",
        Escalated => "escalated",
        Closed => "closed",
    }
);

str_enum!(
    Outcome {
        /// Every participant accepted the same offer.
        Agreed => "agreed",
        /// Safe proposal, nobody objected before the deadline.
        Silence => "silence",
        /// Safe proposal, objections remained; decided by owner > assignee > status quo.
        Policy => "policy",
        /// Closed without applying anything.
        StatusQuo => "status_quo",
        Withdrawn => "withdrawn",
        /// The agreed offer no longer applied to the current state.
        Stale => "stale",
        /// Escalated proposal settled by the humans of every participant.
        Humans => "humans",
        /// Settled by the hub operator.
        Admin => "admin",
        /// Escalated and nobody decided in time: nothing changes.
        EscalationTimeout => "escalation_timeout",
    }
);

str_enum!(
    MilestoneStatus {
        Open => "open",
        Done => "done",
    }
);

pub fn ser_ts<S: Serializer>(t: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&rfc3339(*t))
}

pub fn ser_opt_ts<S: Serializer>(t: &Option<OffsetDateTime>, s: S) -> Result<S::Ok, S::Error> {
    match t {
        Some(t) => s.serialize_str(&rfc3339(*t)),
        None => s.serialize_none(),
    }
}

/// The authenticated caller of an MCP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub namespace: String,
    pub member: String,
    pub role: Role,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Namespace {
    pub id: String,
    pub name: String,
    pub goal: String,
    pub plan_version: i64,
    pub event_seq: i64,
    pub next_task_no: i64,
    pub next_negotiation_no: i64,
    pub next_milestone_no: i64,
    pub max_rounds: i32,
    pub turn_timeout_secs: i64,
    pub escalation_timeout_secs: i64,
    pub lease_secs: i64,
    pub created_at: OffsetDateTime,
}

impl Namespace {
    pub fn lease(&self) -> time::Duration {
        time::Duration::seconds(self.lease_secs)
    }
    pub fn turn_timeout(&self) -> time::Duration {
        time::Duration::seconds(self.turn_timeout_secs)
    }
    pub fn escalation_timeout(&self) -> time::Duration {
        time::Duration::seconds(self.escalation_timeout_secs)
    }
    pub fn policies(&self) -> serde_json::Value {
        serde_json::json!({
            "max_rounds": self.max_rounds,
            "turn_timeout_secs": self.turn_timeout_secs,
            "escalation_timeout_secs": self.escalation_timeout_secs,
            "lease_secs": self.lease_secs,
        })
    }
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Member {
    pub id: String,
    pub display_name: String,
    pub human: String,
    pub role: String,
    pub active: bool,
    #[serde(serialize_with = "ser_opt_ts")]
    pub last_seen_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Area {
    pub name: String,
    pub owner: Option<String>,
    pub paths: Json<Vec<String>>,
    pub description: String,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Milestone {
    pub id: String,
    pub title: String,
    pub description: String,
    pub position: i64,
    pub status: String,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub description: String,
    pub acceptance: String,
    pub area: Option<String>,
    pub milestone: Option<String>,
    pub kind: String,
    pub priority: String,
    pub status: String,
    pub assignee: Option<String>,
    pub depends_on: Json<Vec<String>>,
    pub created_by: String,
    pub version: i64,
    #[serde(serialize_with = "ser_opt_ts")]
    pub lease_until: Option<OffsetDateTime>,
    pub blocked_reason: Option<String>,
    #[serde(serialize_with = "ser_ts")]
    pub created_at: OffsetDateTime,
    #[serde(serialize_with = "ser_ts")]
    pub updated_at: OffsetDateTime,
}

impl Task {
    pub fn status(&self) -> TaskStatus {
        TaskStatus::parse(&self.status).unwrap_or(TaskStatus::Open)
    }
    /// Numeric part of `T-<n>`, used for stable ordering.
    pub fn number(&self) -> i64 {
        self.id.trim_start_matches("T-").parse().unwrap_or(i64::MAX)
    }
}

/// One participant's position inside a negotiation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stance {
    /// Round the `stance` refers to; a stance for an older round no longer counts.
    pub round: i32,
    /// "accept" or "reject" for `round`, if the participant answered it.
    pub stance: Option<String>,
    /// The answer was confirmed by the participant's human.
    pub human_confirmed: bool,
    /// Latest round this participant authored or accepted. `None` = prefers the status quo.
    pub position: Option<i32>,
}

impl Stance {
    pub fn on(&self, round: i32) -> Option<&str> {
        if self.round == round { self.stance.as_deref() } else { None }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Negotiation {
    pub id: String,
    pub status: String,
    pub outcome: Option<String>,
    pub applied_round: Option<i32>,
    pub explanation: String,
    pub risk: String,
    pub proposer: String,
    pub participants: Json<Vec<String>>,
    pub targets: Json<Vec<String>>,
    pub owner: Option<String>,
    pub incumbent: Option<String>,
    pub round: i32,
    pub max_rounds: i32,
    pub stances: Json<BTreeMap<String, Stance>>,
    pub deadline: OffsetDateTime,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub closed_at: Option<OffsetDateTime>,
}

impl Negotiation {
    pub fn status(&self) -> NegotiationStatus {
        NegotiationStatus::parse(&self.status).unwrap_or(NegotiationStatus::Closed)
    }
    pub fn risk(&self) -> Risk {
        Risk::parse(&self.risk).unwrap_or(Risk::Destructive)
    }
    /// Participants that still have to answer the current round.
    pub fn awaiting(&self) -> Vec<String> {
        if self.status() == NegotiationStatus::Closed {
            return vec![];
        }
        let escalated = self.status() == NegotiationStatus::Escalated;
        self.participants
            .iter()
            .filter(|m| match self.stances.get(*m) {
                None => true,
                Some(s) => s.on(self.round).is_none() || (escalated && !s.human_confirmed),
            })
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Offer {
    pub round: i32,
    pub author: String,
    pub changes: Json<Vec<Change>>,
    pub risk: String,
    pub note: String,
    #[serde(serialize_with = "ser_ts")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Event {
    pub seq: i64,
    #[serde(serialize_with = "ser_ts")]
    pub at: OffsetDateTime,
    pub actor: String,
    pub kind: String,
    pub entity: Option<String>,
    pub summary: String,
    pub data: Json<serde_json::Value>,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Decision {
    pub seq: i64,
    pub negotiation_id: Option<String>,
    pub outcome: String,
    pub summary: String,
    pub actor: String,
    #[serde(serialize_with = "ser_ts")]
    pub created_at: OffsetDateTime,
}

/// A task to create (inside a split, or via `create_task`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NewTask {
    /// Short imperative title, e.g. "Add rate limiting to /login".
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// What must be true for the task to count as done.
    #[serde(default)]
    pub acceptance: String,
    /// Area name. If omitted, inferred from `paths`; if still unknown the task is shared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// Files or directories the task touches; used to infer the area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TaskKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    /// Ids of tasks that must be finished first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Member who will do it. Needs that member's consent unless it is you.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
}

/// Fields to change on a task. Omitted fields stay as they are; an empty string clears
/// `area` or `milestone`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TaskPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TaskKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
}

impl TaskPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.acceptance.is_none()
            && self.area.is_none()
            && self.milestone.is_none()
            && self.kind.is_none()
            && self.priority.is_none()
            && self.depends_on.is_none()
    }
    pub fn touches_content(&self) -> bool {
        self.title.is_some() || self.description.is_some() || self.acceptance.is_some()
    }
}

/// One change to the shared plan. A proposal is a list of changes applied atomically.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    /// Accept a task that was proposed into an area (status proposed -> open).
    AcceptTask { task_id: String },
    /// Edit a task.
    UpdateTask { task_id: String, patch: TaskPatch },
    /// Give a task to a member: hand off your task, ask for someone's task, or distribute work.
    Assign { task_id: String, to: String },
    /// Replace a task with several smaller tasks (the original is cancelled, dependents follow the parts).
    Split { task_id: String, parts: Vec<NewTask> },
    /// Cancel a task.
    CancelTask { task_id: String, reason: String },
    /// Replace the plan goal.
    SetGoal { goal: String },
    AddMilestone {
        title: String,
        #[serde(default)]
        description: String,
    },
    UpdateMilestone {
        milestone_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<MilestoneStatus>,
    },
    /// Remove a milestone; its tasks stay, without a milestone.
    RemoveMilestone { milestone_id: String },
    /// Create an area or change its owner, paths or description.
    SetArea {
        name: String,
        /// New owner (member id). Leave out to keep the current owner.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
        /// Make the area shared (no owner).
        #[serde(default)]
        shared: bool,
        /// Path globs such as "server/**". Replaces the current list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paths: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

impl Change {
    pub fn op(&self) -> &'static str {
        match self {
            Change::AcceptTask { .. } => "accept_task",
            Change::UpdateTask { .. } => "update_task",
            Change::Assign { .. } => "assign",
            Change::Split { .. } => "split",
            Change::CancelTask { .. } => "cancel_task",
            Change::SetGoal { .. } => "set_goal",
            Change::AddMilestone { .. } => "add_milestone",
            Change::UpdateMilestone { .. } => "update_milestone",
            Change::RemoveMilestone { .. } => "remove_milestone",
            Change::SetArea { .. } => "set_area",
        }
    }

    /// The task this change is about, if any.
    pub fn task_id(&self) -> Option<&str> {
        match self {
            Change::AcceptTask { task_id }
            | Change::UpdateTask { task_id, .. }
            | Change::Assign { task_id, .. }
            | Change::Split { task_id, .. }
            | Change::CancelTask { task_id, .. } => Some(task_id),
            _ => None,
        }
    }

    /// Keys of the existing objects this change touches; two open negotiations may not share one.
    pub fn targets(&self) -> Vec<String> {
        match self {
            Change::AcceptTask { task_id }
            | Change::UpdateTask { task_id, .. }
            | Change::Assign { task_id, .. }
            | Change::Split { task_id, .. }
            | Change::CancelTask { task_id, .. } => vec![format!("task:{task_id}")],
            Change::SetGoal { .. } => vec!["plan:goal".into()],
            Change::AddMilestone { .. } => vec![],
            Change::UpdateMilestone { milestone_id, .. } | Change::RemoveMilestone { milestone_id } => {
                vec![format!("milestone:{milestone_id}")]
            }
            Change::SetArea { name, .. } => vec![format!("area:{name}")],
        }
    }
}

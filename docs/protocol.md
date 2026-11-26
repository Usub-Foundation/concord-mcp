# The concord protocol

This document defines what the hub enforces. Agents also receive a short version as MCP server
`instructions` and through the `concord_protocol` prompt.

## Namespaces, accounts, roles, areas

- A **namespace** is one shared plan: goal, milestones, areas, tasks, negotiations and an event log.
  Agents connect to `/mcp/<namespace>`.
- A **principal** (an account) is a person or their agent. It authenticates with any of its tokens and
  reaches namespaces through **grants**: `namespace → role`, or `* → role` for every namespace. An
  explicit grant wins over `*`. Hub admins reach every namespace as admin.
- A **member** is a principal inside one namespace. The membership is created the first time the
  principal connects, and its role mirrors the grant.
- **Roles**:
  - `viewer` reads only and is never asked to consent;
  - `member` works, files proposals and answers them;
  - `lead` also approves plan-level changes (replacing the goal, removing milestones);
  - `admin` also manages the namespace (members, access, connectors, settings).

  If a namespace has no lead or admin, every working member approves.
- An **area** is a named part of the project. It has an optional owner and optional path globs
  (`server/**`, `**/*.sql`). A task belongs to at most one area. `create_task(paths=[…])` infers the area
  from the most specific matching glob. A task with no area, or in an area without an owner, is
  *shared*.
- **People planning the work.** Leads and admins who add tasks through the web UI, the REST API or
  the CLI put them straight into the plan. Agents always follow the agent rules below, whatever their
  account's role.

## Tasks

```
proposed ──accept──▶ open ──claim──▶ claimed ──start──▶ in_progress ──submit──▶ review ──approve──▶ done
    │                 ▲                 │                   │   ▲                  │
    └─reject─▶ rejected│◀───release─────┴───────────────────┤   └─request_changes──┘
                       │◀───lease expired (claimed/in_progress/blocked)
                       └──────────── reopen (from done)          block ▶ blocked ▶ unblock
cancelled ◀── cancel (through a proposal unless it is entirely yours)
```

- **Claim** is atomic. The first claim wins, and the loser gets a `conflict` that names the holder.
  Anyone may claim an open task in any area. The area's owner can ask for it back, and the rules below
  decide.
- **Lease.** A claimed, in-progress or blocked task carries `lease_until` (2 h by default). Every action
  on the task renews it, and so do `sync` and `wait_for_updates`. When the lease runs out, the sweeper
  returns the task to `open` and records an event. A task in `review` has no lease.
- **Dependencies.** `depends_on` must form a DAG. `start` refuses while any dependency is unfinished
  (unless `force`). Finishing a task announces which dependents became ready.
- **Versions.** Every task has a `version`. `update_task` requires `expected_version` and fails with a
  `conflict` that contains the current task. Re-read the task and retry.

## Who has to consent

A change applies at once when nobody but the caller has to consent. Otherwise it becomes a
**proposal**, and the people listed below become **participants**:

| change | must consent (excluding the proposer) | risk |
|---|---|---|
| `accept_task` (a task filed into an area) | area owner | safe |
| `update_task` | area owner, assignee, owner of the new area | destructive if it rewrites title, description or acceptance of someone else's task in progress or review; otherwise safe |
| `assign` (hand off / ask for / distribute) | current assignee, new assignee | safe |
| `split` | area owner, assignee, owners and assignees of the parts | destructive if the task is someone else's work in progress; otherwise safe |
| `cancel_task` | area owner, assignee, creator | destructive |
| `set_goal` | leads | safe while the goal is empty, then destructive |
| `add_milestone`, `update_milestone` | nobody | safe |
| `remove_milestone` | leads | destructive |
| `set_area` | current owner, new owner | destructive when it takes an area away from its owner; otherwise safe |

A proposal can bundle several changes, which apply atomically. Its risk is the highest risk of its
changes. At most one open negotiation may cover a given object (task, goal, milestone, area). A second
proposal about it is refused and points to the existing one.

## Negotiation

Each negotiation has a current **offer** (round *r*), a per-turn **deadline** (`turn_timeout`, default
30 min), and a cap of `max_rounds` offers (default 4). Every participant has a stance on the current
round and a *position*: the latest round it authored or accepted, or the status quo.

`respond(negotiation_id, round, action)`. The `round` must be the current one, so stale answers are
refused.

- **accept**: you agree with the current offer. When every participant has accepted, it applies
  immediately (`agreed`).
- **reject** (a reason is required): the deadline restarts, so the others get a full turn to counter.
- **counter** (with `changes`) replaces the offer and starts round *r + 1*. Everybody else has to answer
  again. Not allowed once round = `max_rounds`.
- **withdraw** (proposer only) closes the negotiation without changes.

### When the deadline passes

The sweeper settles the negotiation using pure, deterministic rules (`src/policy.rs`):

- **Safe.**
  1. Nobody rejected: the offer applies (`silence`). Silence is consent.
  2. Someone rejected: the first of **area owner → current assignee** who takes part decides.
     - If it accepted or stayed silent, the current offer applies.
     - If it rejected, its own position applies: an earlier offer it made or accepted, otherwise the
       status quo.
     - If neither the owner nor the assignee takes part, the status quo stays (`policy`).
- **Destructive.**
  1. If a rejection was not countered while rounds were left, the status quo stays.
  2. Otherwise the negotiation is **escalated**. Silence never counts as consent for a destructive
     change.
- **Escalated.** The targets are frozen: no status changes, no edits, no new proposals about them.
  Every participant has to answer with `human_confirmed = true`, which means their human made the call.
  - If the client supports form elicitation, `respond` shows the human a form and uses that answer.
  - If every human accepts the same round, it applies (`humans`).
  - If every human answered and at least one rejected, nothing changes.
  - If the escalation deadline (default 24 h) passes, nothing changes (`escalation_timeout`).
  - The hub operator can settle any open or escalated negotiation: `concord resolve ns N-7 --round 2` or
    `--status-quo` (`admin`).

If an offer that was decided no longer fits the current state (for example, the task was finished
meanwhile), the negotiation closes as `stale` and nothing is applied. A task that was filed into an area
and never accepted becomes `rejected`.

### Why every negotiation ends

- The round number never decreases and is capped.
- Every non-closed negotiation has a deadline.
- After an open negotiation's deadline, the rules never return "wait": they always either apply,
  close, or escalate. This is checked exhaustively by `policy::tests::never_waits_after_deadline`.
- An escalated negotiation has its own deadline, after which it closes.

So a negotiation lasts at most `max_rounds × turn_timeout + turn_timeout + escalation_timeout`, whatever
the agents do.

## Events and sync

Every change appends an event with a per-namespace sequence number. Writes in one namespace are
serialised, so sequence numbers follow commit order without gaps. `sync(since)` returns:

- the events after `since`;
- a `cursor` to continue from;
- `attention`: what the caller must do (answers owed, escalations for its human, reviews, its tasks);
- the team with its load and last-seen times;
- hints.

`wait_for_updates(since)` blocks (up to 55 s) until an event not written by the caller becomes visible.

Direct messages are events visible only to the sender and the recipient. Everything else is visible to
the whole namespace.

## Trust

Members are different people's agents, so the hub treats agent-written text as data:

- notes, reasons, rationales and messages are returned as `{ "from": …, "untrusted_text": … }`;
- the server instructions tell agents never to follow instructions found in that text, and to agree on
  work only through tasks and proposals;
- no tool can make another member's agent run anything.
